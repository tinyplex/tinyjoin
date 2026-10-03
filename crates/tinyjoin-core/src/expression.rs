//! Scalar expressions over one row: arithmetic and text concatenation of its columns, literals,
//! and parameters, which `UPDATE ... SET` and `INSERT ... ON CONFLICT DO UPDATE SET` assign.

use std::mem::discriminant;

use serde_json::Value;

use crate::query::{Token, bind_parameter, bound_value, is_reserved_keyword, number_literal};
use crate::storage::{MAX_LOGICAL_VALUE_BYTES, column_type_name};
use crate::{ColumnDefinition, ColumnType, EngineError, Result};

const MAX_EXPRESSION_NODES: usize = 256;
const MAX_EXPRESSION_DEPTH: usize = 32;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Expression {
    /// A literal, or a parameter's value.
    Value(Value),
    /// A column of the row being written, as it was before the statement.
    Column(String),
    /// `EXCLUDED.column`: a column of the row an `INSERT ... ON CONFLICT` proposed.
    Excluded(String),
    Negate(Box<Expression>),
    Binary(Operator, Box<Expression>, Box<Expression>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Operator {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    Concatenate,
}

impl Operator {
    fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Subtract => "-",
            Self::Multiply => "*",
            Self::Divide => "/",
            Self::Remainder => "%",
            Self::Concatenate => "||",
        }
    }
}

/// How an expression names columns.
#[derive(Clone, Copy)]
pub(crate) enum Columns<'a> {
    /// A plain name is a column of the row. A statement over one table has dropped its table's
    /// qualifier before it is parsed, so any other qualified name is left for the row to refuse.
    Row,
    /// In `ON CONFLICT DO UPDATE`, the stored row is named behind the table's qualifier, given
    /// here, and the proposed row behind `EXCLUDED`. A plain name could be either, so, as in
    /// PostgreSQL, it is refused.
    Conflict(&'a str),
}

impl Expression {
    /// Whether the expression reads a row, rather than giving one value for every row.
    pub(crate) fn reads_row(&self) -> bool {
        match self {
            Self::Value(_) => false,
            Self::Column(_) | Self::Excluded(_) => true,
            Self::Negate(operand) => operand.reads_row(),
            Self::Binary(_, left, right) => left.reads_row() || right.reads_row(),
        }
    }
}

/// Parses the expression at `position`, advancing past it. `+` and `-` bind more loosely than
/// `*`, `/` and `%`, and `||` more loosely than both, as in PostgreSQL.
pub(crate) fn parse_expression_at(
    tokens: &[Token],
    position: &mut usize,
    params: &[Value],
    columns: Columns<'_>,
) -> Result<Expression> {
    let mut parser = Parser {
        tokens,
        position: *position,
        params,
        columns,
        nodes: 0,
    };
    let expression = parser.concatenation(0)?;
    *position = parser.position;
    Ok(expression)
}

struct Parser<'a> {
    tokens: &'a [Token],
    position: usize,
    params: &'a [Value],
    columns: Columns<'a>,
    nodes: usize,
}

impl Parser<'_> {
    fn node(&mut self, expression: Expression) -> Result<Expression> {
        self.nodes += 1;
        if self.nodes > MAX_EXPRESSION_NODES {
            return Err(EngineError::invalid_query(format!(
                "An expression cannot contain more than {MAX_EXPRESSION_NODES} terms"
            )));
        }
        Ok(expression)
    }

    fn binary(
        &mut self,
        operator: Operator,
        left: Expression,
        right: Expression,
    ) -> Result<Expression> {
        self.node(Expression::Binary(
            operator,
            Box::new(left),
            Box::new(right),
        ))
    }

    fn concatenation(&mut self, depth: usize) -> Result<Expression> {
        let mut left = self.additive(depth)?;
        while self.consume(&Token::Concat) {
            let right = self.additive(depth)?;
            left = self.binary(Operator::Concatenate, left, right)?;
        }
        Ok(left)
    }

    fn additive(&mut self, depth: usize) -> Result<Expression> {
        let mut left = self.multiplicative(depth)?;
        let tokens = self.tokens;
        loop {
            let (operator, right) = match tokens.get(self.position) {
                Some(Token::Plus) => {
                    self.position += 1;
                    (Operator::Add, self.multiplicative(depth)?)
                }
                Some(Token::Minus) => {
                    self.position += 1;
                    (Operator::Subtract, self.multiplicative(depth)?)
                }
                // The lexer reads `a -1` as `a` and the number `-1`, which here subtracts `1`.
                Some(Token::Number(number)) if number.starts_with('-') => {
                    self.position += 1;
                    let first = Expression::Value(self.number(&number[1..])?);
                    let first = self.node(first)?;
                    (Operator::Subtract, self.multiplicative_from(first, depth)?)
                }
                _ => return Ok(left),
            };
            left = self.binary(operator, left, right)?;
        }
    }

    fn multiplicative(&mut self, depth: usize) -> Result<Expression> {
        let first = self.unary(depth)?;
        self.multiplicative_from(first, depth)
    }

    fn multiplicative_from(&mut self, mut left: Expression, depth: usize) -> Result<Expression> {
        loop {
            let operator = match self.tokens.get(self.position) {
                Some(Token::Star) => Operator::Multiply,
                Some(Token::Slash) => Operator::Divide,
                Some(Token::Percent) => Operator::Remainder,
                _ => return Ok(left),
            };
            self.position += 1;
            let right = self.unary(depth)?;
            left = self.binary(operator, left, right)?;
        }
    }

    fn unary(&mut self, depth: usize) -> Result<Expression> {
        if depth > MAX_EXPRESSION_DEPTH {
            return Err(EngineError::invalid_query(format!(
                "An expression cannot nest more than {MAX_EXPRESSION_DEPTH} levels deep"
            )));
        }
        if self.consume(&Token::Minus) {
            let operand = self.unary(depth + 1)?;
            return self.node(Expression::Negate(Box::new(operand)));
        }
        let tokens = self.tokens;
        let Some(token) = tokens.get(self.position) else {
            return Err(EngineError::parse_error("Expected a SQL value"));
        };
        self.position += 1;
        let expression = match token {
            Token::LParen => {
                let inner = self.concatenation(depth + 1)?;
                if !self.consume(&Token::RParen) {
                    return Err(EngineError::parse_error("Expected `)` after an expression"));
                }
                return Ok(inner);
            }
            Token::String(value) => Expression::Value(Value::String(value.clone())),
            Token::Number(value) => Expression::Value(self.number(value)?),
            Token::Placeholder(index) => Expression::Value(bind_parameter(index, self.params)?),
            Token::Identifier {
                value,
                quoted: false,
            } if value.eq_ignore_ascii_case("null") => Expression::Value(Value::Null),
            Token::Identifier {
                value,
                quoted: false,
            } if value.eq_ignore_ascii_case("true") => Expression::Value(Value::Bool(true)),
            Token::Identifier {
                value,
                quoted: false,
            } if value.eq_ignore_ascii_case("false") => Expression::Value(Value::Bool(false)),
            Token::Identifier { .. } => {
                self.position -= 1;
                self.column()?
            }
            _ => {
                return Err(EngineError::unsupported_sql(
                    "An expression supports columns, literals, parameters, parentheses, unary \
                     `-`, and the operators `+`, `-`, `*`, `/`, `%`, and `||`",
                ));
            }
        };
        self.node(expression)
    }

    fn column(&mut self) -> Result<Expression> {
        let first = self.identifier()?;
        if matches!(self.tokens.get(self.position), Some(Token::LParen)) {
            return Err(EngineError::unsupported_sql(format!(
                "Function `{first}` is not supported in an expression"
            )));
        }
        if !self.consume(&Token::Dot) {
            return match self.columns {
                Columns::Row => Ok(Expression::Column(first)),
                Columns::Conflict(_) => Err(EngineError::invalid_query(format!(
                    "Column `{first}` is ambiguous in ON CONFLICT DO UPDATE; write \
                     `EXCLUDED.{first}` for the proposed row, or qualify it with the table's name \
                     for the stored row"
                ))),
            };
        }
        let second = self.identifier()?;
        if matches!(self.tokens.get(self.position), Some(Token::Dot)) {
            return Err(EngineError::unsupported_sql(
                "Columns can contain at most one table qualifier",
            ));
        }
        Ok(match self.columns {
            Columns::Conflict(_) if first == "excluded" => Expression::Excluded(second),
            Columns::Conflict(table) if first == table => Expression::Column(second),
            _ => Expression::Column(format!("{first}.{second}")),
        })
    }

    fn identifier(&mut self) -> Result<String> {
        let Some(Token::Identifier { value, quoted }) = self.tokens.get(self.position) else {
            return Err(EngineError::parse_error("Expected a SQL identifier"));
        };
        if value.is_empty() || (!quoted && is_reserved_keyword(value)) {
            return Err(EngineError::parse_error("Expected a valid SQL identifier"));
        }
        self.position += 1;
        Ok(if *quoted {
            value.clone()
        } else {
            value.to_ascii_lowercase()
        })
    }

    fn number(&self, text: &str) -> Result<Value> {
        number_literal(text)
            .map(Value::Number)
            .ok_or_else(|| EngineError::invalid_query(format!("Invalid number literal `{text}`")))
    }

    /// Consumes a token of `token`'s kind, which is one that holds no text.
    fn consume(&mut self, token: &Token) -> bool {
        let found = self
            .tokens
            .get(self.position)
            .is_some_and(|next| discriminant(next) == discriminant(token));
        if found {
            self.position += 1;
        }
        found
    }
}

/// The type of the values `expression` gives, with each column's from `column_type`, or `None`
/// where it gives only NULL. An operand of a type its operator does not take is refused here,
/// before any row is read: arithmetic takes integers and floats, and `||` takes text.
pub(crate) fn value_type(
    expression: &Expression,
    column_type: &dyn Fn(&str) -> Result<ColumnType>,
) -> Result<Option<ColumnType>> {
    Ok(match expression {
        Expression::Value(value) => match value {
            Value::Null => None,
            Value::Bool(_) => Some(ColumnType::Boolean),
            Value::Number(number) if number.is_f64() => Some(ColumnType::Float),
            Value::Number(_) => Some(ColumnType::Integer),
            Value::String(_) => Some(ColumnType::Text),
            Value::Array(_) | Value::Object(_) => Some(ColumnType::Json),
        },
        Expression::Column(column) | Expression::Excluded(column) => Some(column_type(column)?),
        Expression::Negate(operand) => {
            operand_types(Operator::Subtract, value_type(operand, column_type)?, None)?
        }
        Expression::Binary(operator, left, right) => operand_types(
            *operator,
            value_type(left, column_type)?,
            value_type(right, column_type)?,
        )?,
    })
}

fn operand_types(
    operator: Operator,
    left: Option<ColumnType>,
    right: Option<ColumnType>,
) -> Result<Option<ColumnType>> {
    // As for PostgreSQL's double precision, a float has no remainder.
    let takes = |operand: Option<ColumnType>| match operator {
        Operator::Concatenate => matches!(operand, None | Some(ColumnType::Text)),
        Operator::Remainder => matches!(operand, None | Some(ColumnType::Integer)),
        _ => matches!(
            operand,
            None | Some(ColumnType::Integer | ColumnType::Float)
        ),
    };
    for operand in [left, right] {
        if !takes(operand) {
            return Err(EngineError::type_mismatch(format!(
                "Operator `{}` cannot take {} values",
                operator.symbol(),
                column_type_name(operand.expect("NULL fits any operator"))
            )));
        }
    }
    Ok(match (left, right) {
        (Some(ColumnType::Float), _) | (_, Some(ColumnType::Float)) => Some(ColumnType::Float),
        (left, right) => left.or(right),
    })
}

/// Refuses an expression that reads a row when its values cannot be assigned to `target`, before
/// any row is read. A value it gives is still checked against `target` as it is written.
pub(crate) fn check_assignment(
    expression: &Expression,
    target: &ColumnDefinition,
    table: &str,
    column_type: &dyn Fn(&str) -> Result<ColumnType>,
) -> Result<()> {
    let fits = match value_type(expression, column_type)? {
        None => true,
        Some(given) => match target.data_type {
            ColumnType::Json => true,
            ColumnType::Float => matches!(given, ColumnType::Integer | ColumnType::Float),
            expected => given == expected,
        },
    };
    if fits {
        Ok(())
    } else {
        Err(EngineError::type_mismatch(format!(
            "Column `{}` in `{table}` expects {}",
            target.name,
            column_type_name(target.data_type)
        )))
    }
}

/// The one value an expression that reads no row gives, refusing operands of the wrong type first.
pub(crate) fn constant(expression: &Expression) -> Result<Value> {
    let unread = || EngineError::new("INTERNAL_ERROR", "A constant expression read a row");
    value_type(expression, &|_| Err(unread()))?;
    evaluate(expression, &mut |_, _| Err(unread()))
}

/// The value `expression` gives, reading each column it names through `column`, which is told
/// whether the name is `EXCLUDED`'s.
pub(crate) fn evaluate(
    expression: &Expression,
    column: &mut dyn FnMut(&str, bool) -> Result<Value>,
) -> Result<Value> {
    match expression {
        Expression::Value(value) => Ok(value.clone()),
        Expression::Column(name) => column(name, false),
        Expression::Excluded(name) => column(name, true),
        Expression::Negate(operand) => apply(
            Operator::Subtract,
            Value::Number(0.into()),
            evaluate(operand, column)?,
        ),
        Expression::Binary(operator, left, right) => {
            let left = evaluate(left, column)?;
            apply(*operator, left, evaluate(right, column)?)
        }
    }
}

/// An operator applied to two values. NULL gives NULL. Integers stay integers, within the
/// JavaScript-safe range, and division truncates them toward zero; any float makes the result a
/// float, which must be finite, and which `%` does not take.
fn apply(operator: Operator, left: Value, right: Value) -> Result<Value> {
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    let mismatch = || {
        EngineError::type_mismatch(format!(
            "Operator `{}` cannot take these values",
            operator.symbol()
        ))
    };
    if operator == Operator::Concatenate {
        let (Value::String(mut left), Value::String(right)) = (left, right) else {
            return Err(mismatch());
        };
        if left.len() + right.len() > MAX_LOGICAL_VALUE_BYTES {
            return Err(EngineError::new(
                "RESOURCE_LIMIT",
                format!("A concatenated value cannot exceed {MAX_LOGICAL_VALUE_BYTES} bytes"),
            ));
        }
        left.push_str(&right);
        return Ok(Value::String(left));
    }
    let (Value::Number(left), Value::Number(right)) = (left, right) else {
        return Err(mismatch());
    };
    let divides = matches!(operator, Operator::Divide | Operator::Remainder);
    if let (Some(left), Some(right)) = (left.as_i64(), right.as_i64()) {
        if divides && right == 0 {
            return Err(division_by_zero());
        }
        let result = match operator {
            Operator::Add => left.checked_add(right),
            Operator::Subtract => left.checked_sub(right),
            Operator::Multiply => left.checked_mul(right),
            Operator::Divide => left.checked_div(right),
            // Concatenation returned above.
            Operator::Remainder | Operator::Concatenate => left.checked_rem(right),
        };
        return match result {
            Some(result) if result.unsigned_abs() <= MAX_SAFE_INTEGER => {
                Ok(Value::Number(result.into()))
            }
            _ => Err(EngineError::new(
                "NUMERIC_OVERFLOW",
                "Arithmetic exceeds TinyJoin's safe-integer range",
            )),
        };
    }
    let (Some(left), Some(right)) = (left.as_f64(), right.as_f64()) else {
        return Err(mismatch());
    };
    if divides && right == 0.0 {
        return Err(division_by_zero());
    }
    let result = match operator {
        Operator::Add => left + right,
        Operator::Subtract => left - right,
        Operator::Multiply => left * right,
        Operator::Divide => left / right,
        Operator::Remainder | Operator::Concatenate => return Err(mismatch()),
    };
    serde_json::Number::from_f64(result)
        .map(Value::Number)
        .ok_or_else(|| {
            EngineError::new(
                "NUMERIC_OVERFLOW",
                "Arithmetic produced a non-finite result",
            )
        })
}

fn division_by_zero() -> EngineError {
    EngineError::new("DIVISION_BY_ZERO", "Division by zero")
}

/// Binds the parameters of a copy of a prepared expression in place.
pub(crate) fn bind_expression(expression: &mut Expression, params: &[Value]) -> Result<()> {
    match expression {
        Expression::Value(value) => *value = bound_value(value, params)?,
        Expression::Column(_) | Expression::Excluded(_) => {}
        Expression::Negate(operand) => bind_expression(operand, params)?,
        Expression::Binary(_, left, right) => {
            bind_expression(left, params)?;
            bind_expression(right, params)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::query::tokenize;

    /// The value an expression gives against a row of `n = 7`, `price = 2.5`, `name = 'ann'`,
    /// `missing = NULL`, and `doc` a JSON column, or the error code it fails with.
    fn value(sql: &str) -> std::result::Result<Value, String> {
        let row = json!({"n": 7, "price": 2.5, "name": "ann", "missing": null, "doc": {"a": 1}});
        let column_type = |column: &str| {
            Ok(match column {
                "n" => ColumnType::Integer,
                "price" => ColumnType::Float,
                "name" | "missing" => ColumnType::Text,
                "doc" => ColumnType::Json,
                _ => return Err(EngineError::column_not_found(column, "t")),
            })
        };
        let run = || {
            let tokens = tokenize(sql)?;
            let mut position = 0;
            let expression =
                parse_expression_at(&tokens, &mut position, &[json!(3)], Columns::Row)?;
            assert_eq!(position, tokens.len(), "{sql} was not read to its end");
            value_type(&expression, &column_type)?;
            evaluate(&expression, &mut |column, _| Ok(row[column].clone()))
        };
        run().map_err(|error| error.code)
    }

    #[test]
    fn operators_bind_and_associate_as_in_postgresql() {
        for (sql, expected) in [
            ("1 + 2 * 3", json!(7)),
            ("(1 + 2) * 3", json!(9)),
            ("10 - 2 - 3", json!(5)),
            ("10 -2", json!(8)),
            ("10-2*3", json!(4)),
            ("2 * -3", json!(-6)),
            ("- -3", json!(3)),
            ("-n + $1", json!(-4)),
            ("n % 4 * 2", json!(6)),
            ("'to ' || name || '!'", json!("to ann!")),
            ("name || ' and ' || name", json!("ann and ann")),
        ] {
            assert_eq!(value(sql), Ok(expected), "{sql}");
        }
    }

    #[test]
    fn integers_stay_integers_and_floats_must_be_finite() {
        for (sql, expected) in [
            ("7 / 2", json!(3)),
            ("-7 / 2", json!(-3)),
            ("7 % 3", json!(1)),
            ("-7 % 3", json!(-1)),
            ("7.0 / 2", json!(3.5)),
            ("n / 2.0", json!(3.5)),
            ("price * 2", json!(5.0)),
            ("9007199254740990 + 1", json!(9_007_199_254_740_991_i64)),
            ("-9007199254740991 - 0", json!(-9_007_199_254_740_991_i64)),
        ] {
            assert_eq!(value(sql), Ok(expected), "{sql}");
        }
        for (sql, code) in [
            ("9007199254740991 + 1", "NUMERIC_OVERFLOW"),
            ("-9007199254740991 - 1", "NUMERIC_OVERFLOW"),
            ("4503599627370496 * 2", "NUMERIC_OVERFLOW"),
            ("1e308 * 10", "NUMERIC_OVERFLOW"),
            ("n / 0", "DIVISION_BY_ZERO"),
            ("n % 0", "DIVISION_BY_ZERO"),
            ("price / 0.0", "DIVISION_BY_ZERO"),
            ("price / 0", "DIVISION_BY_ZERO"),
        ] {
            assert_eq!(value(sql), Err(code.to_owned()), "{sql}");
        }
    }

    #[test]
    fn null_gives_null_and_operands_must_fit_their_operators() {
        for sql in ["NULL + 1", "missing || 'x'", "-(NULL)", "n * NULL / 0"] {
            assert_eq!(value(sql), Ok(Value::Null), "{sql}");
        }
        for (sql, code) in [
            ("name + 1", "TYPE_MISMATCH"),
            ("'1' + 1", "TYPE_MISMATCH"),
            ("TRUE * 2", "TYPE_MISMATCH"),
            ("doc + 1", "TYPE_MISMATCH"),
            ("n || 'x'", "TYPE_MISMATCH"),
            ("price % 1", "TYPE_MISMATCH"),
            ("n % 2.0", "TYPE_MISMATCH"),
            ("-name", "TYPE_MISMATCH"),
            // Operand types are checked before anything is worked out.
            ("1 / 0 + name", "TYPE_MISMATCH"),
            ("other + 1", "COLUMN_NOT_FOUND"),
            ("t.n + 1", "COLUMN_NOT_FOUND"),
            ("lower(name)", "UNSUPPORTED_SQL"),
            ("(n + 1", "SQL_PARSE_ERROR"),
            ("n + ", "SQL_PARSE_ERROR"),
            ("a.b.c", "UNSUPPORTED_SQL"),
            ("n + $2", "BIND_ERROR"),
        ] {
            assert_eq!(value(sql), Err(code.to_owned()), "{sql}");
        }
    }

    #[test]
    fn expressions_are_bounded_in_size_and_depth() {
        let terms = vec!["1"; MAX_EXPRESSION_NODES / 2].join(" + ");
        assert!(value(&terms).is_ok());
        let terms = vec!["1"; MAX_EXPRESSION_NODES / 2 + 1].join(" + ");
        assert_eq!(value(&terms), Err("INVALID_QUERY".to_owned()));
        let nested = format!(
            "{}1{}",
            "(".repeat(MAX_EXPRESSION_DEPTH),
            ")".repeat(MAX_EXPRESSION_DEPTH)
        );
        assert_eq!(value(&nested), Ok(json!(1)));
        let nested = format!("({nested})");
        assert_eq!(value(&nested), Err("INVALID_QUERY".to_owned()));
    }

    #[test]
    fn a_conflict_assignment_names_the_stored_and_proposed_rows() {
        let parse = |sql: &str| {
            let tokens = tokenize(sql).unwrap();
            parse_expression_at(&tokens, &mut 0, &[], Columns::Conflict("kv"))
        };
        assert_eq!(
            parse("kv.n + EXCLUDED.n").unwrap(),
            Expression::Binary(
                Operator::Add,
                Box::new(Expression::Column("n".to_owned())),
                Box::new(Expression::Excluded("n".to_owned()))
            )
        );
        assert_eq!(
            parse("\"excluded\".n").unwrap(),
            Expression::Excluded("n".to_owned())
        );
        assert_eq!(parse("n + 1").unwrap_err().code, "INVALID_QUERY");
        assert_eq!(
            parse("other.n").unwrap(),
            Expression::Column("other.n".to_owned())
        );
    }
}
