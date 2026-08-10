use std::str::FromStr;

use serde_json::{Map, Number, Value};

use crate::{
    EngineError, Filter, FilterOperator, QueryPlan, QueryResult, Result, Row, StorageDriver,
};

const MAX_SQL_BYTES: usize = 64 * 1024;
const MAX_SQL_TOKENS: usize = 4 * 1024;
const MAX_PROJECTION_COLUMNS: usize = 256;
const MAX_FILTERS: usize = 256;
const MAX_PARAMETERS: usize = 1024;
const MAX_COMMENT_DEPTH: usize = 32;

pub(crate) fn execute<S: StorageDriver>(storage: &S, plan: &QueryPlan) -> Result<QueryResult> {
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

    let table_rows = storage.scan_table(&plan.table)?;
    if plan.limit == Some(0) {
        return Ok(QueryResult {
            revision: storage.revision(),
            rows: Vec::new(),
        });
    }

    let mut rows = Vec::new();
    for row in table_rows {
        if !matches_filters(&row, &plan.filters, &plan.table)? {
            continue;
        }
        rows.push(project_row(row, plan.columns.as_deref(), &plan.table)?);
        if plan.limit.is_some_and(|limit| rows.len() >= limit) {
            break;
        }
    }

    Ok(QueryResult {
        revision: storage.revision(),
        rows,
    })
}

pub(crate) fn parse_sql(sql: &str, params: &[Value]) -> Result<QueryPlan> {
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

    SqlParser::new(tokenize(sql)?, params).parse()
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Token {
    Identifier { value: String, quoted: bool },
    String(String),
    Number(String),
    Placeholder(String),
    Star,
    Comma,
    Dot,
    Eq,
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
        let filters = if self.consume_keyword("where") {
            self.parse_filters()?
        } else {
            Vec::new()
        };
        let limit = if self.consume_keyword("limit") {
            Some(self.parse_limit()?)
        } else {
            None
        };

        self.consume(TokenMatcher::Semicolon);
        if !self.is_done() {
            return Err(unsupported_shape());
        }

        Ok(QueryPlan {
            table,
            columns,
            filters,
            limit,
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

    fn parse_filters(&mut self) -> Result<Vec<Filter>> {
        let mut filters = Vec::new();
        loop {
            if filters.len() >= MAX_FILTERS {
                return Err(EngineError::invalid_query(format!(
                    "A query cannot contain more than {MAX_FILTERS} filters"
                )));
            }
            let column = self.parse_identifier()?;
            if !self.consume(TokenMatcher::Eq) {
                return Err(unsupported_shape());
            }
            filters.push(Filter {
                column,
                operator: FilterOperator::Eq,
                value: self.parse_value()?,
            });
            if !self.consume_keyword("and") {
                break;
            }
        }
        Ok(filters)
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
    Semicolon,
}

fn tokenize(sql: &str) -> Result<Vec<Token>> {
    Lexer::new(sql).tokenize()
}

fn is_identifier_start(character: char) -> bool {
    character == '_' || character.is_ascii_alphabetic() || !character.is_ascii()
}

fn is_identifier_continue(character: char) -> bool {
    is_identifier_start(character) || character.is_ascii_digit() || character == '$'
}

fn is_reserved_keyword(identifier: &str) -> bool {
    [
        "select", "from", "where", "and", "limit", "null", "true", "false",
    ]
    .iter()
    .any(|keyword| identifier.eq_ignore_ascii_case(keyword))
}

fn bind_parameter(index: &str, params: &[Value]) -> Result<Value> {
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

fn matches_filters(row: &Row, filters: &[Filter], table: &str) -> Result<bool> {
    for filter in filters {
        let Some(actual) = row.get(&filter.column) else {
            return Err(EngineError::column_not_found(&filter.column, table));
        };
        let matched = match filter.operator {
            FilterOperator::Eq => {
                actual != &Value::Null && filter.value != Value::Null && actual == &filter.value
            }
        };
        if !matched {
            return Ok(false);
        }
    }
    Ok(true)
}

fn project_row(mut row: Row, columns: Option<&[String]>, table: &str) -> Result<Row> {
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
        "The initial SQL subset supports SELECT columns FROM one table, ANDed equality filters, and LIMIT",
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{Engine, InMemoryStorage, TableSchema};

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
        let result = engine()
            .query_sql("SELECT * FROM posts LIMIT 0", &[])
            .unwrap();

        assert_eq!(result.revision, 1);
        assert!(result.rows.is_empty());
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
    fn folds_unquoted_identifiers_but_preserves_quoted_ones() {
        assert_eq!(
            parse_sql("SELECT ID FROM POSTS", &[]).unwrap(),
            QueryPlan {
                table: "posts".to_owned(),
                columns: Some(vec!["id".to_owned()]),
                filters: vec![],
                limit: None,
            }
        );
        assert_eq!(
            parse_sql("SELECT \"ID\" FROM \"Posts\"", &[]).unwrap(),
            QueryPlan {
                table: "Posts".to_owned(),
                columns: Some(vec!["ID".to_owned()]),
                filters: vec![],
                limit: None,
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
                filters: vec![
                    Filter {
                        column: "title".to_owned(),
                        operator: FilterOperator::Eq,
                        value: json!("James's post"),
                    },
                    Filter {
                        column: "published".to_owned(),
                        operator: FilterOperator::Eq,
                        value: json!(true),
                    },
                    Filter {
                        column: "rating".to_owned(),
                        operator: FilterOperator::Eq,
                        value: json!(-4.5),
                    },
                    Filter {
                        column: "removed".to_owned(),
                        operator: FilterOperator::Eq,
                        value: Value::Null,
                    },
                ],
                limit: Some(10),
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
            limit: None,
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
            "SELECT * FROM posts ORDER BY id",
            "SELECT count(*) FROM posts",
            "SELECT * FROM posts JOIN users ON posts.user_id = users.id",
            "SELECT * FROM posts WHERE id > 1",
            "SELECT DISTINCT * FROM posts",
            "SELECT * FROM posts; SELECT * FROM posts",
            "UPDATE posts SET title = 'no'",
        ] {
            let error = engine().query_sql(sql, &[]).unwrap_err();
            assert_eq!(error.code, "UNSUPPORTED_SQL", "query was `{sql}`");
        }
    }
}
