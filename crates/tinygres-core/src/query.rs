use std::cmp::Ordering;
use std::str::FromStr;

use serde_json::{Map, Number, Value};

use crate::storage::{
    StorageReader, estimated_row_bytes, estimated_value_bytes, validate_json_value,
};
use crate::{
    ColumnDefinition, ColumnType, EngineError, Filter, FilterOperator, NullOrder, OrderBy,
    OrderDirection, Predicate, QueryPlan, QueryResult, Result, Row, VisitControl, VisitOutcome,
};

const MAX_SQL_BYTES: usize = 64 * 1024;
const MAX_SQL_TOKENS: usize = 4 * 1024;
const MAX_PROJECTION_COLUMNS: usize = 256;
const MAX_FILTERS: usize = 256;
const MAX_PREDICATE_DEPTH: usize = 32;
const MAX_IN_VALUES: usize = 1024;
const MAX_ORDER_COLUMNS: usize = 32;
const MAX_PARAMETERS: usize = 1024;
const MAX_COMMENT_DEPTH: usize = 32;
const MAX_RESULT_ROWS: usize = 100_000;
const MAX_QUERY_RESULT_BYTES: usize = 16 * 1024 * 1024;
const MAX_SCAN_ROWS: usize = 1_000_000;
const MAX_ORDERED_ROWS: usize = 100_000;

pub(crate) fn execute<S: StorageReader>(storage: &S, plan: &QueryPlan) -> Result<QueryResult> {
    if plan.table.trim().is_empty() {
        return Err(EngineError::invalid_query(
            "A query must name exactly one table",
        ));
    }
    if plan.columns.as_ref().is_some_and(Vec::is_empty) {
        return Err(EngineError::invalid_query(
            "A projection must contain at least one column",
        ));
    }
    if plan
        .columns
        .as_ref()
        .is_some_and(|columns| columns.len() > MAX_PROJECTION_COLUMNS)
    {
        return Err(EngineError::invalid_query(format!(
            "A projection cannot contain more than {MAX_PROJECTION_COLUMNS} columns"
        )));
    }
    if plan.filters.len() > MAX_FILTERS {
        return Err(EngineError::invalid_query(format!(
            "A query cannot contain more than {MAX_FILTERS} filters"
        )));
    }
    if plan.order_by.len() > MAX_ORDER_COLUMNS {
        return Err(EngineError::invalid_query(format!(
            "A query cannot order by more than {MAX_ORDER_COLUMNS} columns"
        )));
    }
    validate_predicate_complexity(plan.predicate.as_ref())?;

    if plan.limit.is_some_and(|limit| limit > MAX_RESULT_ROWS) {
        return Err(result_limit_exceeded());
    }
    if let Some(limit) = plan.limit {
        plan.offset.checked_add(limit).ok_or_else(|| {
            EngineError::invalid_query("Query OFFSET plus LIMIT exceeds the supported range")
        })?;
    }

    let schema = storage.table_schema(&plan.table)?;
    if !schema.columns.is_empty() {
        if let Some(columns) = &plan.columns {
            for column in columns {
                if !schema
                    .columns
                    .iter()
                    .any(|definition| definition.name == *column)
                {
                    return Err(EngineError::column_not_found(column, &plan.table));
                }
            }
        }
        for filter in &plan.filters {
            let definition = column_definition(&schema, &filter.column, &plan.table)?;
            validate_comparison_value(definition, filter.operator, &filter.value, &plan.table)?;
        }
        if let Some(predicate) = &plan.predicate {
            validate_predicate_columns(predicate, &schema, &plan.table)?;
            validate_predicate_types(predicate, &schema, &plan.table)?;
        }
        for order in &plan.order_by {
            let definition = column_definition(&schema, &order.column, &plan.table)?;
            if definition.data_type == ColumnType::Json {
                return Err(EngineError::type_mismatch(format!(
                    "JSON column `{}` in `{}` cannot be ordered",
                    order.column, plan.table
                )));
            }
        }
    }

    if plan.limit == Some(0) {
        return Ok(QueryResult {
            revision: storage.revision(),
            rows: Vec::new(),
        });
    }

    let rows = if plan.order_by.is_empty() {
        execute_unordered(storage, plan, &schema)?
    } else {
        execute_ordered(storage, plan, &schema)?
    };

    Ok(QueryResult {
        revision: storage.revision(),
        rows,
    })
}

fn execute_unordered<S: StorageReader>(
    storage: &S,
    plan: &QueryPlan,
    schema: &crate::TableSchema,
) -> Result<Vec<Row>> {
    let mut scanned = 0_usize;
    let mut skipped_matches = 0_usize;
    let mut result_bytes = 0_usize;
    let mut rows = Vec::new();
    visit_candidate_rows(storage, plan, schema, &mut |row| {
        count_scanned_row(&mut scanned)?;
        if !matches_filters(row, &plan.filters, &plan.table)?
            || !matches_predicate(row, plan.predicate.as_ref(), &plan.table)?
        {
            return Ok(VisitControl::Continue);
        }
        if skipped_matches < plan.offset {
            skipped_matches += 1;
            return Ok(VisitControl::Continue);
        }
        if rows.len() == MAX_RESULT_ROWS {
            return Err(result_limit_exceeded());
        }
        let projected_bytes = projected_row_bytes(row, plan.columns.as_deref(), &plan.table)?;
        let next_result_bytes = checked_result_add(result_bytes, projected_bytes)?;
        ensure_result_budget(next_result_bytes)?;
        let clone_peak = checked_result_add(result_bytes, owned_row_bytes(row)?)?;
        ensure_result_budget(clone_peak)?;
        rows.push(project_row(
            row.clone(),
            plan.columns.as_deref(),
            &plan.table,
        )?);
        result_bytes = next_result_bytes;
        if plan.limit.is_some_and(|limit| rows.len() == limit) {
            Ok(VisitControl::Stop)
        } else {
            Ok(VisitControl::Continue)
        }
    })?;
    Ok(rows)
}

fn execute_ordered<S: StorageReader>(
    storage: &S,
    plan: &QueryPlan,
    schema: &crate::TableSchema,
) -> Result<Vec<Row>> {
    let mut scanned = 0_usize;
    let mut ordered_bytes = 0_usize;
    let mut rows = Vec::new();
    visit_candidate_rows(storage, plan, schema, &mut |row| {
        count_scanned_row(&mut scanned)?;
        if matches_filters(row, &plan.filters, &plan.table)?
            && matches_predicate(row, plan.predicate.as_ref(), &plan.table)?
        {
            if rows.len() == MAX_ORDERED_ROWS {
                return Err(EngineError::new(
                    "QUERY_WORK_LIMIT_EXCEEDED",
                    format!(
                        "An ordered query cannot collect more than {MAX_ORDERED_ROWS} matching rows"
                    ),
                ));
            }
            let next_ordered_bytes = checked_result_add(ordered_bytes, owned_row_bytes(row)?)?;
            ensure_result_budget(next_ordered_bytes)?;
            rows.push(row.clone());
            ordered_bytes = next_ordered_bytes;
        }
        Ok(VisitControl::Continue)
    })?;
    sort_rows(&mut rows, &plan.order_by, &plan.table)?;
    let take = plan.limit.unwrap_or(MAX_RESULT_ROWS + 1);
    let mut remaining_ordered_bytes = ordered_bytes;
    let mut result_bytes = 0_usize;
    let mut projected_rows = Vec::new();
    for (index, row) in rows.into_iter().enumerate() {
        if index >= plan.offset && projected_rows.len() == take {
            break;
        }
        let row_bytes = owned_row_bytes(&row)?;
        remaining_ordered_bytes = remaining_ordered_bytes
            .checked_sub(row_bytes)
            .ok_or_else(result_bytes_limit_exceeded)?;
        if index < plan.offset {
            continue;
        }
        if projected_rows.len() == MAX_RESULT_ROWS {
            return Err(result_limit_exceeded());
        }
        let projected_bytes = projected_row_bytes(&row, plan.columns.as_deref(), &plan.table)?;
        let next_result_bytes = checked_result_add(result_bytes, projected_bytes)?;
        ensure_result_budget(next_result_bytes)?;
        ensure_result_budget(checked_result_add(
            remaining_ordered_bytes,
            next_result_bytes,
        )?)?;
        projected_rows.push(project_row(row, plan.columns.as_deref(), &plan.table)?);
        result_bytes = next_result_bytes;
    }
    Ok(projected_rows)
}

fn visit_candidate_rows<S: StorageReader>(
    storage: &S,
    plan: &QueryPlan,
    schema: &crate::TableSchema,
    visitor: &mut dyn FnMut(&Row) -> Result<VisitControl>,
) -> Result<VisitOutcome> {
    if let Some(key) = primary_key_lookup(plan, schema) {
        return match storage.lookup_primary_key(&plan.table, &key)? {
            Some(row) if visitor(&row)? == VisitControl::Stop => Ok(VisitOutcome::Stopped),
            _ => Ok(VisitOutcome::Complete),
        };
    }
    if let Some((columns, key)) = secondary_index_key(storage, plan, schema)?
        && let Some(outcome) = storage.visit_index(&plan.table, &columns, &key, visitor)?
    {
        return Ok(outcome);
    }
    storage.visit_table(&plan.table, visitor)
}

fn secondary_index_key<S: StorageReader>(
    storage: &S,
    plan: &QueryPlan,
    schema: &crate::TableSchema,
) -> Result<Option<(Vec<String>, Row)>> {
    let mut equalities = Map::new();
    for filter in &plan.filters {
        if filter.operator == FilterOperator::Eq && filter.value != Value::Null {
            equalities.insert(filter.column.clone(), filter.value.clone());
        }
    }
    collect_guaranteed_equalities(plan.predicate.as_ref(), &mut equalities);
    for definition in storage.indexes_for_table(&plan.table)? {
        if definition.columns.iter().all(|column| {
            equalities
                .get(column)
                .is_some_and(|value| exact_primary_key_value(schema, column, value))
        }) {
            let key = definition
                .columns
                .iter()
                .map(|column| (column.clone(), equalities[column].clone()))
                .collect();
            return Ok(Some((definition.columns, key)));
        }
    }
    Ok(None)
}

fn count_scanned_row(scanned: &mut usize) -> Result<()> {
    *scanned += 1;
    if *scanned > MAX_SCAN_ROWS {
        Err(EngineError::new(
            "QUERY_WORK_LIMIT_EXCEEDED",
            format!("A query cannot scan more than {MAX_SCAN_ROWS} rows"),
        ))
    } else {
        Ok(())
    }
}

fn result_limit_exceeded() -> EngineError {
    EngineError::new(
        "RESULT_LIMIT_EXCEEDED",
        format!("A query cannot return more than {MAX_RESULT_ROWS} rows"),
    )
}

fn projected_row_bytes(row: &Row, columns: Option<&[String]>, table: &str) -> Result<usize> {
    let Some(columns) = columns else {
        return owned_row_bytes(row);
    };
    let mut bytes = 32_usize;
    for (index, column) in columns.iter().enumerate() {
        let value = row
            .get(column)
            .ok_or_else(|| EngineError::column_not_found(column, table))?;
        if columns[..index].contains(column) {
            return Err(EngineError::column_not_found(column, table));
        }
        bytes = checked_result_add(bytes, 64)?;
        bytes = checked_result_add(bytes, checked_result_mul(column.len(), 2)?)?;
        bytes = checked_result_add(bytes, checked_result_mul(owned_value_bytes(value)?, 2)?)?;
    }
    Ok(bytes)
}

fn owned_row_bytes(row: &Row) -> Result<usize> {
    estimated_row_bytes(row).map_err(|_| result_bytes_limit_exceeded())
}

fn owned_value_bytes(value: &Value) -> Result<usize> {
    estimated_value_bytes(value).map_err(|_| result_bytes_limit_exceeded())
}

fn ensure_result_budget(bytes: usize) -> Result<()> {
    if bytes > MAX_QUERY_RESULT_BYTES {
        Err(result_bytes_limit_exceeded())
    } else {
        Ok(())
    }
}

fn checked_result_add(left: usize, right: usize) -> Result<usize> {
    left.checked_add(right)
        .ok_or_else(result_bytes_limit_exceeded)
}

fn checked_result_mul(left: usize, right: usize) -> Result<usize> {
    left.checked_mul(right)
        .ok_or_else(result_bytes_limit_exceeded)
}

fn result_bytes_limit_exceeded() -> EngineError {
    EngineError::new(
        "QUERY_WORK_LIMIT_EXCEEDED",
        format!("A query cannot materialize more than {MAX_QUERY_RESULT_BYTES} bytes of results"),
    )
}

pub(crate) fn parse_sql(sql: &str, params: &[Value]) -> Result<QueryPlan> {
    validate_sql_input(sql, params)?;
    SqlParser::new(tokenize(sql)?, params).parse()
}

pub(crate) fn validate_sql_input(sql: &str, params: &[Value]) -> Result<()> {
    if sql.len() > MAX_SQL_BYTES {
        return Err(EngineError::invalid_query(format!(
            "SQL text exceeds the {MAX_SQL_BYTES}-byte limit"
        )));
    }
    if params.len() > MAX_PARAMETERS {
        return Err(EngineError::invalid_query(format!(
            "A SQL query cannot receive more than {MAX_PARAMETERS} parameters"
        )));
    }
    for value in params {
        validate_json_value(value).map_err(|error| {
            EngineError::bind_error(format!(
                "A SQL parameter is outside storage bounds: {}",
                error.message
            ))
        })?;
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Token {
    Identifier { value: String, quoted: bool },
    String(String),
    Number(String),
    Placeholder(String),
    Star,
    Comma,
    Dot,
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
    LParen,
    RParen,
    Semicolon,
    Other,
}

struct Lexer<'a> {
    sql: &'a str,
    position: usize,
    tokens: Vec<Token>,
}

impl<'a> Lexer<'a> {
    fn new(sql: &'a str) -> Self {
        Self {
            sql,
            position: 0,
            tokens: Vec::new(),
        }
    }

    fn tokenize(mut self) -> Result<Vec<Token>> {
        while self.position < self.sql.len() {
            self.skip_ignored()?;
            let Some(character) = self.peek() else {
                break;
            };
            let token = match character {
                '*' => {
                    self.advance();
                    Token::Star
                }
                ',' => {
                    self.advance();
                    Token::Comma
                }
                '.' => {
                    self.advance();
                    Token::Dot
                }
                '=' => {
                    self.advance();
                    Token::Eq
                }
                '!' if self.peek_after_current() == Some('=') => {
                    self.advance();
                    self.advance();
                    Token::Neq
                }
                '<' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        Token::Lte
                    } else if self.peek() == Some('>') {
                        self.advance();
                        Token::Neq
                    } else {
                        Token::Lt
                    }
                }
                '>' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        Token::Gte
                    } else {
                        Token::Gt
                    }
                }
                '(' => {
                    self.advance();
                    Token::LParen
                }
                ')' => {
                    self.advance();
                    Token::RParen
                }
                ';' => {
                    self.advance();
                    Token::Semicolon
                }
                '"' => self.quoted_identifier()?,
                '\'' => self.string_literal()?,
                '$' if self
                    .peek_after_current()
                    .is_some_and(|next| next.is_ascii_digit()) =>
                {
                    self.placeholder()
                }
                character if character.is_ascii_digit() => self.number(),
                '-' if self
                    .peek_after_current()
                    .is_some_and(|next| next.is_ascii_digit()) =>
                {
                    self.number()
                }
                character if is_identifier_start(character) => self.identifier(),
                _ => {
                    self.advance();
                    Token::Other
                }
            };
            self.push(token)?;
        }
        Ok(self.tokens)
    }

    fn skip_ignored(&mut self) -> Result<()> {
        loop {
            while self.peek().is_some_and(char::is_whitespace) {
                self.advance();
            }
            if self.remaining().starts_with("--") {
                while self.peek().is_some_and(|character| character != '\n') {
                    self.advance();
                }
                continue;
            }
            if self.remaining().starts_with("/*") {
                self.position += 2;
                let mut depth = 1;
                while depth > 0 {
                    if self.position >= self.sql.len() {
                        return Err(EngineError::parse_error(
                            "Unterminated block comment in SQL text",
                        ));
                    }
                    if self.remaining().starts_with("/*") {
                        depth += 1;
                        if depth > MAX_COMMENT_DEPTH {
                            return Err(EngineError::invalid_query(format!(
                                "SQL comments cannot nest more than {MAX_COMMENT_DEPTH} levels"
                            )));
                        }
                        self.position += 2;
                    } else if self.remaining().starts_with("*/") {
                        depth -= 1;
                        self.position += 2;
                    } else {
                        self.advance();
                    }
                }
                continue;
            }
            return Ok(());
        }
    }

    fn quoted_identifier(&mut self) -> Result<Token> {
        self.advance();
        let mut value = String::new();
        loop {
            let Some(character) = self.peek() else {
                return Err(EngineError::parse_error(
                    "Unterminated quoted identifier in SQL text",
                ));
            };
            self.advance();
            if character == '"' {
                if self.peek() == Some('"') {
                    self.advance();
                    value.push('"');
                    continue;
                }
                break;
            }
            value.push(character);
        }
        Ok(Token::Identifier {
            value,
            quoted: true,
        })
    }

    fn string_literal(&mut self) -> Result<Token> {
        self.advance();
        let mut value = String::new();
        loop {
            let Some(character) = self.peek() else {
                return Err(EngineError::parse_error(
                    "Unterminated string literal in SQL text",
                ));
            };
            self.advance();
            if character == '\'' {
                if self.peek() == Some('\'') {
                    self.advance();
                    value.push('\'');
                    continue;
                }
                break;
            }
            value.push(character);
        }
        Ok(Token::String(value))
    }

    fn placeholder(&mut self) -> Token {
        self.advance();
        let start = self.position;
        while self
            .peek()
            .is_some_and(|character| character.is_ascii_digit())
        {
            self.advance();
        }
        Token::Placeholder(self.sql[start..self.position].to_owned())
    }

    fn number(&mut self) -> Token {
        let start = self.position;
        if self.peek() == Some('-') {
            self.advance();
        }
        while self
            .peek()
            .is_some_and(|character| character.is_ascii_digit())
        {
            self.advance();
        }
        if self.peek() == Some('.') {
            self.advance();
            while self
                .peek()
                .is_some_and(|character| character.is_ascii_digit())
            {
                self.advance();
            }
        }
        if self
            .peek()
            .is_some_and(|character| matches!(character, 'e' | 'E'))
        {
            self.advance();
            if self
                .peek()
                .is_some_and(|character| matches!(character, '+' | '-'))
            {
                self.advance();
            }
            while self
                .peek()
                .is_some_and(|character| character.is_ascii_digit())
            {
                self.advance();
            }
        }
        Token::Number(self.sql[start..self.position].to_owned())
    }

    fn identifier(&mut self) -> Token {
        let start = self.position;
        self.advance();
        while self.peek().is_some_and(is_identifier_continue) {
            self.advance();
        }
        Token::Identifier {
            value: self.sql[start..self.position].to_owned(),
            quoted: false,
        }
    }

    fn push(&mut self, token: Token) -> Result<()> {
        if self.tokens.len() >= MAX_SQL_TOKENS {
            return Err(EngineError::invalid_query(format!(
                "SQL text exceeds the {MAX_SQL_TOKENS}-token limit"
            )));
        }
        self.tokens.push(token);
        Ok(())
    }

    fn remaining(&self) -> &'a str {
        &self.sql[self.position..]
    }

    fn peek(&self) -> Option<char> {
        self.remaining().chars().next()
    }

    fn peek_after_current(&self) -> Option<char> {
        self.remaining().chars().nth(1)
    }

    fn advance(&mut self) {
        if let Some(character) = self.peek() {
            self.position += character.len_utf8();
        }
    }
}

struct SqlParser<'a> {
    tokens: Vec<Token>,
    position: usize,
    params: &'a [Value],
}

impl<'a> SqlParser<'a> {
    fn new(tokens: Vec<Token>, params: &'a [Value]) -> Self {
        Self {
            tokens,
            position: 0,
            params,
        }
    }

    fn parse(mut self) -> Result<QueryPlan> {
        if !self.consume_keyword("select") {
            return Err(EngineError::unsupported_sql(
                "Only read-only SELECT statements are supported",
            ));
        }

        let columns = self.parse_projection()?;
        if !self.consume_keyword("from") {
            return Err(unsupported_shape());
        }
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
        let order_by = if self.consume_keyword("order") {
            self.expect_keyword("by")?;
            self.parse_order_by()?
        } else {
            Vec::new()
        };
        let limit = if self.consume_keyword("limit") {
            Some(self.parse_limit()?)
        } else {
            None
        };
        let offset = if self.consume_keyword("offset") {
            self.parse_limit()?
        } else {
            0
        };

        self.consume(TokenMatcher::Semicolon);
        if !self.is_done() {
            return Err(unsupported_shape());
        }

        Ok(QueryPlan {
            table,
            columns,
            filters: Vec::new(),
            predicate,
            order_by,
            limit,
            offset,
        })
    }

    fn parse_projection(&mut self) -> Result<Option<Vec<String>>> {
        if self.consume(TokenMatcher::Star) {
            return Ok(None);
        }

        let mut columns = Vec::new();
        loop {
            if columns.len() >= MAX_PROJECTION_COLUMNS {
                return Err(EngineError::invalid_query(format!(
                    "A projection cannot contain more than {MAX_PROJECTION_COLUMNS} columns"
                )));
            }
            columns.push(self.parse_identifier()?);
            if !self.consume(TokenMatcher::Comma) {
                break;
            }
        }
        Ok(Some(columns))
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

    fn parse_order_by(&mut self) -> Result<Vec<OrderBy>> {
        let mut orders = Vec::new();
        loop {
            if orders.len() >= MAX_ORDER_COLUMNS {
                return Err(EngineError::invalid_query(format!(
                    "A query cannot order by more than {MAX_ORDER_COLUMNS} columns"
                )));
            }
            let column = self.parse_identifier()?;
            let direction = if self.consume_keyword("desc") {
                OrderDirection::Desc
            } else {
                self.consume_keyword("asc");
                OrderDirection::Asc
            };
            let nulls = if self.consume_keyword("nulls") {
                if self.consume_keyword("first") {
                    NullOrder::First
                } else if self.consume_keyword("last") {
                    NullOrder::Last
                } else {
                    return Err(EngineError::parse_error(
                        "Expected FIRST or LAST after NULLS",
                    ));
                }
            } else {
                NullOrder::Default
            };
            orders.push(OrderBy {
                column,
                direction,
                nulls,
            });
            if !self.consume(TokenMatcher::Comma) {
                break;
            }
        }
        Ok(orders)
    }

    fn parse_limit(&mut self) -> Result<usize> {
        let value = self.parse_value()?;
        let Value::Number(number) = value else {
            return Err(EngineError::invalid_query(
                "LIMIT must be a non-negative integer",
            ));
        };
        number
            .as_u64()
            .and_then(|number| usize::try_from(number).ok())
            .ok_or_else(|| EngineError::invalid_query("LIMIT is too large"))
    }

    fn parse_value(&mut self) -> Result<Value> {
        let Some(token) = self.next() else {
            return Err(EngineError::parse_error("Expected a SQL value"));
        };
        match token {
            Token::String(value) => Ok(Value::String(value)),
            Token::Number(value) => Number::from_str(&value).map(Value::Number).map_err(|_| {
                EngineError::invalid_query(format!("Invalid number literal `{value}`"))
            }),
            Token::Placeholder(index) => bind_parameter(&index, self.params),
            Token::Identifier {
                value,
                quoted: false,
            } if value.eq_ignore_ascii_case("null") => Ok(Value::Null),
            Token::Identifier {
                value,
                quoted: false,
            } if value.eq_ignore_ascii_case("true") => Ok(Value::Bool(true)),
            Token::Identifier {
                value,
                quoted: false,
            } if value.eq_ignore_ascii_case("false") => Ok(Value::Bool(false)),
            _ => Err(unsupported_shape()),
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

    fn expect_keyword(&mut self, keyword: &str) -> Result<()> {
        if self.consume_keyword(keyword) {
            Ok(())
        } else {
            Err(EngineError::parse_error(format!(
                "Expected keyword `{keyword}`"
            )))
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
        token_matches(self.tokens.get(self.position), matcher)
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

pub(crate) fn parse_predicate_at(
    tokens: &[Token],
    position: &mut usize,
    params: &[Value],
) -> Result<Predicate> {
    let mut parser = PredicateParser {
        tokens,
        position: *position,
        params,
        nodes: 0,
    };
    let predicate = parser.parse_or(0)?;
    *position = parser.position;
    Ok(predicate)
}

struct PredicateParser<'a> {
    tokens: &'a [Token],
    position: usize,
    params: &'a [Value],
    nodes: usize,
}

impl PredicateParser<'_> {
    fn parse_or(&mut self, depth: usize) -> Result<Predicate> {
        self.assert_depth(depth)?;
        let mut predicates = vec![self.parse_and(depth)?];
        while self.consume_keyword("or") {
            predicates.push(self.parse_and(depth)?);
        }
        if predicates.len() == 1 {
            Ok(predicates.pop().expect("one predicate was inserted"))
        } else {
            self.node(Predicate::Or { predicates })
        }
    }

    fn parse_and(&mut self, depth: usize) -> Result<Predicate> {
        self.assert_depth(depth)?;
        let mut predicates = vec![self.parse_not(depth)?];
        while self.consume_keyword("and") {
            predicates.push(self.parse_not(depth)?);
        }
        if predicates.len() == 1 {
            Ok(predicates.pop().expect("one predicate was inserted"))
        } else {
            self.node(Predicate::And { predicates })
        }
    }

    fn parse_not(&mut self, depth: usize) -> Result<Predicate> {
        self.assert_depth(depth)?;
        if self.consume_keyword("not") {
            let predicate = self.parse_not(depth + 1)?;
            return self.node(Predicate::Not {
                predicate: Box::new(predicate),
            });
        }
        self.parse_primary(depth)
    }

    fn parse_primary(&mut self, depth: usize) -> Result<Predicate> {
        self.assert_depth(depth)?;
        if self.consume_token(TokenMatcher::LParen) {
            let predicate = self.parse_or(depth + 1)?;
            self.expect_token(TokenMatcher::RParen, "Expected `)` after WHERE expression")?;
            return Ok(predicate);
        }

        let column = self.parse_identifier()?;
        if self.consume_keyword("is") {
            let negated = self.consume_keyword("not");
            if !self.consume_keyword("null") {
                return Err(EngineError::unsupported_sql(
                    "This SQL subset supports IS NULL and IS NOT NULL",
                ));
            }
            return self.node(Predicate::IsNull { column, negated });
        }

        let negated_in = self.consume_keyword("not");
        if self.consume_keyword("in") {
            let predicate = self.parse_in(column)?;
            return if negated_in {
                self.node(Predicate::Not {
                    predicate: Box::new(predicate),
                })
            } else {
                Ok(predicate)
            };
        }
        if negated_in {
            return Err(unsupported_shape());
        }

        let operator = match self.next() {
            Some(Token::Eq) => FilterOperator::Eq,
            Some(Token::Neq) => FilterOperator::Neq,
            Some(Token::Lt) => FilterOperator::Lt,
            Some(Token::Lte) => FilterOperator::Lte,
            Some(Token::Gt) => FilterOperator::Gt,
            Some(Token::Gte) => FilterOperator::Gte,
            _ => return Err(unsupported_shape()),
        };
        let value = self.parse_value()?;
        self.node(Predicate::Comparison {
            column,
            operator,
            value,
        })
    }

    fn parse_in(&mut self, column: String) -> Result<Predicate> {
        self.expect_token(TokenMatcher::LParen, "Expected `(` after IN")?;
        let mut values = Vec::new();
        loop {
            if values.len() >= MAX_IN_VALUES {
                return Err(EngineError::invalid_query(format!(
                    "IN cannot contain more than {MAX_IN_VALUES} values"
                )));
            }
            values.push(self.parse_value()?);
            if !self.consume_token(TokenMatcher::Comma) {
                break;
            }
        }
        self.expect_token(TokenMatcher::RParen, "Expected `)` after IN values")?;
        self.node(Predicate::In { column, values })
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
        let first = if quoted {
            value
        } else {
            value.to_ascii_lowercase()
        };
        if !self.consume_token(TokenMatcher::Dot) {
            return Ok(first);
        }
        let Some(Token::Identifier { value, quoted }) = self.next() else {
            return Err(EngineError::parse_error(
                "Expected a column name after qualifier",
            ));
        };
        if value.is_empty() || (!quoted && is_reserved_keyword(&value)) {
            return Err(EngineError::parse_error(
                "Expected a valid column name after qualifier",
            ));
        }
        let second = if quoted {
            value
        } else {
            value.to_ascii_lowercase()
        };
        if self.consume_token(TokenMatcher::Dot) {
            return Err(EngineError::unsupported_sql(
                "WHERE columns can contain at most one table qualifier",
            ));
        }
        Ok(format!("{first}.{second}"))
    }

    fn parse_value(&mut self) -> Result<Value> {
        let Some(token) = self.next() else {
            return Err(EngineError::parse_error("Expected a SQL value"));
        };
        match token {
            Token::String(value) => Ok(Value::String(value)),
            Token::Number(value) => Number::from_str(&value).map(Value::Number).map_err(|_| {
                EngineError::invalid_query(format!("Invalid number literal `{value}`"))
            }),
            Token::Placeholder(index) => bind_parameter(&index, self.params),
            Token::Identifier {
                value,
                quoted: false,
            } if value.eq_ignore_ascii_case("null") => Ok(Value::Null),
            Token::Identifier {
                value,
                quoted: false,
            } if value.eq_ignore_ascii_case("true") => Ok(Value::Bool(true)),
            Token::Identifier {
                value,
                quoted: false,
            } if value.eq_ignore_ascii_case("false") => Ok(Value::Bool(false)),
            _ => Err(unsupported_shape()),
        }
    }

    fn node(&mut self, predicate: Predicate) -> Result<Predicate> {
        self.nodes += 1;
        if self.nodes > MAX_FILTERS {
            return Err(EngineError::invalid_query(format!(
                "A query cannot contain more than {MAX_FILTERS} predicate nodes"
            )));
        }
        Ok(predicate)
    }

    fn assert_depth(&self, depth: usize) -> Result<()> {
        if depth > MAX_PREDICATE_DEPTH {
            Err(EngineError::invalid_query(format!(
                "A WHERE expression cannot nest more than {MAX_PREDICATE_DEPTH} levels"
            )))
        } else {
            Ok(())
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

    fn consume_token(&mut self, matcher: TokenMatcher) -> bool {
        if token_matches(self.tokens.get(self.position), matcher) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn expect_token(&mut self, matcher: TokenMatcher, message: &str) -> Result<()> {
        if self.consume_token(matcher) {
            Ok(())
        } else {
            Err(EngineError::parse_error(message))
        }
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.position)?.clone();
        self.position += 1;
        Some(token)
    }
}

#[derive(Clone, Copy)]
enum TokenMatcher {
    Star,
    Comma,
    Dot,
    LParen,
    RParen,
    Semicolon,
}

fn token_matches(token: Option<&Token>, matcher: TokenMatcher) -> bool {
    matches!(
        (token, matcher),
        (Some(Token::Star), TokenMatcher::Star)
            | (Some(Token::Comma), TokenMatcher::Comma)
            | (Some(Token::Dot), TokenMatcher::Dot)
            | (Some(Token::LParen), TokenMatcher::LParen)
            | (Some(Token::RParen), TokenMatcher::RParen)
            | (Some(Token::Semicolon), TokenMatcher::Semicolon)
    )
}

pub(crate) fn tokenize(sql: &str) -> Result<Vec<Token>> {
    Lexer::new(sql).tokenize()
}

fn is_identifier_start(character: char) -> bool {
    character == '_' || character.is_ascii_alphabetic() || !character.is_ascii()
}

fn is_identifier_continue(character: char) -> bool {
    is_identifier_start(character) || character.is_ascii_digit() || character == '$'
}

pub(crate) fn is_reserved_keyword(identifier: &str) -> bool {
    [
        "select",
        "from",
        "where",
        "and",
        "or",
        "is",
        "in",
        "limit",
        "offset",
        "order",
        "by",
        "asc",
        "desc",
        "nulls",
        "first",
        "last",
        "null",
        "true",
        "false",
        "create",
        "table",
        "if",
        "not",
        "exists",
        "primary",
        "key",
        "default",
        "insert",
        "into",
        "values",
        "update",
        "set",
        "delete",
        "returning",
        "as",
        "join",
        "inner",
        "left",
        "outer",
        "on",
        "group",
        "having",
    ]
    .iter()
    .any(|keyword| identifier.eq_ignore_ascii_case(keyword))
}

pub(crate) fn bind_parameter(index: &str, params: &[Value]) -> Result<Value> {
    let placeholder = format!("${index}");
    let index = index.parse::<usize>().map_err(|_| {
        EngineError::bind_error(format!("Invalid parameter placeholder `{placeholder}`"))
    })?;
    if index == 0 {
        return Err(EngineError::bind_error(
            "PostgreSQL parameter indexes start at $1",
        ));
    }
    params.get(index - 1).cloned().ok_or_else(|| {
        EngineError::bind_error(format!("No value was provided for `{placeholder}`"))
    })
}

pub(crate) fn matches_filters(row: &Row, filters: &[Filter], table: &str) -> Result<bool> {
    for filter in filters {
        let Some(actual) = row.get(&filter.column) else {
            return Err(EngineError::column_not_found(&filter.column, table));
        };
        if evaluate_comparison(
            actual,
            &filter.value,
            filter.operator,
            table,
            &filter.column,
        )? != Truth::True
        {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn matches_predicate(
    row: &Row,
    predicate: Option<&Predicate>,
    table: &str,
) -> Result<bool> {
    predicate
        .map(|predicate| evaluate_predicate(row, predicate, table))
        .transpose()
        .map(|truth| truth.unwrap_or(Truth::True) == Truth::True)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Truth {
    True,
    False,
    Unknown,
}

fn evaluate_predicate(row: &Row, predicate: &Predicate, table: &str) -> Result<Truth> {
    match predicate {
        Predicate::Comparison {
            column,
            operator,
            value,
        } => evaluate_comparison(
            row.get(column)
                .ok_or_else(|| EngineError::column_not_found(column, table))?,
            value,
            *operator,
            table,
            column,
        ),
        Predicate::IsNull { column, negated } => {
            let is_null = row
                .get(column)
                .ok_or_else(|| EngineError::column_not_found(column, table))?
                == &Value::Null;
            Ok(if is_null ^ negated {
                Truth::True
            } else {
                Truth::False
            })
        }
        Predicate::In { column, values } => {
            let actual = row
                .get(column)
                .ok_or_else(|| EngineError::column_not_found(column, table))?;
            let mut unknown = false;
            for value in values {
                match evaluate_comparison(actual, value, FilterOperator::Eq, table, column)? {
                    Truth::True => return Ok(Truth::True),
                    Truth::Unknown => unknown = true,
                    Truth::False => {}
                }
            }
            Ok(if unknown {
                Truth::Unknown
            } else {
                Truth::False
            })
        }
        Predicate::And { predicates } => {
            let mut unknown = false;
            for predicate in predicates {
                match evaluate_predicate(row, predicate, table)? {
                    Truth::False => return Ok(Truth::False),
                    Truth::Unknown => unknown = true,
                    Truth::True => {}
                }
            }
            Ok(if unknown { Truth::Unknown } else { Truth::True })
        }
        Predicate::Or { predicates } => {
            let mut unknown = false;
            for predicate in predicates {
                match evaluate_predicate(row, predicate, table)? {
                    Truth::True => return Ok(Truth::True),
                    Truth::Unknown => unknown = true,
                    Truth::False => {}
                }
            }
            Ok(if unknown {
                Truth::Unknown
            } else {
                Truth::False
            })
        }
        Predicate::Not { predicate } => Ok(match evaluate_predicate(row, predicate, table)? {
            Truth::True => Truth::False,
            Truth::False => Truth::True,
            Truth::Unknown => Truth::Unknown,
        }),
    }
}

fn evaluate_comparison(
    left: &Value,
    right: &Value,
    operator: FilterOperator,
    table: &str,
    column: &str,
) -> Result<Truth> {
    if left == &Value::Null || right == &Value::Null {
        return Ok(Truth::Unknown);
    }
    if matches!(operator, FilterOperator::Eq | FilterOperator::Neq) {
        let equal = values_equal(left, right, table, column)?;
        let matched = if operator == FilterOperator::Eq {
            equal
        } else {
            !equal
        };
        return Ok(if matched { Truth::True } else { Truth::False });
    }
    let ordering = compare_values(left, right, table, column)?;
    let matched = match operator {
        FilterOperator::Eq | FilterOperator::Neq => unreachable!("handled above"),
        FilterOperator::Lt => ordering == Ordering::Less,
        FilterOperator::Lte => ordering != Ordering::Greater,
        FilterOperator::Gt => ordering == Ordering::Greater,
        FilterOperator::Gte => ordering != Ordering::Less,
    };
    Ok(if matched { Truth::True } else { Truth::False })
}

fn values_equal(left: &Value, right: &Value, table: &str, column: &str) -> Result<bool> {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => Ok(left.as_f64() == right.as_f64()),
        (Value::String(left), Value::String(right)) => Ok(left == right),
        (Value::Bool(left), Value::Bool(right)) => Ok(left == right),
        (Value::Array(left), Value::Array(right)) => Ok(left == right),
        (Value::Object(left), Value::Object(right)) => Ok(left == right),
        _ => Err(comparison_error(table, column)),
    }
}

fn compare_values(left: &Value, right: &Value, table: &str, column: &str) -> Result<Ordering> {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left
            .as_f64()
            .and_then(|left| right.as_f64().and_then(|right| left.partial_cmp(&right)))
            .ok_or_else(|| comparison_error(table, column)),
        (Value::String(left), Value::String(right)) => Ok(left.cmp(right)),
        (Value::Bool(left), Value::Bool(right)) => Ok(left.cmp(right)),
        (Value::Array(_), Value::Array(_)) | (Value::Object(_), Value::Object(_)) => {
            Err(comparison_error(table, column))
        }
        _ => Err(comparison_error(table, column)),
    }
}

fn comparison_error(table: &str, column: &str) -> EngineError {
    EngineError::type_mismatch(format!(
        "Values compared with column `{column}` in `{table}` must have compatible scalar types"
    ))
}

fn sort_rows(rows: &mut [Row], order_by: &[OrderBy], table: &str) -> Result<()> {
    validate_order_values(rows, order_by, table)?;
    rows.sort_by(|left, right| {
        for order in order_by {
            match compare_order_column(left, right, order) {
                Ordering::Equal => {}
                ordering => return ordering,
            }
        }
        Ordering::Equal
    });
    Ok(())
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum OrderValueKind {
    Number,
    String,
    Boolean,
}

fn validate_order_values(rows: &[Row], order_by: &[OrderBy], table: &str) -> Result<()> {
    for order in order_by {
        let mut expected = None;
        for row in rows {
            let value = row
                .get(&order.column)
                .ok_or_else(|| EngineError::column_not_found(&order.column, table))?;
            let kind = match value {
                Value::Null => continue,
                Value::Number(_) => OrderValueKind::Number,
                Value::String(_) => OrderValueKind::String,
                Value::Bool(_) => OrderValueKind::Boolean,
                Value::Array(_) | Value::Object(_) => {
                    return Err(comparison_error(table, &order.column));
                }
            };
            if expected.is_some_and(|expected| expected != kind) {
                return Err(comparison_error(table, &order.column));
            }
            expected = Some(kind);
        }
    }
    Ok(())
}

fn compare_order_column(left: &Row, right: &Row, order: &OrderBy) -> Ordering {
    let left = &left[&order.column];
    let right = &right[&order.column];
    let nulls_first = match order.nulls {
        NullOrder::First => true,
        NullOrder::Last => false,
        NullOrder::Default => order.direction == OrderDirection::Desc,
    };
    let ordering = match (left == &Value::Null, right == &Value::Null) {
        (true, true) => Ordering::Equal,
        (true, false) => {
            if nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (false, true) => {
            if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, false) => match (left, right) {
            (Value::Number(left), Value::Number(right)) => left
                .as_f64()
                .and_then(|left| right.as_f64().and_then(|right| left.partial_cmp(&right)))
                .unwrap_or(Ordering::Equal),
            (Value::String(left), Value::String(right)) => left.cmp(right),
            (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
            _ => Ordering::Equal,
        },
    };
    if order.direction == OrderDirection::Desc {
        // Explicit null placement is independent of direction.
        if left == &Value::Null || right == &Value::Null {
            ordering
        } else {
            ordering.reverse()
        }
    } else {
        ordering
    }
}

fn validate_predicate_complexity(predicate: Option<&Predicate>) -> Result<()> {
    fn visit(predicate: &Predicate, depth: usize, nodes: &mut usize) -> Result<()> {
        if depth > MAX_PREDICATE_DEPTH {
            return Err(EngineError::invalid_query(format!(
                "A WHERE expression cannot nest more than {MAX_PREDICATE_DEPTH} levels"
            )));
        }
        *nodes += 1;
        if *nodes > MAX_FILTERS {
            return Err(EngineError::invalid_query(format!(
                "A query cannot contain more than {MAX_FILTERS} predicate nodes"
            )));
        }
        match predicate {
            Predicate::In { values, .. } if values.len() > MAX_IN_VALUES => {
                Err(EngineError::invalid_query(format!(
                    "IN cannot contain more than {MAX_IN_VALUES} values"
                )))
            }
            Predicate::And { predicates } | Predicate::Or { predicates } => {
                for predicate in predicates {
                    visit(predicate, depth + 1, nodes)?;
                }
                Ok(())
            }
            Predicate::Not { predicate } => visit(predicate, depth + 1, nodes),
            _ => Ok(()),
        }
    }
    let Some(predicate) = predicate else {
        return Ok(());
    };
    visit(predicate, 0, &mut 0)
}

pub(crate) fn validate_predicate_columns(
    predicate: &Predicate,
    schema: &crate::TableSchema,
    table: &str,
) -> Result<()> {
    match predicate {
        Predicate::Comparison { column, .. }
        | Predicate::IsNull { column, .. }
        | Predicate::In { column, .. } => {
            if schema.columns.iter().any(|item| item.name == *column) {
                Ok(())
            } else {
                Err(EngineError::column_not_found(column, table))
            }
        }
        Predicate::And { predicates } | Predicate::Or { predicates } => {
            for predicate in predicates {
                validate_predicate_columns(predicate, schema, table)?;
            }
            Ok(())
        }
        Predicate::Not { predicate } => validate_predicate_columns(predicate, schema, table),
    }
}

fn column_definition<'a>(
    schema: &'a crate::TableSchema,
    column: &str,
    table: &str,
) -> Result<&'a ColumnDefinition> {
    schema
        .columns
        .iter()
        .find(|definition| definition.name == column)
        .ok_or_else(|| EngineError::column_not_found(column, table))
}

pub(crate) fn validate_predicate_types(
    predicate: &Predicate,
    schema: &crate::TableSchema,
    table: &str,
) -> Result<()> {
    match predicate {
        Predicate::Comparison {
            column,
            operator,
            value,
        } => validate_comparison_value(
            column_definition(schema, column, table)?,
            *operator,
            value,
            table,
        ),
        Predicate::In { column, values } => {
            let definition = column_definition(schema, column, table)?;
            for value in values {
                validate_comparison_value(definition, FilterOperator::Eq, value, table)?;
            }
            Ok(())
        }
        Predicate::IsNull { .. } => Ok(()),
        Predicate::And { predicates } | Predicate::Or { predicates } => {
            for predicate in predicates {
                validate_predicate_types(predicate, schema, table)?;
            }
            Ok(())
        }
        Predicate::Not { predicate } => validate_predicate_types(predicate, schema, table),
    }
}

fn validate_comparison_value(
    definition: &ColumnDefinition,
    operator: FilterOperator,
    value: &Value,
    table: &str,
) -> Result<()> {
    if value == &Value::Null {
        return Ok(());
    }
    let compatible = match definition.data_type {
        ColumnType::Boolean => value.is_boolean(),
        ColumnType::Integer | ColumnType::Float => value.is_number(),
        ColumnType::Text => value.is_string(),
        ColumnType::Json => matches!(operator, FilterOperator::Eq | FilterOperator::Neq),
    };
    if compatible {
        Ok(())
    } else {
        Err(comparison_error(table, &definition.name))
    }
}

fn primary_key_lookup(plan: &QueryPlan, schema: &crate::TableSchema) -> Option<Row> {
    let mut equalities = Map::new();
    for filter in &plan.filters {
        if filter.operator == FilterOperator::Eq && filter.value != Value::Null {
            equalities.insert(filter.column.clone(), filter.value.clone());
        }
    }
    collect_guaranteed_equalities(plan.predicate.as_ref(), &mut equalities);
    schema
        .primary_key
        .iter()
        .all(|column| {
            equalities
                .get(column)
                .is_some_and(|value| exact_primary_key_value(schema, column, value))
        })
        .then(|| {
            schema
                .primary_key
                .iter()
                .map(|column| (column.clone(), equalities[column].clone()))
                .collect()
        })
}

/// A direct B-tree lookup must be semantically indistinguishable from scanning
/// and evaluating the predicate. Untyped replication schemas cannot prove that
/// a lookup miss is not really an incompatible-type error. Floats likewise
/// have multiple JSON encodings that compare numerically equal (`1`/`1.0`).
fn exact_primary_key_value(schema: &crate::TableSchema, column: &str, value: &Value) -> bool {
    let Some(definition) = schema.columns.iter().find(|item| item.name == column) else {
        return false;
    };
    match definition.data_type {
        ColumnType::Boolean => value.is_boolean(),
        ColumnType::Integer => {
            const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
            value
                .as_u64()
                .is_some_and(|number| number <= MAX_SAFE_INTEGER)
                || value.as_i64().is_some_and(|number| {
                    number >= -(MAX_SAFE_INTEGER as i64) && number <= MAX_SAFE_INTEGER as i64
                })
        }
        ColumnType::Text => value.is_string(),
        ColumnType::Float | ColumnType::Json => false,
    }
}

fn collect_guaranteed_equalities(predicate: Option<&Predicate>, values: &mut Row) {
    match predicate {
        Some(Predicate::Comparison {
            column,
            operator: FilterOperator::Eq,
            value,
        }) if value != &Value::Null => {
            values.insert(column.clone(), value.clone());
        }
        Some(Predicate::And { predicates }) => {
            for predicate in predicates {
                collect_guaranteed_equalities(Some(predicate), values);
            }
        }
        _ => {}
    }
}

pub(crate) fn project_row(mut row: Row, columns: Option<&[String]>, table: &str) -> Result<Row> {
    let Some(columns) = columns else {
        return Ok(row);
    };
    let mut projected = Map::new();
    for column in columns {
        let value = row
            .remove(column)
            .ok_or_else(|| EngineError::column_not_found(column, table))?;
        projected.insert(column.clone(), value);
    }
    Ok(projected)
}

fn unsupported_shape() -> EngineError {
    EngineError::unsupported_sql(
        "The SQL subset supports simple projections and predicates over one table, ordering, and pagination",
    )
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use serde_json::json;

    use super::*;
    use crate::{ColumnDefinition, Engine, InMemoryStorage, IndexDefinition, TableSchema};

    fn row(value: Value) -> Row {
        value
            .as_object()
            .expect("test row must be an object")
            .clone()
    }

    fn engine() -> Engine<InMemoryStorage> {
        let mut engine = Engine::default();
        engine
            .define_table(TableSchema {
                name: "posts".to_owned(),
                primary_key: vec!["id".to_owned()],
                columns: vec![
                    ColumnDefinition {
                        name: "id".to_owned(),
                        data_type: ColumnType::Integer,
                        nullable: false,
                        default: None,
                    },
                    ColumnDefinition {
                        name: "title".to_owned(),
                        data_type: ColumnType::Text,
                        nullable: false,
                        default: None,
                    },
                    ColumnDefinition {
                        name: "user_id".to_owned(),
                        data_type: ColumnType::Integer,
                        nullable: true,
                        default: None,
                    },
                    ColumnDefinition {
                        name: "deleted".to_owned(),
                        data_type: ColumnType::Boolean,
                        nullable: true,
                        default: None,
                    },
                ],
            })
            .unwrap();
        engine
            .replace_table(
                "posts",
                vec![
                    row(json!({"id": 1, "title": "one", "user_id": 7, "deleted": null})),
                    row(json!({"id": 2, "title": "two", "user_id": 8, "deleted": null})),
                    row(json!({"id": 3, "title": "three", "user_id": 7, "deleted": true})),
                ],
            )
            .unwrap();
        engine
    }

    struct VisitorOnlyStorage {
        schema: TableSchema,
        repeated_row: Row,
        repetitions: usize,
        table_rows: Vec<Row>,
        index: Option<(IndexDefinition, Option<Vec<Row>>)>,
        table_visits: Cell<usize>,
        index_visits: Cell<usize>,
    }

    impl VisitorOnlyStorage {
        fn new(repeated_row: Row, repetitions: usize) -> Self {
            Self {
                schema: TableSchema {
                    name: "items".to_owned(),
                    primary_key: vec!["id".to_owned()],
                    columns: vec![
                        ColumnDefinition {
                            name: "id".to_owned(),
                            data_type: ColumnType::Integer,
                            nullable: false,
                            default: None,
                        },
                        ColumnDefinition {
                            name: "user_id".to_owned(),
                            data_type: ColumnType::Integer,
                            nullable: false,
                            default: None,
                        },
                        ColumnDefinition {
                            name: "selected".to_owned(),
                            data_type: ColumnType::Boolean,
                            nullable: true,
                            default: None,
                        },
                    ],
                },
                repeated_row,
                repetitions,
                table_rows: Vec::new(),
                index: None,
                table_visits: Cell::new(0),
                index_visits: Cell::new(0),
            }
        }

        fn add_payload_column(&mut self) {
            self.schema.columns.push(ColumnDefinition {
                name: "payload".to_owned(),
                data_type: ColumnType::Text,
                nullable: false,
                default: None,
            });
        }
    }

    impl StorageReader for VisitorOnlyStorage {
        fn visit_table(
            &self,
            table: &str,
            visitor: &mut dyn FnMut(&Row) -> Result<VisitControl>,
        ) -> Result<VisitOutcome> {
            if table != self.schema.name {
                return Err(EngineError::table_not_found(table));
            }
            for row in std::iter::repeat_n(&self.repeated_row, self.repetitions)
                .chain(self.table_rows.iter())
            {
                self.table_visits.set(self.table_visits.get() + 1);
                if visitor(row)? == VisitControl::Stop {
                    return Ok(VisitOutcome::Stopped);
                }
            }
            Ok(VisitOutcome::Complete)
        }

        fn table_row_count(&self, table: &str) -> Result<usize> {
            if table != self.schema.name {
                return Err(EngineError::table_not_found(table));
            }
            Ok(self.repetitions + self.table_rows.len())
        }

        fn scan_table(&self, _table: &str) -> Result<Vec<Row>> {
            panic!("simple queries must use visit_table")
        }

        fn lookup_primary_key(&self, _table: &str, _key: &Row) -> Result<Option<Row>> {
            Ok(None)
        }

        fn index_definition(&self, name: &str) -> Option<IndexDefinition> {
            self.index
                .as_ref()
                .map(|(definition, _)| definition)
                .filter(|definition| definition.name == name)
                .cloned()
        }

        fn indexes_for_table(&self, table: &str) -> Result<Vec<IndexDefinition>> {
            if table != self.schema.name {
                return Err(EngineError::table_not_found(table));
            }
            Ok(self
                .index
                .as_ref()
                .map(|(definition, _)| vec![definition.clone()])
                .unwrap_or_default())
        }

        fn visit_index(
            &self,
            table: &str,
            columns: &[String],
            _key: &Row,
            visitor: &mut dyn FnMut(&Row) -> Result<VisitControl>,
        ) -> Result<Option<VisitOutcome>> {
            if table != self.schema.name {
                return Err(EngineError::table_not_found(table));
            }
            let Some((definition, postings)) = &self.index else {
                return Ok(None);
            };
            if definition.columns != columns {
                return Ok(None);
            }
            let Some(postings) = postings else {
                return Ok(None);
            };
            for row in postings {
                self.index_visits.set(self.index_visits.get() + 1);
                if visitor(row)? == VisitControl::Stop {
                    return Ok(Some(VisitOutcome::Stopped));
                }
            }
            Ok(Some(VisitOutcome::Complete))
        }

        fn lookup_index(
            &self,
            _table: &str,
            _columns: &[String],
            _key: &Row,
        ) -> Result<Option<Vec<Row>>> {
            panic!("simple queries must use visit_index")
        }

        fn table_schema(&self, table: &str) -> Result<TableSchema> {
            if table == self.schema.name {
                Ok(self.schema.clone())
            } else {
                Err(EngineError::table_not_found(table))
            }
        }

        fn revision(&self) -> u64 {
            7
        }
    }

    #[test]
    fn executes_projection_parameter_filter_and_limit() {
        let result = engine()
            .query_sql(
                "SELECT id, title FROM posts WHERE user_id = $1 LIMIT 1",
                &[json!(7)],
            )
            .unwrap();

        assert_eq!(result.revision, 1);
        assert_eq!(result.rows, vec![row(json!({"id": 1, "title": "one"}))]);
    }

    #[test]
    fn limit_zero_returns_no_rows() {
        let database = engine();
        assert_eq!(
            database
                .query_sql("SELECT missing FROM posts LIMIT 0", &[])
                .unwrap_err()
                .code,
            "COLUMN_NOT_FOUND"
        );
        assert_eq!(
            database
                .query_sql("SELECT * FROM posts WHERE id = 'bad' LIMIT 0", &[])
                .unwrap_err()
                .code,
            "TYPE_MISMATCH"
        );
        let result = database
            .query_sql("SELECT * FROM posts LIMIT 0", &[])
            .unwrap();

        assert_eq!(result.revision, 1);
        assert!(result.rows.is_empty());
        assert_eq!(database.into_storage().visitor_counts(), (0, 0));
    }

    #[test]
    fn unordered_limits_stop_visiting_at_the_requested_window() {
        let database = engine();
        assert_eq!(
            database
                .query_sql("SELECT id FROM posts LIMIT 1 OFFSET 1", &[])
                .unwrap()
                .rows,
            vec![row(json!({"id": 2}))]
        );
        let storage = database.into_storage();
        assert_eq!(storage.visitor_counts(), (2, 0));
        assert_eq!(storage.access_counts(), (1, 0));
    }

    #[test]
    fn supports_anded_equality_filters() {
        let result = engine()
            .query_sql(
                "SELECT * FROM posts WHERE user_id = $1 AND deleted = $2",
                &[json!(7), json!(true)],
            )
            .unwrap();

        assert_eq!(
            result.rows,
            vec![row(
                json!({"id": 3, "title": "three", "user_id": 7, "deleted": true})
            )]
        );
    }

    #[test]
    fn evaluates_bounded_boolean_predicates_with_sql_null_logic() {
        let database = engine();
        let result = database
            .query_sql(
                "SELECT id FROM posts \
                 WHERE user_id >= 7 AND (title <> 'two' OR deleted IS NOT NULL) \
                 ORDER BY id DESC",
                &[],
            )
            .unwrap();
        assert_eq!(
            result.rows,
            vec![row(json!({"id": 3})), row(json!({"id": 1}))]
        );

        assert_eq!(
            database
                .query_sql("SELECT id FROM posts WHERE NOT deleted IS NULL", &[])
                .unwrap()
                .rows,
            vec![row(json!({"id": 3}))]
        );
        assert_eq!(
            database
                .query_sql(
                    "SELECT id FROM posts WHERE id IN (3, 1, NULL) ORDER BY id",
                    &[],
                )
                .unwrap()
                .rows,
            vec![row(json!({"id": 1})), row(json!({"id": 3}))]
        );
        assert!(
            database
                .query_sql("SELECT id FROM posts WHERE id NOT IN (1, NULL)", &[])
                .unwrap()
                .rows
                .is_empty()
        );
    }

    #[test]
    fn orders_nulls_like_postgres_then_applies_offset_and_limit() {
        let database = engine();
        assert_eq!(
            database
                .query_sql(
                    "SELECT id FROM posts ORDER BY deleted ASC, id DESC LIMIT 2 OFFSET 1",
                    &[],
                )
                .unwrap()
                .rows,
            vec![row(json!({"id": 2})), row(json!({"id": 1}))]
        );
        assert_eq!(
            database
                .query_sql(
                    "SELECT id FROM posts ORDER BY deleted DESC NULLS LAST, id ASC",
                    &[],
                )
                .unwrap()
                .rows,
            vec![
                row(json!({"id": 3})),
                row(json!({"id": 1})),
                row(json!({"id": 2})),
            ]
        );
        assert_eq!(database.into_storage().visitor_counts(), (6, 0));
    }

    #[test]
    fn complete_primary_key_equality_uses_direct_row_lookup() {
        let database = engine();
        assert_eq!(
            database
                .query_sql("SELECT title FROM posts WHERE id = $1", &[json!(2)])
                .unwrap()
                .rows,
            vec![row(json!({"title": "two"}))]
        );
        assert_eq!(database.into_storage().access_counts(), (0, 1));

        let database = engine();
        assert_eq!(
            database
                .query_sql("SELECT title FROM posts WHERE id = 1.0", &[])
                .unwrap()
                .rows,
            vec![row(json!({"title": "one"}))]
        );
        assert_eq!(database.into_storage().access_counts(), (1, 0));

        let database = engine();
        assert_eq!(
            database
                .query_sql("SELECT title FROM posts WHERE title > 1 AND id = 999", &[],)
                .unwrap_err()
                .code,
            "TYPE_MISMATCH"
        );
        assert_eq!(database.into_storage().access_counts(), (0, 0));

        let database = engine();
        assert_eq!(
            database
                .query_sql("SELECT title FROM posts WHERE id = '1' AND id = 999", &[],)
                .unwrap_err()
                .code,
            "TYPE_MISMATCH"
        );
        assert_eq!(database.into_storage().access_counts(), (0, 0));

        let database = engine();
        assert_eq!(
            database
                .query_sql("SELECT title FROM posts WHERE id = '1'", &[])
                .unwrap_err()
                .code,
            "TYPE_MISMATCH"
        );
        assert_eq!(database.into_storage().access_counts(), (0, 0));

        let database = engine();
        database
            .query_sql("SELECT * FROM posts WHERE id = 1 OR id = 2", &[])
            .unwrap();
        assert_eq!(database.into_storage().access_counts(), (1, 0));
    }

    #[test]
    fn complete_secondary_index_equality_uses_postings_and_residual_filters() {
        let mut database = engine();
        database
            .execute_sql("CREATE INDEX posts_user ON posts (user_id)", &[])
            .unwrap();
        assert_eq!(
            database
                .query_sql(
                    "SELECT title FROM posts WHERE user_id = 7 AND deleted = true",
                    &[],
                )
                .unwrap()
                .rows,
            vec![row(json!({"title": "three"}))]
        );
        let storage = database.into_storage();
        assert_eq!(storage.access_counts(), (0, 1));
        assert_eq!(storage.visitor_counts(), (2, 0));

        let mut database = engine();
        database
            .execute_sql(
                "CREATE INDEX posts_user_deleted ON posts (user_id, deleted)",
                &[],
            )
            .unwrap();
        database
            .query_sql("SELECT * FROM posts WHERE user_id = 7", &[])
            .unwrap();
        assert_eq!(database.into_storage().access_counts(), (1, 0));

        let mut database = engine();
        database
            .execute_sql("CREATE INDEX posts_user ON posts (user_id)", &[])
            .unwrap();
        database
            .query_sql("SELECT * FROM posts WHERE user_id = 7 OR user_id = 8", &[])
            .unwrap();
        assert_eq!(database.into_storage().access_counts(), (1, 0));
    }

    #[test]
    fn engine_queries_a_storage_reader_without_a_write_contract() {
        let mut storage =
            VisitorOnlyStorage::new(row(json!({"id": 0, "user_id": 0, "selected": false})), 0);
        storage
            .table_rows
            .push(row(json!({"id": 1, "user_id": 7, "selected": true})));
        let engine = Engine::new(storage);

        let result = engine
            .query_sql(
                "SELECT id FROM items WHERE user_id = 7 AND selected = true",
                &[],
            )
            .unwrap();

        assert_eq!(result.revision, 7);
        assert_eq!(result.rows, vec![row(json!({"id": 1}))]);
    }

    #[test]
    fn visitor_index_absence_falls_back_but_an_empty_posting_does_not() {
        let definition = IndexDefinition {
            name: "items_user".to_owned(),
            table: "items".to_owned(),
            columns: vec!["user_id".to_owned()],
            unique: false,
        };
        let matching = row(json!({"id": 1, "user_id": 7, "selected": true}));
        let mut absent =
            VisitorOnlyStorage::new(row(json!({"id": 0, "user_id": 0, "selected": false})), 0);
        absent.table_rows.push(matching.clone());
        absent.index = Some((definition.clone(), None));
        assert_eq!(
            execute(
                &absent,
                &parse_sql(
                    "SELECT id FROM items WHERE user_id = 7 AND selected = true",
                    &[],
                )
                .unwrap(),
            )
            .unwrap()
            .rows,
            vec![row(json!({"id": 1}))]
        );
        assert_eq!(absent.table_visits.get(), 1);

        let mut empty = VisitorOnlyStorage::new(matching, 0);
        empty.index = Some((definition, Some(Vec::new())));
        assert!(
            execute(
                &empty,
                &parse_sql("SELECT id FROM items WHERE user_id = 7", &[]).unwrap(),
            )
            .unwrap()
            .rows
            .is_empty()
        );
        assert_eq!(empty.table_visits.get(), 0);
    }

    #[test]
    fn index_visitors_apply_residual_filters_before_stopping() {
        let mut storage =
            VisitorOnlyStorage::new(row(json!({"id": 0, "user_id": 0, "selected": false})), 0);
        storage.index = Some((
            IndexDefinition {
                name: "items_user".to_owned(),
                table: "items".to_owned(),
                columns: vec!["user_id".to_owned()],
                unique: false,
            },
            Some(vec![
                row(json!({"id": 1, "user_id": 7, "selected": false})),
                row(json!({"id": 2, "user_id": 7, "selected": true})),
                row(json!({"id": 3, "user_id": 7, "selected": true})),
            ]),
        ));
        assert_eq!(
            execute(
                &storage,
                &parse_sql(
                    "SELECT id FROM items WHERE user_id = 7 AND selected = true LIMIT 1",
                    &[],
                )
                .unwrap(),
            )
            .unwrap()
            .rows,
            vec![row(json!({"id": 2}))]
        );
        assert_eq!(storage.index_visits.get(), 2);
        assert_eq!(storage.table_visits.get(), 0);
    }

    #[test]
    fn simple_query_work_and_result_caps_are_explicit_and_read_free_when_static() {
        let non_match = row(json!({"id": 1, "user_id": 1, "selected": false}));
        let storage = VisitorOnlyStorage::new(non_match.clone(), MAX_SCAN_ROWS + 1);
        let error = execute(
            &storage,
            &parse_sql("SELECT * FROM items WHERE selected = true", &[]).unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.code, "QUERY_WORK_LIMIT_EXCEEDED");
        assert_eq!(storage.table_visits.get(), MAX_SCAN_ROWS + 1);

        let mut storage = VisitorOnlyStorage::new(Map::new(), MAX_RESULT_ROWS + 1);
        storage.schema.columns.clear();
        assert_eq!(
            execute(&storage, &parse_sql("SELECT * FROM items", &[]).unwrap())
                .unwrap_err()
                .code,
            "RESULT_LIMIT_EXCEEDED"
        );
        assert_eq!(storage.table_visits.get(), MAX_RESULT_ROWS + 1);

        let mut storage = VisitorOnlyStorage::new(row(json!({"id": 1})), MAX_ORDERED_ROWS + 1);
        storage.schema.columns.truncate(1);
        assert_eq!(
            execute(
                &storage,
                &parse_sql("SELECT * FROM items ORDER BY id LIMIT 1", &[]).unwrap(),
            )
            .unwrap_err()
            .code,
            "QUERY_WORK_LIMIT_EXCEEDED"
        );
        assert_eq!(storage.table_visits.get(), MAX_ORDERED_ROWS + 1);

        let storage = VisitorOnlyStorage::new(non_match, 1);
        let mut plan = parse_sql("SELECT * FROM items", &[]).unwrap();
        plan.limit = Some(MAX_RESULT_ROWS + 1);
        assert_eq!(
            execute(&storage, &plan).unwrap_err().code,
            "RESULT_LIMIT_EXCEEDED"
        );
        plan.limit = Some(2);
        plan.offset = usize::MAX;
        assert_eq!(execute(&storage, &plan).unwrap_err().code, "INVALID_QUERY");
        assert_eq!(storage.table_visits.get(), 0);
    }

    #[test]
    fn oversized_borrowed_rows_fail_before_unordered_or_ordered_clones() {
        let mut storage = VisitorOnlyStorage::new(
            row(json!({
                "id": 1,
                "user_id": 1,
                "selected": true,
                "payload": "x".repeat(MAX_QUERY_RESULT_BYTES / 2),
            })),
            1,
        );
        storage.add_payload_column();

        assert_eq!(
            execute(
                &storage,
                &parse_sql("SELECT payload FROM items", &[]).unwrap(),
            )
            .unwrap_err()
            .code,
            "QUERY_WORK_LIMIT_EXCEEDED"
        );
        assert_eq!(storage.table_visits.get(), 1);

        storage.table_visits.set(0);
        assert_eq!(
            execute(
                &storage,
                &parse_sql("SELECT id FROM items ORDER BY id LIMIT 1", &[]).unwrap(),
            )
            .unwrap_err()
            .code,
            "QUERY_WORK_LIMIT_EXCEEDED"
        );
        assert_eq!(storage.table_visits.get(), 1);
    }

    #[test]
    fn cumulative_borrowed_results_reject_the_first_row_over_the_byte_budget() {
        let repeated_row = row(json!({
            "id": 1,
            "user_id": 1,
            "selected": true,
            "payload": "x".repeat(128 * 1024),
        }));
        let plan = parse_sql("SELECT payload FROM items", &[]).unwrap();
        let projected_bytes =
            projected_row_bytes(&repeated_row, plan.columns.as_deref(), "items").unwrap();
        let clone_bytes = owned_row_bytes(&repeated_row).unwrap();
        let accepted_rows = (MAX_QUERY_RESULT_BYTES - clone_bytes) / projected_bytes + 1;
        assert!((accepted_rows - 1) * projected_bytes + clone_bytes <= MAX_QUERY_RESULT_BYTES);
        assert!(accepted_rows * projected_bytes + clone_bytes > MAX_QUERY_RESULT_BYTES);

        let mut accepted = VisitorOnlyStorage::new(repeated_row.clone(), accepted_rows);
        accepted.add_payload_column();
        assert_eq!(execute(&accepted, &plan).unwrap().rows.len(), accepted_rows);
        assert_eq!(accepted.table_visits.get(), accepted_rows);

        let mut rejected = VisitorOnlyStorage::new(repeated_row, accepted_rows + 1);
        rejected.add_payload_column();
        assert_eq!(
            execute(&rejected, &plan).unwrap_err().code,
            "QUERY_WORK_LIMIT_EXCEEDED"
        );
        assert_eq!(rejected.table_visits.get(), accepted_rows + 1);
    }

    #[test]
    fn ordered_candidates_reject_the_first_borrowed_clone_over_the_byte_budget() {
        let repeated_row = row(json!({
            "id": 1,
            "user_id": 1,
            "selected": true,
            "payload": "x".repeat(128 * 1024),
        }));
        let row_bytes = owned_row_bytes(&repeated_row).unwrap();
        let accepted_rows = MAX_QUERY_RESULT_BYTES / row_bytes;
        assert!(accepted_rows * row_bytes <= MAX_QUERY_RESULT_BYTES);
        assert!((accepted_rows + 1) * row_bytes > MAX_QUERY_RESULT_BYTES);
        let plan = parse_sql("SELECT id FROM items ORDER BY id LIMIT 1", &[]).unwrap();

        let mut accepted = VisitorOnlyStorage::new(repeated_row.clone(), accepted_rows);
        accepted.add_payload_column();
        assert_eq!(execute(&accepted, &plan).unwrap().rows.len(), 1);
        assert_eq!(accepted.table_visits.get(), accepted_rows);

        let mut rejected = VisitorOnlyStorage::new(repeated_row, accepted_rows + 1);
        rejected.add_payload_column();
        assert_eq!(
            execute(&rejected, &plan).unwrap_err().code,
            "QUERY_WORK_LIMIT_EXCEEDED"
        );
        assert_eq!(rejected.table_visits.get(), accepted_rows + 1);
    }

    #[test]
    fn result_byte_budget_arithmetic_has_an_exact_checked_boundary() {
        ensure_result_budget(MAX_QUERY_RESULT_BYTES).unwrap();
        assert_eq!(
            ensure_result_budget(MAX_QUERY_RESULT_BYTES + 1)
                .unwrap_err()
                .code,
            "QUERY_WORK_LIMIT_EXCEEDED"
        );
        assert_eq!(
            checked_result_add(usize::MAX, 1).unwrap_err().code,
            "QUERY_WORK_LIMIT_EXCEEDED"
        );
        assert_eq!(
            checked_result_mul(usize::MAX, 2).unwrap_err().code,
            "QUERY_WORK_LIMIT_EXCEEDED"
        );
    }

    #[test]
    fn rejects_unorderable_values_before_sorting() {
        let mut database = Engine::default();
        database
            .define_table(TableSchema {
                name: "legacy".to_owned(),
                primary_key: vec!["id".to_owned()],
                columns: vec![],
            })
            .unwrap();
        database
            .replace_table(
                "legacy",
                vec![
                    row(json!({"id": 1, "value": "one"})),
                    row(json!({"id": 2, "value": 2})),
                ],
            )
            .unwrap();
        assert_eq!(
            database
                .query_sql("SELECT id FROM legacy ORDER BY value", &[])
                .unwrap_err()
                .code,
            "TYPE_MISMATCH"
        );

        let mut database = Engine::default();
        database
            .define_table(TableSchema {
                name: "documents".to_owned(),
                primary_key: vec!["id".to_owned()],
                columns: vec![
                    ColumnDefinition {
                        name: "id".to_owned(),
                        data_type: ColumnType::Integer,
                        nullable: false,
                        default: None,
                    },
                    ColumnDefinition {
                        name: "payload".to_owned(),
                        data_type: ColumnType::Json,
                        nullable: true,
                        default: None,
                    },
                ],
            })
            .unwrap();
        assert_eq!(
            database
                .query_sql("SELECT id FROM documents ORDER BY payload", &[])
                .unwrap_err()
                .code,
            "TYPE_MISMATCH"
        );
    }

    #[test]
    fn rejects_incompatible_comparisons_and_bounds_predicate_complexity() {
        assert_eq!(
            engine()
                .query_sql("SELECT * FROM posts WHERE id > '1'", &[])
                .unwrap_err()
                .code,
            "TYPE_MISMATCH"
        );

        let deep = format!(
            "SELECT * FROM posts WHERE {}id = 1{}",
            "(".repeat(MAX_PREDICATE_DEPTH + 1),
            ")".repeat(MAX_PREDICATE_DEPTH + 1),
        );
        assert_eq!(parse_sql(&deep, &[]).unwrap_err().code, "INVALID_QUERY");

        let too_many = format!(
            "SELECT * FROM posts WHERE id IN ({})",
            vec!["1"; MAX_IN_VALUES + 1].join(",")
        );
        assert_eq!(parse_sql(&too_many, &[]).unwrap_err().code, "INVALID_QUERY");
    }

    #[test]
    fn folds_unquoted_identifiers_but_preserves_quoted_ones() {
        assert_eq!(
            parse_sql("SELECT ID FROM POSTS", &[]).unwrap(),
            QueryPlan {
                table: "posts".to_owned(),
                columns: Some(vec!["id".to_owned()]),
                filters: vec![],
                predicate: None,
                order_by: vec![],
                limit: None,
                offset: 0,
            }
        );
        assert_eq!(
            parse_sql("SELECT \"ID\" FROM \"Posts\"", &[]).unwrap(),
            QueryPlan {
                table: "Posts".to_owned(),
                columns: Some(vec!["ID".to_owned()]),
                filters: vec![],
                predicate: None,
                order_by: vec![],
                limit: None,
                offset: 0,
            }
        );
    }

    #[test]
    fn parses_supported_literals_qualified_names_and_comments() {
        assert_eq!(
            parse_sql(
                "/* snapshot */ SELECT \"display\"\"name\" FROM PUBLIC.Posts \
                 WHERE title = 'James''s post' AND published = TRUE \
                 AND rating = -4.5 AND removed = NULL LIMIT $1; -- done",
                &[json!(10)],
            )
            .unwrap(),
            QueryPlan {
                table: "public.posts".to_owned(),
                columns: Some(vec!["display\"name".to_owned()]),
                filters: vec![],
                predicate: Some(Predicate::And {
                    predicates: vec![
                        Predicate::Comparison {
                            column: "title".to_owned(),
                            operator: FilterOperator::Eq,
                            value: json!("James's post"),
                        },
                        Predicate::Comparison {
                            column: "published".to_owned(),
                            operator: FilterOperator::Eq,
                            value: json!(true),
                        },
                        Predicate::Comparison {
                            column: "rating".to_owned(),
                            operator: FilterOperator::Eq,
                            value: json!(-4.5),
                        },
                        Predicate::Comparison {
                            column: "removed".to_owned(),
                            operator: FilterOperator::Eq,
                            value: Value::Null,
                        },
                    ],
                }),
                order_by: vec![],
                limit: Some(10),
                offset: 0,
            }
        );
    }

    #[test]
    fn rejects_malformed_literals_identifiers_and_comments_as_parse_errors() {
        for sql in [
            "SELECT \"title FROM posts",
            "SELECT * FROM posts WHERE title = 'unfinished",
            "SELECT * FROM posts /* unfinished",
        ] {
            let error = parse_sql(sql, &[]).unwrap_err();
            assert_eq!(error.code, "SQL_PARSE_ERROR", "query was `{sql}`");
        }
    }

    #[test]
    fn rejects_reserved_words_as_unquoted_identifiers() {
        for sql in ["SELECT from FROM posts", "SELECT select FROM posts"] {
            let error = parse_sql(sql, &[]).unwrap_err();
            assert_eq!(error.code, "SQL_PARSE_ERROR", "query was `{sql}`");
        }

        assert_eq!(
            parse_sql("SELECT \"from\", \"select\" FROM posts", &[])
                .unwrap()
                .columns,
            Some(vec!["from".to_owned(), "select".to_owned()])
        );
    }

    #[test]
    fn bounds_sql_input_and_parser_complexity() {
        let too_long = "x".repeat(MAX_SQL_BYTES + 1);
        assert_eq!(parse_sql(&too_long, &[]).unwrap_err().code, "INVALID_QUERY");

        let too_many_params = vec![Value::Null; MAX_PARAMETERS + 1];
        assert_eq!(
            parse_sql("SELECT * FROM posts", &too_many_params)
                .unwrap_err()
                .code,
            "INVALID_QUERY"
        );

        let too_many_tokens = format!("SELECT * FROM posts {}", "unused ".repeat(MAX_SQL_TOKENS));
        assert_eq!(
            parse_sql(&too_many_tokens, &[]).unwrap_err().code,
            "INVALID_QUERY"
        );

        let too_many_filters = format!(
            "SELECT * FROM posts WHERE {}",
            vec!["id = 1"; MAX_FILTERS + 1].join(" AND ")
        );
        assert_eq!(
            parse_sql(&too_many_filters, &[]).unwrap_err().code,
            "INVALID_QUERY"
        );

        let deeply_nested_comment = format!(
            "{}{} SELECT * FROM posts",
            "/*".repeat(MAX_COMMENT_DEPTH + 1),
            "*/".repeat(MAX_COMMENT_DEPTH + 1)
        );
        assert_eq!(
            parse_sql(&deeply_nested_comment, &[]).unwrap_err().code,
            "INVALID_QUERY"
        );
    }

    #[test]
    fn applies_complexity_caps_to_structured_query_plans_too() {
        let mut plan = QueryPlan {
            table: "posts".to_owned(),
            columns: Some(vec!["id".to_owned(); MAX_PROJECTION_COLUMNS + 1]),
            filters: vec![],
            predicate: None,
            order_by: vec![],
            limit: None,
            offset: 0,
        };
        assert_eq!(engine().query(&plan).unwrap_err().code, "INVALID_QUERY");

        plan.columns = None;
        plan.filters = vec![
            Filter {
                column: "id".to_owned(),
                operator: FilterOperator::Eq,
                value: json!(1),
            };
            MAX_FILTERS + 1
        ];
        assert_eq!(engine().query(&plan).unwrap_err().code, "INVALID_QUERY");
    }

    #[test]
    fn null_equality_never_matches() {
        let result = engine()
            .query_sql("SELECT * FROM posts WHERE deleted = NULL", &[])
            .unwrap();
        assert!(result.rows.is_empty());
    }

    #[test]
    fn rejects_missing_bind_values() {
        let error = engine()
            .query_sql("SELECT * FROM posts WHERE user_id = $1", &[])
            .unwrap_err();
        assert_eq!(error.code, "BIND_ERROR");
    }

    #[test]
    fn rejects_unknown_columns() {
        let error = engine()
            .query_sql("SELECT missing FROM posts", &[])
            .unwrap_err();
        assert_eq!(error.code, "COLUMN_NOT_FOUND");
    }

    #[test]
    fn rejects_every_unimplemented_query_shape() {
        for sql in [
            "SELECT * FROM posts JOIN users ON posts.user_id = users.id",
            "SELECT id + 1 FROM posts",
            "SELECT * FROM posts AS p",
            "SELECT DISTINCT * FROM posts",
            "SELECT * FROM posts; SELECT * FROM posts",
            "UPDATE posts SET title = 'no'",
        ] {
            let error = engine().query_sql(sql, &[]).unwrap_err();
            assert_eq!(error.code, "UNSUPPORTED_SQL", "query was `{sql}`");
        }
    }
}
