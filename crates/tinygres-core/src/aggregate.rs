use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::str::FromStr;

use serde_json::{Map, Number, Value};

use crate::query::{
    Token, bind_parameter, is_reserved_keyword, matches_predicate, parse_predicate_at, tokenize,
    validate_predicate_columns, validate_predicate_types, validate_sql_input,
};
use crate::{
    ColumnDefinition, ColumnType, EngineError, NullOrder, OrderBy, OrderDirection, Predicate,
    QueryResult, Result, Row, StorageDriver, TableSchema,
};

const MAX_SELECT_ITEMS: usize = 256;
const MAX_AGGREGATES: usize = 64;
const MAX_GROUP_COLUMNS: usize = 32;
const MAX_ORDER_COLUMNS: usize = 32;
const MAX_GROUPS: usize = 100_000;
const MAX_AGGREGATE_CELLS: usize = 1_000_000;
const MAX_SAFE_INTEGER: i128 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AggregateFunction {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggregateFunction {
    fn name(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Sum => "sum",
            Self::Avg => "avg",
            Self::Min => "min",
            Self::Max => "max",
        }
    }
}

#[derive(Clone, Debug)]
enum AggregateArgument {
    Star,
    Column(String),
}

#[derive(Clone, Debug)]
enum SelectExpression {
    Column(String),
    Aggregate {
        function: AggregateFunction,
        argument: AggregateArgument,
    },
}

#[derive(Clone, Debug)]
struct SelectItem {
    expression: SelectExpression,
    output: String,
}

#[derive(Clone, Debug)]
pub(crate) struct AggregatePlan {
    table: String,
    items: Vec<SelectItem>,
    predicate: Option<Predicate>,
    group_by: Vec<String>,
    order_by: Vec<OrderBy>,
    limit: Option<usize>,
    offset: usize,
}

pub(crate) fn is_aggregate_select(tokens: &[Token]) -> bool {
    let mut before_from = true;
    for (index, token) in tokens.iter().enumerate() {
        let Token::Identifier { value, quoted } = token else {
            continue;
        };
        if !quoted && value.eq_ignore_ascii_case("from") {
            before_from = false;
            continue;
        }
        if !quoted
            && value.eq_ignore_ascii_case("group")
            && matches!(
                tokens.get(index + 1),
                Some(Token::Identifier {
                    value,
                    quoted: false,
                }) if value.eq_ignore_ascii_case("by")
            )
        {
            return true;
        }
        if before_from
            && !quoted
            && aggregate_function(value).is_some()
            && matches!(tokens.get(index + 1), Some(Token::LParen))
        {
            return true;
        }
    }
    false
}

pub(crate) fn parse_sql(sql: &str, params: &[Value]) -> Result<AggregatePlan> {
    validate_sql_input(sql, params)?;
    Parser::new(tokenize(sql)?, params).parse()
}

pub(crate) fn execute<S: StorageDriver>(storage: &S, plan: &AggregatePlan) -> Result<QueryResult> {
    let schema = storage.table_schema(&plan.table)?;
    validate_plan(plan, &schema)?;

    let mut matching = Vec::new();
    for row in storage.scan_table(&plan.table)? {
        if matches_predicate(&row, plan.predicate.as_ref(), &plan.table)? {
            matching.push(row);
        }
    }

    let grouped: Vec<Vec<Row>> = if plan.group_by.is_empty() {
        vec![matching]
    } else {
        let mut groups = BTreeMap::<String, Vec<Row>>::new();
        for row in matching {
            let key = group_key(&schema, &plan.group_by, &row)?;
            if !groups.contains_key(&key) && groups.len() >= MAX_GROUPS {
                return Err(EngineError::invalid_query(format!(
                    "A grouped query cannot produce more than {MAX_GROUPS} groups"
                )));
            }
            groups.entry(key).or_default().push(row);
        }
        groups.into_values().collect()
    };

    let aggregate_count = plan
        .items
        .iter()
        .filter(|item| matches!(item.expression, SelectExpression::Aggregate { .. }))
        .count();
    if grouped.len().saturating_mul(aggregate_count) > MAX_AGGREGATE_CELLS {
        return Err(EngineError::invalid_query(format!(
            "A grouped query cannot materialize more than {MAX_AGGREGATE_CELLS} aggregate cells"
        )));
    }

    let mut rows = grouped
        .iter()
        .map(|group| project_group(plan, &schema, group))
        .collect::<Result<Vec<_>>>()?;
    if !plan.order_by.is_empty() {
        sort_rows(&mut rows, &plan.order_by);
    }
    let rows = rows
        .into_iter()
        .skip(plan.offset)
        .take(plan.limit.unwrap_or(usize::MAX))
        .collect();

    Ok(QueryResult {
        revision: storage.revision(),
        rows,
    })
}

fn validate_plan(plan: &AggregatePlan, schema: &TableSchema) -> Result<()> {
    if schema.columns.is_empty() {
        return Err(EngineError::unsupported_sql(format!(
            "Aggregate queries require a typed table catalog for `{}`",
            plan.table
        )));
    }
    if let Some(predicate) = &plan.predicate {
        validate_predicate_columns(predicate, schema, &plan.table)?;
        validate_predicate_types(predicate, schema, &plan.table)?;
    }

    let mut groups = HashSet::new();
    for column in &plan.group_by {
        if !groups.insert(column.as_str()) {
            return Err(EngineError::invalid_query(format!(
                "GROUP BY names column `{column}` more than once"
            )));
        }
        let definition = column_definition(schema, column, &plan.table)?;
        if definition.data_type == ColumnType::Json {
            return Err(EngineError::type_mismatch(format!(
                "JSON column `{column}` in `{}` cannot be grouped",
                plan.table
            )));
        }
    }

    let mut outputs = HashSet::new();
    let mut aggregate_count = 0;
    for item in &plan.items {
        if !outputs.insert(item.output.as_str()) {
            return Err(EngineError::invalid_query(format!(
                "SELECT produces output column `{}` more than once; use distinct AS aliases",
                item.output
            )));
        }
        match &item.expression {
            SelectExpression::Column(column) => {
                column_definition(schema, column, &plan.table)?;
                if !groups.contains(column.as_str()) {
                    return Err(EngineError::invalid_query(format!(
                        "Column `{column}` must appear in GROUP BY or be used in an aggregate"
                    )));
                }
            }
            SelectExpression::Aggregate { function, argument } => {
                aggregate_count += 1;
                if aggregate_count > MAX_AGGREGATES {
                    return Err(EngineError::invalid_query(format!(
                        "A query cannot contain more than {MAX_AGGREGATES} aggregate calls"
                    )));
                }
                validate_aggregate(*function, argument, schema, &plan.table)?;
            }
        }
    }
    for order in &plan.order_by {
        if !outputs.contains(order.column.as_str()) {
            return Err(EngineError::column_not_found(
                &order.column,
                "aggregate output",
            ));
        }
    }
    Ok(())
}

fn validate_aggregate(
    function: AggregateFunction,
    argument: &AggregateArgument,
    schema: &TableSchema,
    table: &str,
) -> Result<()> {
    let AggregateArgument::Column(column) = argument else {
        return if function == AggregateFunction::Count {
            Ok(())
        } else {
            Err(EngineError::unsupported_sql(
                "Only COUNT accepts `*` as an aggregate argument",
            ))
        };
    };
    let definition = column_definition(schema, column, table)?;
    let supported = match function {
        AggregateFunction::Count => true,
        AggregateFunction::Sum | AggregateFunction::Avg => {
            matches!(
                definition.data_type,
                ColumnType::Integer | ColumnType::Float
            )
        }
        AggregateFunction::Min | AggregateFunction::Max => matches!(
            definition.data_type,
            ColumnType::Integer | ColumnType::Float | ColumnType::Text
        ),
    };
    if supported {
        Ok(())
    } else {
        Err(EngineError::type_mismatch(format!(
            "{} cannot aggregate {} column `{column}` in `{table}`",
            function.name(),
            column_type_name(definition.data_type)
        )))
    }
}

fn project_group(plan: &AggregatePlan, schema: &TableSchema, rows: &[Row]) -> Result<Row> {
    let mut projected = Map::new();
    for item in &plan.items {
        let value = match &item.expression {
            SelectExpression::Column(column) => rows
                .first()
                .and_then(|row| row.get(column))
                .cloned()
                .ok_or_else(|| {
                    EngineError::invalid_query(format!(
                        "Grouped column `{column}` has no representative row"
                    ))
                })?,
            SelectExpression::Aggregate { function, argument } => {
                aggregate_value(*function, argument, schema, rows)?
            }
        };
        projected.insert(item.output.clone(), value);
    }
    Ok(projected)
}

fn aggregate_value(
    function: AggregateFunction,
    argument: &AggregateArgument,
    schema: &TableSchema,
    rows: &[Row],
) -> Result<Value> {
    if function == AggregateFunction::Count {
        let count = match argument {
            AggregateArgument::Star => rows.len(),
            AggregateArgument::Column(column) => rows
                .iter()
                .filter(|row| row.get(column).is_some_and(|value| value != &Value::Null))
                .count(),
        };
        if (count as u128) > MAX_SAFE_INTEGER as u128 {
            return Err(EngineError::new(
                "NUMERIC_OVERFLOW",
                "COUNT exceeds TinyGres's safe-integer range",
            ));
        }
        return u64::try_from(count)
            .map(Number::from)
            .map(Value::Number)
            .map_err(|_| EngineError::new("NUMERIC_OVERFLOW", "COUNT result overflowed"));
    }

    let AggregateArgument::Column(column) = argument else {
        return Err(EngineError::unsupported_sql(
            "Only COUNT accepts `*` as an aggregate argument",
        ));
    };
    let definition = column_definition(schema, column, &schema.name)?;
    let values = rows
        .iter()
        .filter_map(|row| row.get(column))
        .filter(|value| *value != &Value::Null)
        .collect::<Vec<_>>();
    if values.is_empty() {
        return Ok(Value::Null);
    }

    match function {
        AggregateFunction::Count => unreachable!("COUNT returned above"),
        AggregateFunction::Sum => sum_values(definition, &values),
        AggregateFunction::Avg => average_values(definition, &values),
        AggregateFunction::Min | AggregateFunction::Max => {
            let mut selected = values[0];
            for value in &values[1..] {
                let ordering = compare_typed(definition.data_type, value, selected);
                let replace = if function == AggregateFunction::Min {
                    ordering == Ordering::Less
                } else {
                    ordering == Ordering::Greater
                };
                if replace {
                    selected = value;
                }
            }
            Ok(selected.clone())
        }
    }
}

fn sum_values(definition: &ColumnDefinition, values: &[&Value]) -> Result<Value> {
    match definition.data_type {
        ColumnType::Integer => {
            let mut sum = 0_i128;
            for value in values {
                sum = sum
                    .checked_add(integer_value(value)?)
                    .ok_or_else(|| EngineError::new("NUMERIC_OVERFLOW", "SUM result overflowed"))?;
            }
            if !(-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&sum) {
                return Err(EngineError::new(
                    "NUMERIC_OVERFLOW",
                    "SUM exceeds TinyGres's safe-integer range",
                ));
            }
            Ok(Value::Number(Number::from(sum as i64)))
        }
        ColumnType::Float => finite_number(
            values
                .iter()
                .map(|value| value.as_f64().expect("typed float was validated"))
                .sum(),
            "SUM",
        ),
        _ => Err(EngineError::type_mismatch("SUM requires a numeric column")),
    }
}

fn average_values(definition: &ColumnDefinition, values: &[&Value]) -> Result<Value> {
    let sum = match definition.data_type {
        ColumnType::Integer => values
            .iter()
            .map(|value| integer_value(value).map(|value| value as f64))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .sum::<f64>(),
        ColumnType::Float => values
            .iter()
            .map(|value| value.as_f64().expect("typed float was validated"))
            .sum::<f64>(),
        _ => return Err(EngineError::type_mismatch("AVG requires a numeric column")),
    };
    finite_number(sum / values.len() as f64, "AVG")
}

fn finite_number(value: f64, operation: &str) -> Result<Value> {
    Number::from_f64(value).map(Value::Number).ok_or_else(|| {
        EngineError::new(
            "NUMERIC_OVERFLOW",
            format!("{operation} produced a non-finite result"),
        )
    })
}

fn integer_value(value: &Value) -> Result<i128> {
    value
        .as_i64()
        .map(i128::from)
        .or_else(|| value.as_u64().map(i128::from))
        .ok_or_else(|| EngineError::type_mismatch("Expected a typed integer value"))
}

fn group_key(schema: &TableSchema, columns: &[String], row: &Row) -> Result<String> {
    let mut parts = Vec::with_capacity(columns.len());
    for column in columns {
        let definition = column_definition(schema, column, &schema.name)?;
        let value = row
            .get(column)
            .ok_or_else(|| EngineError::column_not_found(column, &schema.name))?;
        let part = match (definition.data_type, value) {
            (_, Value::Null) => "null".to_owned(),
            (ColumnType::Boolean, Value::Bool(value)) => format!("b:{value}"),
            (ColumnType::Integer, _) => format!("i:{}", integer_value(value)?),
            (ColumnType::Float, Value::Number(value)) => {
                let mut number = value.as_f64().expect("typed float was validated");
                if number == 0.0 {
                    number = 0.0;
                }
                format!("f:{:016x}", number.to_bits())
            }
            (ColumnType::Text, Value::String(value)) => {
                format!(
                    "s:{}",
                    serde_json::to_string(value).expect("strings encode")
                )
            }
            _ => {
                return Err(EngineError::type_mismatch(format!(
                    "Column `{column}` contains a value incompatible with its catalog type"
                )));
            }
        };
        parts.push(part);
    }
    serde_json::to_string(&parts)
        .map_err(|error| EngineError::invalid_query(format!("Could not encode group key: {error}")))
}

fn sort_rows(rows: &mut [Row], order_by: &[OrderBy]) {
    rows.sort_by(|left, right| {
        for order in order_by {
            let ordering = compare_order_values(
                left.get(&order.column).expect("order output was validated"),
                right
                    .get(&order.column)
                    .expect("order output was validated"),
                order,
            );
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    });
}

fn compare_order_values(left: &Value, right: &Value, order: &OrderBy) -> Ordering {
    let nulls_first = match order.nulls {
        NullOrder::First => true,
        NullOrder::Last => false,
        NullOrder::Default => order.direction == OrderDirection::Desc,
    };
    let ordering = match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => {
            if nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (_, Value::Null) => {
            if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Value::Number(left), Value::Number(right)) => left
            .as_f64()
            .partial_cmp(&right.as_f64())
            .unwrap_or(Ordering::Equal),
        (Value::String(left), Value::String(right)) => left.cmp(right),
        (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
        _ => Ordering::Equal,
    };
    if order.direction == OrderDirection::Desc && left != &Value::Null && right != &Value::Null {
        ordering.reverse()
    } else {
        ordering
    }
}

fn compare_typed(data_type: ColumnType, left: &Value, right: &Value) -> Ordering {
    match data_type {
        ColumnType::Integer | ColumnType::Float => left
            .as_f64()
            .partial_cmp(&right.as_f64())
            .unwrap_or(Ordering::Equal),
        ColumnType::Text => left
            .as_str()
            .expect("typed text was validated")
            .cmp(right.as_str().expect("typed text was validated")),
        _ => Ordering::Equal,
    }
}

fn column_definition<'a>(
    schema: &'a TableSchema,
    column: &str,
    table: &str,
) -> Result<&'a ColumnDefinition> {
    schema
        .columns
        .iter()
        .find(|definition| definition.name == column)
        .ok_or_else(|| EngineError::column_not_found(column, table))
}

fn column_type_name(data_type: ColumnType) -> &'static str {
    match data_type {
        ColumnType::Boolean => "boolean",
        ColumnType::Integer => "integer",
        ColumnType::Float => "float",
        ColumnType::Text => "text",
        ColumnType::Json => "json",
    }
}

fn aggregate_function(value: &str) -> Option<AggregateFunction> {
    if value.eq_ignore_ascii_case("count") {
        Some(AggregateFunction::Count)
    } else if value.eq_ignore_ascii_case("sum") {
        Some(AggregateFunction::Sum)
    } else if value.eq_ignore_ascii_case("avg") {
        Some(AggregateFunction::Avg)
    } else if value.eq_ignore_ascii_case("min") {
        Some(AggregateFunction::Min)
    } else if value.eq_ignore_ascii_case("max") {
        Some(AggregateFunction::Max)
    } else {
        None
    }
}

struct Parser<'a> {
    tokens: Vec<Token>,
    position: usize,
    params: &'a [Value],
}

impl<'a> Parser<'a> {
    fn new(tokens: Vec<Token>, params: &'a [Value]) -> Self {
        Self {
            tokens,
            position: 0,
            params,
        }
    }

    fn parse(mut self) -> Result<AggregatePlan> {
        self.expect_keyword("select")?;
        let items = self.parse_items()?;
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
        let group_by = if self.consume_keyword("group") {
            self.expect_keyword("by")?;
            self.parse_identifier_list(MAX_GROUP_COLUMNS, "GROUP BY")?
        } else {
            vec![]
        };
        let order_by = if self.consume_keyword("order") {
            self.expect_keyword("by")?;
            self.parse_order_by()?
        } else {
            vec![]
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
        self.consume_semicolon();
        if self.position != self.tokens.len() {
            return Err(EngineError::unsupported_sql(
                "This aggregate subset does not support HAVING, DISTINCT, FILTER, windows, expressions, or joins",
            ));
        }
        Ok(AggregatePlan {
            table,
            items,
            predicate,
            group_by,
            order_by,
            limit,
            offset,
        })
    }

    fn parse_items(&mut self) -> Result<Vec<SelectItem>> {
        let mut items = Vec::new();
        loop {
            if items.len() >= MAX_SELECT_ITEMS {
                return Err(EngineError::invalid_query(format!(
                    "A projection cannot contain more than {MAX_SELECT_ITEMS} items"
                )));
            }
            items.push(self.parse_item()?);
            if !self.consume_comma() {
                break;
            }
        }
        Ok(items)
    }

    fn parse_item(&mut self) -> Result<SelectItem> {
        let (value, quoted) = self.next_identifier()?;
        let function = (!quoted).then(|| aggregate_function(&value)).flatten();
        let expression = if let Some(function) = function.filter(|_| self.consume_lparen()) {
            let argument = if self.consume_star() {
                AggregateArgument::Star
            } else {
                AggregateArgument::Column(self.parse_identifier()?)
            };
            self.expect_rparen("Expected `)` after aggregate argument")?;
            SelectExpression::Aggregate { function, argument }
        } else {
            SelectExpression::Column(normalize_identifier(value, quoted)?)
        };
        let default_output = match &expression {
            SelectExpression::Column(column) => column.clone(),
            SelectExpression::Aggregate { function, .. } => function.name().to_owned(),
        };
        let output = if self.consume_keyword("as") {
            self.parse_identifier()?
        } else {
            default_output
        };
        Ok(SelectItem { expression, output })
    }

    fn parse_table_name(&mut self) -> Result<String> {
        let first = self.parse_identifier()?;
        if !self.consume_dot() {
            return Ok(first);
        }
        let second = self.parse_identifier()?;
        if self.peek_dot() {
            return Err(EngineError::unsupported_sql(
                "Only unqualified or schema-qualified table names are supported",
            ));
        }
        Ok(format!("{first}.{second}"))
    }

    fn parse_identifier_list(&mut self, limit: usize, context: &str) -> Result<Vec<String>> {
        let mut values = Vec::new();
        loop {
            if values.len() >= limit {
                return Err(EngineError::invalid_query(format!(
                    "{context} cannot contain more than {limit} columns"
                )));
            }
            values.push(self.parse_identifier()?);
            if !self.consume_comma() {
                break;
            }
        }
        Ok(values)
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
            if !self.consume_comma() {
                break;
            }
        }
        Ok(orders)
    }

    fn parse_limit(&mut self) -> Result<usize> {
        let value = self.parse_value()?;
        let Value::Number(number) = value else {
            return Err(EngineError::invalid_query(
                "LIMIT and OFFSET must be non-negative integers",
            ));
        };
        number
            .as_u64()
            .and_then(|number| usize::try_from(number).ok())
            .ok_or_else(|| EngineError::invalid_query("LIMIT or OFFSET is too large"))
    }

    fn parse_value(&mut self) -> Result<Value> {
        let Some(token) = self.next() else {
            return Err(EngineError::parse_error("Expected a SQL value"));
        };
        match token {
            Token::Number(value) => Number::from_str(&value).map(Value::Number).map_err(|_| {
                EngineError::invalid_query(format!("Invalid number literal `{value}`"))
            }),
            Token::Placeholder(index) => bind_parameter(&index, self.params),
            _ => Err(EngineError::invalid_query(
                "LIMIT and OFFSET must be non-negative integers",
            )),
        }
    }

    fn parse_identifier(&mut self) -> Result<String> {
        let (value, quoted) = self.next_identifier()?;
        normalize_identifier(value, quoted)
    }

    fn next_identifier(&mut self) -> Result<(String, bool)> {
        let Some(Token::Identifier { value, quoted }) = self.next() else {
            return Err(EngineError::parse_error("Expected a SQL identifier"));
        };
        Ok((value, quoted))
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

    fn consume_comma(&mut self) -> bool {
        self.consume_if(|token| matches!(token, Token::Comma))
    }

    fn consume_dot(&mut self) -> bool {
        self.consume_if(|token| matches!(token, Token::Dot))
    }

    fn peek_dot(&self) -> bool {
        matches!(self.tokens.get(self.position), Some(Token::Dot))
    }

    fn consume_lparen(&mut self) -> bool {
        self.consume_if(|token| matches!(token, Token::LParen))
    }

    fn consume_star(&mut self) -> bool {
        self.consume_if(|token| matches!(token, Token::Star))
    }

    fn expect_rparen(&mut self, message: &str) -> Result<()> {
        if self.consume_if(|token| matches!(token, Token::RParen)) {
            Ok(())
        } else {
            Err(EngineError::parse_error(message))
        }
    }

    fn consume_semicolon(&mut self) {
        self.consume_if(|token| matches!(token, Token::Semicolon));
    }

    fn consume_if(&mut self, predicate: impl FnOnce(&Token) -> bool) -> bool {
        if self.tokens.get(self.position).is_some_and(predicate) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.position)?.clone();
        self.position += 1;
        Some(token)
    }
}

fn normalize_identifier(value: String, quoted: bool) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{Engine, Row, TableSchema};

    fn row(value: Value) -> Row {
        value
            .as_object()
            .expect("test row must be an object")
            .clone()
    }

    fn sales() -> Engine {
        let mut engine = Engine::default();
        engine
            .execute_sql(
                "CREATE TABLE sales (\
                    id INTEGER PRIMARY KEY, region TEXT, amount INTEGER, score DOUBLE PRECISION, \
                    label TEXT, optional INTEGER, active BOOLEAN, metadata JSONB\
                )",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO sales \
                 (id, region, amount, score, label, optional, active, metadata) VALUES \
                 (1, 'east', 10, 1.5, 'b', NULL, true, NULL), \
                 (2, 'east', 20, 2.5, 'a', 2, false, NULL), \
                 (3, 'west', NULL, NULL, 'c', 3, true, NULL), \
                 (4, NULL, 5, 4.0, 'd', NULL, false, NULL)",
                &[],
            )
            .unwrap();
        engine
    }

    #[test]
    fn computes_global_aggregates_with_postgres_null_semantics() {
        let result = sales()
            .query_sql(
                "SELECT COUNT(*) AS rows, COUNT(amount) AS non_null, \
                 SUM(amount) AS total, AVG(amount) AS mean, \
                 MIN(label) AS first_label, MAX(label) AS last_label FROM sales",
                &[],
            )
            .unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0]["rows"], json!(4));
        assert_eq!(result.rows[0]["non_null"], json!(3));
        assert_eq!(result.rows[0]["total"], json!(35));
        assert_eq!(result.rows[0]["mean"], json!(35.0 / 3.0));
        assert_eq!(result.rows[0]["first_label"], json!("a"));
        assert_eq!(result.rows[0]["last_label"], json!("d"));
    }

    #[test]
    fn groups_filters_orders_and_pages_projected_outputs() {
        let result = sales()
            .query_sql(
                "SELECT region, COUNT(*) AS rows, SUM(amount) AS total, AVG(score) AS mean \
                 FROM sales WHERE id >= $1 GROUP BY region \
                 ORDER BY total DESC NULLS LAST, region ASC LIMIT 2 OFFSET 0",
                &[json!(1)],
            )
            .unwrap();
        assert_eq!(
            result.rows,
            vec![
                row(json!({"region": "east", "rows": 2, "total": 30, "mean": 2.0})),
                row(json!({"region": null, "rows": 1, "total": 5, "mean": 4.0})),
            ]
        );

        assert_eq!(
            sales()
                .query_sql(
                    "SELECT region FROM sales GROUP BY region ORDER BY region",
                    &[],
                )
                .unwrap()
                .rows,
            vec![
                row(json!({"region": "east"})),
                row(json!({"region": "west"})),
                row(json!({"region": null})),
            ]
        );
    }

    #[test]
    fn global_and_grouped_empty_inputs_differ_like_postgres() {
        let engine = sales();
        assert_eq!(
            engine
                .query_sql(
                    "SELECT COUNT(*) AS rows, SUM(amount) AS total, AVG(score) AS mean, \
                     MIN(label) AS first_label, MAX(label) AS last_label FROM sales WHERE id > 99",
                    &[],
                )
                .unwrap()
                .rows,
            vec![row(json!({
                "rows": 0,
                "total": null,
                "mean": null,
                "first_label": null,
                "last_label": null
            }))]
        );
        assert!(
            engine
                .query_sql(
                    "SELECT region, COUNT(*) AS rows FROM sales WHERE id > 99 GROUP BY region",
                    &[],
                )
                .unwrap()
                .rows
                .is_empty()
        );
    }

    #[test]
    fn float_group_keys_canonicalize_positive_and_negative_zero() {
        let mut engine = sales();
        engine
            .execute_sql(
                "INSERT INTO sales (id, region, score, label, active) VALUES \
                 (5, 'zero', 0.0, 'z', true), (6, 'negative-zero', -0.0, 'y', false)",
                &[],
            )
            .unwrap();
        assert_eq!(
            engine
                .query_sql(
                    "SELECT score, COUNT(*) AS rows FROM sales WHERE id >= 5 GROUP BY score",
                    &[],
                )
                .unwrap()
                .rows,
            vec![row(json!({"score": 0.0, "rows": 2}))]
        );
    }

    #[test]
    fn rejects_ambiguous_or_unsupported_aggregate_shapes_and_types() {
        let engine = sales();
        for (sql, code) in [
            (
                "SELECT region, COUNT(*) AS rows FROM sales",
                "INVALID_QUERY",
            ),
            ("SELECT SUM(label) AS total FROM sales", "TYPE_MISMATCH"),
            (
                "SELECT MIN(active) AS first_value FROM sales",
                "TYPE_MISMATCH",
            ),
            (
                "SELECT metadata, COUNT(*) AS rows FROM sales GROUP BY metadata",
                "TYPE_MISMATCH",
            ),
            (
                "SELECT COUNT(id), COUNT(amount) FROM sales",
                "INVALID_QUERY",
            ),
            (
                "SELECT COUNT(*) AS rows FROM sales ORDER BY region",
                "COLUMN_NOT_FOUND",
            ),
            (
                "SELECT region, COUNT(*) AS rows FROM sales GROUP BY region HAVING COUNT(*) > 0",
                "UNSUPPORTED_SQL",
            ),
        ] {
            assert_eq!(
                engine.query_sql(sql, &[]).unwrap_err().code,
                code,
                "query was `{sql}`"
            );
        }

        let mut legacy = Engine::default();
        legacy
            .define_table(TableSchema {
                name: "legacy".to_owned(),
                primary_key: vec!["id".to_owned()],
                columns: vec![],
            })
            .unwrap();
        assert_eq!(
            legacy
                .query_sql("SELECT COUNT(*) AS rows FROM legacy", &[])
                .unwrap_err()
                .code,
            "UNSUPPORTED_SQL"
        );
    }

    #[test]
    fn integer_sum_fails_instead_of_losing_javascript_precision() {
        let mut engine = Engine::default();
        engine
            .execute_sql(
                "CREATE TABLE totals (id INTEGER PRIMARY KEY, value INTEGER NOT NULL)",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO totals (id, value) VALUES (1, 9007199254740991), (2, 1)",
                &[],
            )
            .unwrap();
        assert_eq!(
            engine
                .query_sql("SELECT SUM(value) AS total FROM totals", &[])
                .unwrap_err()
                .code,
            "NUMERIC_OVERFLOW"
        );
    }

    #[test]
    fn execute_sql_and_transactions_read_the_staged_aggregate_state() {
        let mut engine = sales();
        engine.begin_transaction().unwrap();
        engine
            .execute_sql(
                "INSERT INTO sales (id, region, label, active) VALUES (5, 'north', 'n', true)",
                &[],
            )
            .unwrap();
        let staged = engine
            .execute_sql("SELECT COUNT(*) AS rows FROM sales", &[])
            .unwrap();
        assert_eq!(staged.command, "SELECT");
        assert_eq!(staged.row_count, 1);
        assert_eq!(staged.rows, vec![row(json!({"rows": 5}))]);
        engine.rollback_transaction().unwrap();
        assert_eq!(
            engine
                .query_sql("SELECT COUNT(*) AS rows FROM sales", &[])
                .unwrap()
                .rows,
            vec![row(json!({"rows": 4}))]
        );
    }

    #[test]
    fn bounds_aggregate_and_grouping_complexity() {
        let engine = sales();
        let aggregates = (0..=super::MAX_AGGREGATES)
            .map(|index| format!("COUNT(*) AS c{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(
            engine
                .query_sql(&format!("SELECT {aggregates} FROM sales"), &[])
                .unwrap_err()
                .code,
            "INVALID_QUERY"
        );

        let groups = std::iter::repeat_n("region", super::MAX_GROUP_COLUMNS + 1)
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(
            engine
                .query_sql(
                    &format!("SELECT region, COUNT(*) AS rows FROM sales GROUP BY {groups}"),
                    &[],
                )
                .unwrap_err()
                .code,
            "INVALID_QUERY"
        );
    }
}
