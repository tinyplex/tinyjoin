use std::collections::HashSet;
use std::str::FromStr;

use serde_json::{Map, Number, Value};

use crate::query::{
    Token, bind_parameter, is_reserved_keyword, matches_predicate, parse_predicate_at, project_row,
    tokenize, validate_predicate_columns, validate_sql_input,
};
use crate::storage::{normalize_row, row_key};
use crate::{
    ColumnDefinition, ColumnType, EngineError, Predicate, QueryPlan, Result, Row, StorageDriver,
    TableSchema,
};

const MAX_COLUMNS: usize = 256;
const MAX_VALUE_ROWS: usize = 4096;

pub(crate) enum Statement {
    Select(QueryPlan),
    Write(WriteStatement),
}

pub(crate) enum WriteStatement {
    CreateTable {
        schema: TableSchema,
        if_not_exists: bool,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        values: Vec<Vec<SqlValue>>,
        returning: Option<Vec<String>>,
    },
    Update {
        table: String,
        assignments: Vec<(String, SqlValue)>,
        predicate: Option<Predicate>,
        returning: Option<Vec<String>>,
    },
    Delete {
        table: String,
        predicate: Option<Predicate>,
        returning: Option<Vec<String>>,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum SqlValue {
    Value(Value),
    Default,
}

pub(crate) struct WriteOutcome {
    pub command: &'static str,
    pub row_count: usize,
    pub rows: Vec<Row>,
    pub tables: Vec<String>,
    pub mutated: bool,
}

pub(crate) fn parse(sql: &str, params: &[Value]) -> Result<Statement> {
    validate_sql_input(sql, params)?;
    let tokens = tokenize(sql)?;
    if matches!(
        tokens.first(),
        Some(Token::Identifier {
            value,
            quoted: false,
        }) if value.eq_ignore_ascii_case("select")
    ) {
        return crate::query::parse_sql(sql, params).map(Statement::Select);
    }
    MutationParser::new(tokens, params)
        .parse()
        .map(Statement::Write)
}

pub(crate) fn execute<S: StorageDriver>(
    storage: &mut S,
    statement: &WriteStatement,
) -> Result<WriteOutcome> {
    match statement {
        WriteStatement::CreateTable {
            schema,
            if_not_exists,
        } => create_table(storage, schema, *if_not_exists),
        WriteStatement::Insert {
            table,
            columns,
            values,
            returning,
        } => insert(
            storage,
            table,
            columns.as_deref(),
            values,
            returning.as_deref(),
        ),
        WriteStatement::Update {
            table,
            assignments,
            predicate,
            returning,
        } => update(
            storage,
            table,
            assignments,
            predicate.as_ref(),
            returning.as_deref(),
        ),
        WriteStatement::Delete {
            table,
            predicate,
            returning,
        } => delete(storage, table, predicate.as_ref(), returning.as_deref()),
    }
}

fn create_table<S: StorageDriver>(
    storage: &mut S,
    schema: &TableSchema,
    if_not_exists: bool,
) -> Result<WriteOutcome> {
    if storage.table_schema(&schema.name).is_ok() {
        if if_not_exists {
            return Ok(WriteOutcome {
                command: "CREATE TABLE",
                row_count: 0,
                rows: vec![],
                tables: vec![],
                mutated: false,
            });
        }
        return Err(EngineError::table_already_exists(&schema.name));
    }

    storage.define_table(schema.clone())?;
    Ok(WriteOutcome {
        command: "CREATE TABLE",
        row_count: 0,
        rows: vec![],
        tables: vec![schema.name.clone()],
        mutated: true,
    })
}

fn insert<S: StorageDriver>(
    storage: &mut S,
    table: &str,
    columns: Option<&[String]>,
    value_rows: &[Vec<SqlValue>],
    returning: Option<&[String]>,
) -> Result<WriteOutcome> {
    let schema = storage.table_schema(table)?;
    let default_values =
        columns.is_none() && value_rows.len() == 1 && value_rows.first().is_some_and(Vec::is_empty);
    let columns = match columns {
        Some(columns) => columns.to_vec(),
        None if !schema.columns.is_empty() => schema
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect(),
        None => {
            return Err(EngineError::invalid_query(format!(
                "INSERT into untyped table `{table}` must include a column list"
            )));
        }
    };
    validate_named_columns(&schema, &columns)?;
    validate_projection(&schema, returning)?;

    let mut rows = storage.scan_table(table)?;
    let mut keys = HashSet::with_capacity(rows.len() + value_rows.len());
    for row in &rows {
        keys.insert(row_key(&schema, row)?);
    }

    let mut inserted = Vec::with_capacity(value_rows.len());
    for values in value_rows {
        if !default_values && values.len() != columns.len() {
            return Err(EngineError::invalid_query(format!(
                "INSERT names {} columns but provides {} values",
                columns.len(),
                values.len()
            )));
        }
        let mut row = Map::new();
        if !default_values {
            for (column, value) in columns.iter().zip(values) {
                if let SqlValue::Value(value) = value {
                    row.insert(column.clone(), value.clone());
                }
            }
        }
        let row = normalize_row(&schema, row)?;
        let key = row_key(&schema, &row)?;
        if !keys.insert(key) {
            return Err(EngineError::constraint_violation(format!(
                "INSERT into `{table}` would duplicate a primary key"
            )));
        }
        inserted.push(row.clone());
        rows.push(row);
    }

    let row_count = inserted.len();
    let returned = project_rows(inserted, returning, table)?;
    storage.replace_table_unrevisioned(table, rows)?;
    Ok(WriteOutcome {
        command: "INSERT",
        row_count,
        rows: returned,
        tables: vec![table.to_owned()],
        mutated: true,
    })
}

fn update<S: StorageDriver>(
    storage: &mut S,
    table: &str,
    assignments: &[(String, SqlValue)],
    predicate: Option<&Predicate>,
    returning: Option<&[String]>,
) -> Result<WriteOutcome> {
    let schema = storage.table_schema(table)?;
    validate_named_columns(
        &schema,
        &assignments
            .iter()
            .map(|(column, _)| column.clone())
            .collect::<Vec<_>>(),
    )?;
    if let Some(predicate) = predicate.filter(|_| !schema.columns.is_empty()) {
        validate_predicate_columns(predicate, &schema, table)?;
    }
    validate_projection(&schema, returning)?;

    let mut rows = storage.scan_table(table)?;
    let mut updated = Vec::new();
    for row in &mut rows {
        if !matches_predicate(row, predicate, table)? {
            continue;
        }
        for (column, value) in assignments {
            let value = match value {
                SqlValue::Value(value) => value.clone(),
                SqlValue::Default => column_default(&schema, column)?,
            };
            row.insert(column.clone(), value);
        }
        *row = normalize_row(&schema, std::mem::take(row))?;
        updated.push(row.clone());
    }

    let mut keys = HashSet::with_capacity(rows.len());
    for row in &rows {
        if !keys.insert(row_key(&schema, row)?) {
            return Err(EngineError::constraint_violation(format!(
                "UPDATE of `{table}` would duplicate a primary key"
            )));
        }
    }

    let row_count = updated.len();
    let returned = project_rows(updated, returning, table)?;
    if row_count > 0 {
        storage.replace_table_unrevisioned(table, rows)?;
    }
    Ok(WriteOutcome {
        command: "UPDATE",
        row_count,
        rows: returned,
        tables: (row_count > 0)
            .then(|| table.to_owned())
            .into_iter()
            .collect(),
        mutated: row_count > 0,
    })
}

fn delete<S: StorageDriver>(
    storage: &mut S,
    table: &str,
    predicate: Option<&Predicate>,
    returning: Option<&[String]>,
) -> Result<WriteOutcome> {
    let schema = storage.table_schema(table)?;
    if let Some(predicate) = predicate.filter(|_| !schema.columns.is_empty()) {
        validate_predicate_columns(predicate, &schema, table)?;
    }
    validate_projection(&schema, returning)?;

    let mut kept = Vec::new();
    let mut deleted = Vec::new();
    for row in storage.scan_table(table)? {
        if matches_predicate(&row, predicate, table)? {
            deleted.push(row);
        } else {
            kept.push(row);
        }
    }
    let row_count = deleted.len();
    let returned = project_rows(deleted, returning, table)?;
    if row_count > 0 {
        storage.replace_table_unrevisioned(table, kept)?;
    }
    Ok(WriteOutcome {
        command: "DELETE",
        row_count,
        rows: returned,
        tables: (row_count > 0)
            .then(|| table.to_owned())
            .into_iter()
            .collect(),
        mutated: row_count > 0,
    })
}

fn validate_named_columns(schema: &TableSchema, columns: &[String]) -> Result<()> {
    let mut names = HashSet::with_capacity(columns.len());
    for column in columns {
        if !names.insert(column) {
            return Err(EngineError::invalid_query(format!(
                "Column `{column}` is named more than once"
            )));
        }
        if !schema.columns.is_empty()
            && !schema
                .columns
                .iter()
                .any(|definition| definition.name == *column)
        {
            return Err(EngineError::column_not_found(column, &schema.name));
        }
    }
    Ok(())
}

fn validate_projection(schema: &TableSchema, returning: Option<&[String]>) -> Result<()> {
    if schema.columns.is_empty() {
        return Ok(());
    }
    if let Some(columns) = returning {
        for column in columns {
            if !schema
                .columns
                .iter()
                .any(|definition| definition.name == *column)
            {
                return Err(EngineError::column_not_found(column, &schema.name));
            }
        }
    }
    Ok(())
}

fn column_default(schema: &TableSchema, name: &str) -> Result<Value> {
    if schema.columns.is_empty() {
        return Err(EngineError::unsupported_sql(
            "DEFAULT requires a typed table catalog",
        ));
    }
    schema
        .columns
        .iter()
        .find(|column| column.name == name)
        .map(|column| column.default.clone().unwrap_or(Value::Null))
        .ok_or_else(|| EngineError::column_not_found(name, &schema.name))
}

fn project_rows(rows: Vec<Row>, returning: Option<&[String]>, table: &str) -> Result<Vec<Row>> {
    let Some(columns) = returning else {
        return Ok(vec![]);
    };
    let projection = if columns.is_empty() {
        None
    } else {
        Some(columns)
    };
    rows.into_iter()
        .map(|row| project_row(row, projection, table))
        .collect()
}

struct MutationParser<'a> {
    tokens: Vec<Token>,
    position: usize,
    params: &'a [Value],
}

impl<'a> MutationParser<'a> {
    fn new(tokens: Vec<Token>, params: &'a [Value]) -> Self {
        Self {
            tokens,
            position: 0,
            params,
        }
    }

    fn parse(mut self) -> Result<WriteStatement> {
        let statement = if self.consume_keyword("create") {
            self.parse_create_table()?
        } else if self.consume_keyword("insert") {
            self.parse_insert()?
        } else if self.consume_keyword("update") {
            self.parse_update()?
        } else if self.consume_keyword("delete") {
            self.parse_delete()?
        } else {
            return Err(unsupported_statement());
        };
        self.consume(TokenMatcher::Semicolon);
        if !self.is_done() {
            return Err(unsupported_statement());
        }
        Ok(statement)
    }

    fn parse_create_table(&mut self) -> Result<WriteStatement> {
        self.expect_keyword("table")?;
        let if_not_exists = if self.consume_keyword("if") {
            self.expect_keyword("not")?;
            self.expect_keyword("exists")?;
            true
        } else {
            false
        };
        let name = self.parse_table_name()?;
        self.expect(TokenMatcher::LParen, "Expected `(` after table name")?;

        let mut columns = Vec::new();
        let mut inline_primary_key = None;
        let mut table_primary_key = None;
        loop {
            if self.consume_keyword("primary") {
                self.expect_keyword("key")?;
                if table_primary_key.is_some() || inline_primary_key.is_some() {
                    return Err(EngineError::invalid_schema(
                        "A table can declare only one primary key",
                    ));
                }
                self.expect(TokenMatcher::LParen, "Expected `(` after PRIMARY KEY")?;
                table_primary_key = Some(self.parse_identifier_list(TokenMatcher::RParen)?);
                self.expect(
                    TokenMatcher::RParen,
                    "Expected `)` after PRIMARY KEY columns",
                )?;
            } else {
                if columns.len() >= MAX_COLUMNS {
                    return Err(EngineError::invalid_schema(format!(
                        "A table cannot contain more than {MAX_COLUMNS} columns"
                    )));
                }
                let (column, primary_key) = self.parse_column_definition()?;
                if primary_key {
                    if inline_primary_key.is_some() || table_primary_key.is_some() {
                        return Err(EngineError::invalid_schema(
                            "A table can declare only one primary key",
                        ));
                    }
                    inline_primary_key = Some(column.name.clone());
                }
                columns.push(column);
            }

            if !self.consume(TokenMatcher::Comma) {
                break;
            }
            if self.peek_matches(TokenMatcher::RParen) {
                return Err(EngineError::parse_error(
                    "A trailing comma is not supported in CREATE TABLE",
                ));
            }
        }
        self.expect(TokenMatcher::RParen, "Expected `)` after table definition")?;

        let primary_key = table_primary_key
            .or_else(|| inline_primary_key.map(|column| vec![column]))
            .ok_or_else(|| {
                EngineError::invalid_schema(format!("Table `{name}` must declare a PRIMARY KEY"))
            })?;
        for column in &mut columns {
            if primary_key.contains(&column.name) {
                column.nullable = false;
            }
        }
        Ok(WriteStatement::CreateTable {
            schema: TableSchema {
                name,
                primary_key,
                columns,
            },
            if_not_exists,
        })
    }

    fn parse_column_definition(&mut self) -> Result<(ColumnDefinition, bool)> {
        let name = self.parse_identifier()?;
        let data_type = self.parse_column_type()?;
        let mut nullable = true;
        let mut default = None;
        let mut primary_key = false;

        loop {
            if self.consume_keyword("primary") {
                self.expect_keyword("key")?;
                primary_key = true;
                nullable = false;
            } else if self.consume_keyword("not") {
                self.expect_keyword("null")?;
                nullable = false;
            } else if self.consume_keyword("null") {
                nullable = true;
            } else if self.consume_keyword("default") {
                if default.is_some() {
                    return Err(EngineError::invalid_schema(format!(
                        "Column `{name}` declares DEFAULT more than once"
                    )));
                }
                default = Some(self.parse_literal()?);
            } else {
                break;
            }
        }
        Ok((
            ColumnDefinition {
                name,
                data_type,
                nullable,
                default,
            },
            primary_key,
        ))
    }

    fn parse_column_type(&mut self) -> Result<ColumnType> {
        let Some(Token::Identifier {
            value,
            quoted: false,
        }) = self.next()
        else {
            return Err(EngineError::parse_error("Expected a column type"));
        };
        let value = value.to_ascii_lowercase();
        let data_type = match value.as_str() {
            "boolean" | "bool" => ColumnType::Boolean,
            "smallint" | "integer" | "int" | "int2" | "int4" | "bigint" | "int8" => {
                ColumnType::Integer
            }
            "real" | "float" | "float4" | "float8" => ColumnType::Float,
            "double" => {
                self.expect_keyword("precision")?;
                ColumnType::Float
            }
            "text" | "varchar" => ColumnType::Text,
            "character" => {
                self.expect_keyword("varying")?;
                ColumnType::Text
            }
            "json" | "jsonb" => ColumnType::Json,
            _ => {
                return Err(EngineError::unsupported_sql(format!(
                    "Column type `{value}` is not supported; use boolean, integer, float, text, or json"
                )));
            }
        };
        if self.peek_matches(TokenMatcher::LParen) {
            return Err(EngineError::unsupported_sql(
                "Type modifiers such as VARCHAR(100) are not supported",
            ));
        }
        Ok(data_type)
    }

    fn parse_insert(&mut self) -> Result<WriteStatement> {
        self.expect_keyword("into")?;
        let table = self.parse_table_name()?;
        let columns = if self.consume(TokenMatcher::LParen) {
            let columns = self.parse_identifier_list(TokenMatcher::RParen)?;
            self.expect(TokenMatcher::RParen, "Expected `)` after INSERT columns")?;
            Some(columns)
        } else {
            None
        };

        let values = if self.consume_keyword("default") {
            self.expect_keyword("values")?;
            vec![vec![]]
        } else {
            if self.consume_keyword("select") {
                return Err(EngineError::unsupported_sql(
                    "INSERT ... SELECT is not supported",
                ));
            }
            self.expect_keyword("values")?;
            let mut rows = Vec::new();
            loop {
                if rows.len() >= MAX_VALUE_ROWS {
                    return Err(EngineError::invalid_query(format!(
                        "INSERT cannot contain more than {MAX_VALUE_ROWS} rows"
                    )));
                }
                self.expect(TokenMatcher::LParen, "Expected `(` before INSERT values")?;
                let mut values = Vec::new();
                if !self.peek_matches(TokenMatcher::RParen) {
                    loop {
                        values.push(self.parse_sql_value(true)?);
                        if !self.consume(TokenMatcher::Comma) {
                            break;
                        }
                    }
                }
                self.expect(TokenMatcher::RParen, "Expected `)` after INSERT values")?;
                rows.push(values);
                if !self.consume(TokenMatcher::Comma) {
                    break;
                }
            }
            rows
        };
        let returning = self.parse_returning()?;
        Ok(WriteStatement::Insert {
            table,
            columns,
            values,
            returning,
        })
    }

    fn parse_update(&mut self) -> Result<WriteStatement> {
        let table = self.parse_table_name()?;
        self.expect_keyword("set")?;
        let mut assignments = Vec::new();
        loop {
            if assignments.len() >= MAX_COLUMNS {
                return Err(EngineError::invalid_query(format!(
                    "UPDATE cannot assign more than {MAX_COLUMNS} columns"
                )));
            }
            let column = self.parse_identifier()?;
            self.expect(TokenMatcher::Eq, "Expected `=` in UPDATE assignment")?;
            assignments.push((column, self.parse_sql_value(true)?));
            if !self.consume(TokenMatcher::Comma) {
                break;
            }
        }
        let predicate = if self.consume_keyword("where") {
            Some(parse_predicate_at(
                &self.tokens,
                &mut self.position,
                self.params,
            )?)
        } else {
            None
        };
        let returning = self.parse_returning()?;
        Ok(WriteStatement::Update {
            table,
            assignments,
            predicate,
            returning,
        })
    }

    fn parse_delete(&mut self) -> Result<WriteStatement> {
        self.expect_keyword("from")?;
        let table = self.parse_table_name()?;
        let predicate = if self.consume_keyword("where") {
            Some(parse_predicate_at(
                &self.tokens,
                &mut self.position,
                self.params,
            )?)
        } else {
            None
        };
        let returning = self.parse_returning()?;
        Ok(WriteStatement::Delete {
            table,
            predicate,
            returning,
        })
    }

    /// `None` means no RETURNING clause. An empty vector means RETURNING `*`.
    fn parse_returning(&mut self) -> Result<Option<Vec<String>>> {
        if !self.consume_keyword("returning") {
            return Ok(None);
        }
        if self.consume(TokenMatcher::Star) {
            return Ok(Some(vec![]));
        }
        Ok(Some(self.parse_identifier_list(TokenMatcher::Semicolon)?))
    }

    fn parse_identifier_list(&mut self, terminator: TokenMatcher) -> Result<Vec<String>> {
        let mut values = Vec::new();
        loop {
            if values.len() >= MAX_COLUMNS {
                return Err(EngineError::invalid_query(format!(
                    "A column list cannot contain more than {MAX_COLUMNS} columns"
                )));
            }
            values.push(self.parse_identifier()?);
            if !self.consume(TokenMatcher::Comma) {
                break;
            }
            if self.peek_matches(terminator) {
                return Err(EngineError::parse_error(
                    "A column list cannot end with a comma",
                ));
            }
        }
        Ok(values)
    }

    fn parse_table_name(&mut self) -> Result<String> {
        let first = self.parse_identifier()?;
        if !self.consume(TokenMatcher::Dot) {
            return Ok(first);
        }
        let second = self.parse_identifier()?;
        if self.peek_matches(TokenMatcher::Dot) {
            return Err(EngineError::unsupported_sql(
                "Only unqualified or schema-qualified table names are supported",
            ));
        }
        Ok(format!("{first}.{second}"))
    }

    fn parse_literal(&mut self) -> Result<Value> {
        let SqlValue::Value(value) = self.parse_sql_value(false)? else {
            unreachable!("DEFAULT was disabled for a column default")
        };
        Ok(value)
    }

    fn parse_sql_value(&mut self, allow_default: bool) -> Result<SqlValue> {
        let Some(token) = self.next() else {
            return Err(EngineError::parse_error("Expected a SQL value"));
        };
        match token {
            Token::String(value) => Ok(SqlValue::Value(Value::String(value))),
            Token::Number(value) => Number::from_str(&value)
                .map(Value::Number)
                .map(SqlValue::Value)
                .map_err(|_| EngineError::invalid_query(format!("Invalid number `{value}`"))),
            Token::Placeholder(index) => bind_parameter(&index, self.params).map(SqlValue::Value),
            Token::Identifier {
                value,
                quoted: false,
            } if value.eq_ignore_ascii_case("null") => Ok(SqlValue::Value(Value::Null)),
            Token::Identifier {
                value,
                quoted: false,
            } if value.eq_ignore_ascii_case("true") => Ok(SqlValue::Value(Value::Bool(true))),
            Token::Identifier {
                value,
                quoted: false,
            } if value.eq_ignore_ascii_case("false") => Ok(SqlValue::Value(Value::Bool(false))),
            Token::Identifier {
                value,
                quoted: false,
            } if allow_default && value.eq_ignore_ascii_case("default") => Ok(SqlValue::Default),
            _ => Err(unsupported_expression()),
        }
    }

    fn parse_identifier(&mut self) -> Result<String> {
        let Some(Token::Identifier { value, quoted }) = self.next() else {
            return Err(EngineError::parse_error("Expected a SQL identifier"));
        };
        if value.is_empty() {
            return Err(EngineError::parse_error(
                "A quoted SQL identifier cannot be empty",
            ));
        }
        if !quoted && is_reserved_keyword(&value) {
            return Err(EngineError::parse_error(format!(
                "Reserved keyword `{value}` cannot be used as an unquoted SQL identifier"
            )));
        }
        Ok(if quoted {
            value
        } else {
            value.to_ascii_lowercase()
        })
    }

    fn expect_keyword(&mut self, keyword: &str) -> Result<()> {
        if self.consume_keyword(keyword) {
            Ok(())
        } else {
            Err(EngineError::parse_error(format!(
                "Expected keyword `{keyword}`"
            )))
        }
    }

    fn consume_keyword(&mut self, keyword: &str) -> bool {
        if matches!(
            self.tokens.get(self.position),
            Some(Token::Identifier {
                value,
                quoted: false,
            }) if value.eq_ignore_ascii_case(keyword)
        ) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, matcher: TokenMatcher, message: &str) -> Result<()> {
        if self.consume(matcher) {
            Ok(())
        } else {
            Err(EngineError::parse_error(message))
        }
    }

    fn consume(&mut self, matcher: TokenMatcher) -> bool {
        if self.peek_matches(matcher) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn peek_matches(&self, matcher: TokenMatcher) -> bool {
        matches!(
            (self.tokens.get(self.position), matcher),
            (Some(Token::Star), TokenMatcher::Star)
                | (Some(Token::Comma), TokenMatcher::Comma)
                | (Some(Token::Dot), TokenMatcher::Dot)
                | (Some(Token::Eq), TokenMatcher::Eq)
                | (Some(Token::LParen), TokenMatcher::LParen)
                | (Some(Token::RParen), TokenMatcher::RParen)
                | (Some(Token::Semicolon), TokenMatcher::Semicolon)
        )
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.position)?.clone();
        self.position += 1;
        Some(token)
    }

    fn is_done(&self) -> bool {
        self.position == self.tokens.len()
    }
}

#[derive(Clone, Copy)]
enum TokenMatcher {
    Star,
    Comma,
    Dot,
    Eq,
    LParen,
    RParen,
    Semicolon,
}

fn unsupported_statement() -> EngineError {
    EngineError::unsupported_sql(
        "Supported statements are SELECT, CREATE TABLE, INSERT, UPDATE, and DELETE",
    )
}

fn unsupported_expression() -> EngineError {
    EngineError::unsupported_sql(
        "This SQL subset supports literals, parameters, NULL, booleans, and DEFAULT values",
    )
}
