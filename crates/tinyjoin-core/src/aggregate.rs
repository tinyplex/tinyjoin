use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet, btree_map::Entry};
use std::str::FromStr;

use serde_json::{Map, Number, Value};

use crate::query::{
    Token, bind_parameter, is_reserved_keyword, matches_predicate, parse_predicate_at, tokenize,
    validate_predicate_columns, validate_predicate_types, validate_sql_input,
};
use crate::storage::StorageReader;
use crate::{
    ColumnDefinition, ColumnType, EngineError, NullOrder, OrderBy, OrderDirection, Predicate,
    QueryResult, Result, ResultField, Row, TableDefinition, VisitControl,
};

const MAX_SELECT_ITEMS: usize = 256;
const MAX_AGGREGATES: usize = 64;
const MAX_GROUP_COLUMNS: usize = 32;
const MAX_ORDER_COLUMNS: usize = 32;
const MAX_GROUPS: usize = 100_000;
const MAX_AGGREGATE_CELLS: usize = 1_000_000;
const MAX_SCAN_ROWS: usize = 1_000_000;
const MAX_AGGREGATE_WORK_BYTES: usize = 16 * 1024 * 1024;
const MAX_AGGREGATE_RESULT_BYTES: usize = 16 * 1024 * 1024;
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

pub(crate) fn bind_plan_parameters(
    plan: &AggregatePlan,
    params: &[Value],
    limit_parameter: Option<usize>,
    offset_parameter: Option<usize>,
) -> Result<AggregatePlan> {
    let mut plan = plan.clone();
    crate::query::bind_predicate_parameters(plan.predicate.as_mut(), params)?;
    if let Some(index) = limit_parameter {
        plan.limit = Some(crate::query::bind_nonnegative_integer_parameter(
            index, params,
        )?);
    }
    if let Some(index) = offset_parameter {
        plan.offset = crate::query::bind_nonnegative_integer_parameter(index, params)?;
    }
    Ok(plan)
}

pub(crate) fn execute(storage: &dyn StorageReader, plan: &AggregatePlan) -> Result<QueryResult> {
    let schema = storage.table_schema(&plan.table)?;
    validate_plan(plan, &schema)?;
    let fields = result_fields(plan, &schema)?;

    if plan.limit == Some(0) {
        return Ok(QueryResult {
            revision: storage.revision(),
            fields,
            rows: Vec::new(),
        });
    }

    let aggregate_count = plan
        .items
        .iter()
        .filter(|item| matches!(item.expression, SelectExpression::Aggregate { .. }))
        .count();
    let mut global = if plan.group_by.is_empty() {
        Some(GroupState::new(plan, &schema, None)?)
    } else {
        None
    };
    let mut working_bytes = global.as_ref().map_or(Ok(0), GroupState::estimated_bytes)?;
    ensure_work_budget(working_bytes, MAX_AGGREGATE_WORK_BYTES)?;
    let mut groups = BTreeMap::<String, GroupState>::new();
    let mut scanned = 0_usize;
    storage.visit_table(&plan.table, &mut |row| {
        scanned = scanned.saturating_add(1);
        if scanned > MAX_SCAN_ROWS {
            return Err(EngineError::new(
                "QUERY_WORK_LIMIT_EXCEEDED",
                format!("An aggregate query cannot scan more than {MAX_SCAN_ROWS} rows"),
            ));
        }
        if matches_predicate(row, plan.predicate.as_ref(), &plan.table)? {
            if let Some(state) = &mut global {
                let before = state.estimated_bytes()?;
                let after = state.estimated_bytes_after(row)?;
                let next_total = replace_budget_charge(
                    working_bytes,
                    before,
                    after,
                    MAX_AGGREGATE_WORK_BYTES,
                )?;
                state.update(row)?;
                working_bytes = next_total;
                return Ok(VisitControl::Continue);
            }
            ensure_group_key_input_budget(&schema, &plan.group_by, row)?;
            let key = group_key(&schema, &plan.group_by, row)?;
            let group_count = groups.len();
            match groups.entry(key) {
                Entry::Vacant(entry) => {
                    if group_count >= MAX_GROUPS {
                        return Err(EngineError::invalid_query(format!(
                            "A grouped query cannot produce more than {MAX_GROUPS} groups"
                        )));
                    }
                    if (group_count + 1).saturating_mul(aggregate_count)
                        > MAX_AGGREGATE_CELLS
                    {
                        return Err(EngineError::invalid_query(format!(
                            "A grouped query cannot materialize more than {MAX_AGGREGATE_CELLS} aggregate cells"
                        )));
                    }
                    let key_bytes = checked_mul(entry.key().len(), 2)?;
                    let state = GroupState::new(plan, &schema, Some(row))?;
                    let charge = checked_add(
                        checked_add(256, key_bytes)?,
                        state.estimated_bytes_after(row)?,
                    )?;
                    let next_total = checked_add(working_bytes, charge)?;
                    ensure_work_budget(next_total, MAX_AGGREGATE_WORK_BYTES)?;
                    let mut state = state;
                    state.update(row)?;
                    working_bytes = next_total;
                    entry.insert(state);
                }
                Entry::Occupied(mut entry) => {
                    let before = entry.get().estimated_bytes()?;
                    let after = entry.get().estimated_bytes_after(row)?;
                    let next_total = replace_budget_charge(
                        working_bytes,
                        before,
                        after,
                        MAX_AGGREGATE_WORK_BYTES,
                    )?;
                    entry.get_mut().update(row)?;
                    working_bytes = next_total;
                }
            }
        }
        Ok(VisitControl::Continue)
    })?;

    let states = if let Some(global) = global {
        vec![global]
    } else {
        groups.into_values().collect()
    };
    let mut result_bytes = 0_usize;
    let mut rows = Vec::with_capacity(states.len());
    for state in &states {
        let projected_bytes = projected_row_bytes(plan, state)?;
        let next_result_bytes = checked_add(result_bytes, projected_bytes)?;
        ensure_result_budget(next_result_bytes)?;
        let row = project_group(plan, state)?;
        result_bytes = next_result_bytes;
        rows.push(row);
    }
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
        fields,
        rows,
    })
}

fn result_fields(plan: &AggregatePlan, schema: &TableDefinition) -> Result<Vec<ResultField>> {
    plan.items
        .iter()
        .map(|item| {
            let data_type = match &item.expression {
                SelectExpression::Column(column) => {
                    column_definition(schema, column, &plan.table)?.data_type
                }
                SelectExpression::Aggregate { function, argument } => match function {
                    AggregateFunction::Count => ColumnType::Integer,
                    AggregateFunction::Avg => ColumnType::Float,
                    AggregateFunction::Sum => {
                        aggregate_argument_type(argument, schema, &plan.table)?
                    }
                    AggregateFunction::Min | AggregateFunction::Max => {
                        aggregate_argument_type(argument, schema, &plan.table)?
                    }
                },
            };
            Ok(ResultField::new(&item.output, data_type))
        })
        .collect()
}

fn aggregate_argument_type(
    argument: &AggregateArgument,
    schema: &TableDefinition,
    table: &str,
) -> Result<ColumnType> {
    match argument {
        AggregateArgument::Column(column) => {
            Ok(column_definition(schema, column, table)?.data_type)
        }
        AggregateArgument::Star => Err(EngineError::unsupported_sql(
            "Only COUNT accepts `*` as an aggregate argument",
        )),
    }
}

fn validate_plan(plan: &AggregatePlan, schema: &TableDefinition) -> Result<()> {
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
    schema: &TableDefinition,
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

struct GroupState {
    grouped_values: Row,
    aggregates: Vec<AggregateAccumulator>,
}

impl GroupState {
    fn new(
        plan: &AggregatePlan,
        schema: &TableDefinition,
        representative: Option<&Row>,
    ) -> Result<Self> {
        let mut grouped_values = Map::new();
        let mut aggregates = Vec::new();
        for item in &plan.items {
            match &item.expression {
                SelectExpression::Column(column) => {
                    let value = representative
                        .and_then(|row| row.get(column))
                        .cloned()
                        .ok_or_else(|| {
                            EngineError::invalid_query(format!(
                                "Grouped column `{column}` has no representative row"
                            ))
                        })?;
                    grouped_values.insert(column.clone(), value);
                }
                SelectExpression::Aggregate { function, argument } => {
                    aggregates.push(AggregateAccumulator::new(*function, argument, schema)?)
                }
            }
        }
        Ok(Self {
            grouped_values,
            aggregates,
        })
    }

    fn update(&mut self, row: &Row) -> Result<()> {
        for aggregate in &mut self.aggregates {
            aggregate.update(row)?;
        }
        Ok(())
    }

    fn estimated_bytes_after(&self, row: &Row) -> Result<usize> {
        let mut bytes = 0_usize;
        for (column, value) in &self.grouped_values {
            bytes = checked_add(bytes, 96)?;
            bytes = checked_add(bytes, checked_mul(column.len(), 2)?)?;
            bytes = checked_add(bytes, checked_mul(owned_value_bytes(value)?, 2)?)?;
        }
        for aggregate in &self.aggregates {
            bytes = checked_add(bytes, aggregate.estimated_bytes_after(row)?)?;
        }
        Ok(bytes)
    }

    fn estimated_bytes(&self) -> Result<usize> {
        let mut bytes = 0_usize;
        for (column, value) in &self.grouped_values {
            bytes = checked_add(bytes, 96)?;
            bytes = checked_add(bytes, checked_mul(column.len(), 2)?)?;
            bytes = checked_add(bytes, checked_mul(owned_value_bytes(value)?, 2)?)?;
        }
        for aggregate in &self.aggregates {
            bytes = checked_add(bytes, aggregate.estimated_bytes()?)?;
        }
        Ok(bytes)
    }
}

enum AggregateAccumulator {
    Count {
        column: Option<String>,
        count: u64,
    },
    IntegerSum {
        column: String,
        sum: i128,
        seen: bool,
    },
    FloatSum {
        column: String,
        sum: f64,
        seen: bool,
    },
    Average {
        column: String,
        data_type: ColumnType,
        sum: f64,
        count: u64,
    },
    Extremum {
        column: String,
        data_type: ColumnType,
        value: Option<Value>,
        minimum: bool,
    },
}

impl AggregateAccumulator {
    fn new(
        function: AggregateFunction,
        argument: &AggregateArgument,
        schema: &TableDefinition,
    ) -> Result<Self> {
        let column = match argument {
            AggregateArgument::Star => None,
            AggregateArgument::Column(column) => Some(column.clone()),
        };
        if function == AggregateFunction::Count {
            return Ok(Self::Count { column, count: 0 });
        }
        let column = column.ok_or_else(|| {
            EngineError::unsupported_sql("Only COUNT accepts `*` as an aggregate argument")
        })?;
        let definition = column_definition(schema, &column, &schema.name)?;
        match function {
            AggregateFunction::Count => unreachable!("COUNT returned above"),
            AggregateFunction::Sum if definition.data_type == ColumnType::Integer => {
                Ok(Self::IntegerSum {
                    column,
                    sum: 0,
                    seen: false,
                })
            }
            AggregateFunction::Sum => Ok(Self::FloatSum {
                column,
                sum: 0.0,
                seen: false,
            }),
            AggregateFunction::Avg => Ok(Self::Average {
                column,
                data_type: definition.data_type,
                sum: 0.0,
                count: 0,
            }),
            AggregateFunction::Min | AggregateFunction::Max => Ok(Self::Extremum {
                column,
                data_type: definition.data_type,
                value: None,
                minimum: function == AggregateFunction::Min,
            }),
        }
    }

    fn update(&mut self, row: &Row) -> Result<()> {
        match self {
            Self::Count { column, count } => {
                let counts = column.as_ref().is_none_or(|column| {
                    row.get(column).is_some_and(|value| value != &Value::Null)
                });
                if counts {
                    *count = count.checked_add(1).ok_or_else(|| {
                        EngineError::new("NUMERIC_OVERFLOW", "COUNT result overflowed")
                    })?;
                }
            }
            Self::IntegerSum { column, sum, seen } => {
                if let Some(value) = non_null_value(row, column) {
                    *sum = sum.checked_add(integer_value(value)?).ok_or_else(|| {
                        EngineError::new("NUMERIC_OVERFLOW", "SUM result overflowed")
                    })?;
                    *seen = true;
                }
            }
            Self::FloatSum { column, sum, seen } => {
                if let Some(value) = non_null_value(row, column) {
                    *sum += value.as_f64().expect("typed float was validated");
                    *seen = true;
                }
            }
            Self::Average {
                column,
                data_type,
                sum,
                count,
            } => {
                if let Some(value) = non_null_value(row, column) {
                    *sum += if *data_type == ColumnType::Integer {
                        integer_value(value)? as f64
                    } else {
                        value.as_f64().expect("typed float was validated")
                    };
                    *count = count.checked_add(1).ok_or_else(|| {
                        EngineError::new("NUMERIC_OVERFLOW", "AVG count overflowed")
                    })?;
                }
            }
            Self::Extremum {
                column,
                data_type,
                value: selected,
                minimum,
            } => {
                if let Some(value) = non_null_value(row, column) {
                    let replace = selected.as_ref().is_none_or(|selected| {
                        let ordering = compare_typed(*data_type, value, selected);
                        if *minimum {
                            ordering == Ordering::Less
                        } else {
                            ordering == Ordering::Greater
                        }
                    });
                    if replace {
                        *selected = Some(value.clone());
                    }
                }
            }
        }
        Ok(())
    }

    fn finish(&self) -> Result<Value> {
        match self {
            Self::Count { count, .. } => {
                if u128::from(*count) > MAX_SAFE_INTEGER as u128 {
                    return Err(EngineError::new(
                        "NUMERIC_OVERFLOW",
                        "COUNT exceeds TinyJoin's safe-integer range",
                    ));
                }
                Ok(Value::Number(Number::from(*count)))
            }
            Self::IntegerSum { sum, seen, .. } => {
                if !seen {
                    return Ok(Value::Null);
                }
                if !(-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(sum) {
                    return Err(EngineError::new(
                        "NUMERIC_OVERFLOW",
                        "SUM exceeds TinyJoin's safe-integer range",
                    ));
                }
                Ok(Value::Number(Number::from(*sum as i64)))
            }
            Self::FloatSum { sum, seen, .. } => {
                if *seen {
                    finite_number(*sum, "SUM")
                } else {
                    Ok(Value::Null)
                }
            }
            Self::Average { sum, count, .. } => {
                if *count == 0 {
                    Ok(Value::Null)
                } else {
                    finite_number(*sum / *count as f64, "AVG")
                }
            }
            Self::Extremum { value, .. } => Ok(value.clone().unwrap_or(Value::Null)),
        }
    }

    fn estimated_bytes(&self) -> Result<usize> {
        let (column, selected) = match self {
            Self::Count { column, .. } => (column.as_deref(), None),
            Self::IntegerSum { column, .. }
            | Self::FloatSum { column, .. }
            | Self::Average { column, .. } => (Some(column.as_str()), None),
            Self::Extremum { column, value, .. } => (Some(column.as_str()), value.as_ref()),
        };
        let mut bytes = 96_usize;
        if let Some(column) = column {
            bytes = checked_add(bytes, checked_mul(column.len(), 2)?)?;
        }
        if let Some(value) = selected {
            bytes = checked_add(bytes, checked_mul(owned_value_bytes(value)?, 2)?)?;
        }
        Ok(bytes)
    }

    fn estimated_bytes_after(&self, row: &Row) -> Result<usize> {
        let mut bytes = self.estimated_bytes()?;
        let Self::Extremum {
            column,
            data_type,
            value: selected,
            minimum,
        } = self
        else {
            return Ok(bytes);
        };
        let Some(incoming) = non_null_value(row, column) else {
            return Ok(bytes);
        };
        let replaces = selected.as_ref().is_none_or(|selected| {
            let ordering = compare_typed(*data_type, incoming, selected);
            if *minimum {
                ordering == Ordering::Less
            } else {
                ordering == Ordering::Greater
            }
        });
        if replaces {
            if let Some(selected) = selected {
                bytes = bytes
                    .checked_sub(checked_mul(owned_value_bytes(selected)?, 2)?)
                    .ok_or_else(work_limit_error)?;
            }
            bytes = checked_add(bytes, checked_mul(owned_value_bytes(incoming)?, 2)?)?;
        }
        Ok(bytes)
    }

    fn result_value_bytes(&self) -> Result<usize> {
        match self {
            Self::Extremum { value, .. } => value.as_ref().map_or(Ok(16), owned_value_bytes),
            Self::Count { .. }
            | Self::IntegerSum { .. }
            | Self::FloatSum { .. }
            | Self::Average { .. } => Ok(16),
        }
    }
}

fn non_null_value<'a>(row: &'a Row, column: &str) -> Option<&'a Value> {
    row.get(column).filter(|value| *value != &Value::Null)
}

fn ensure_group_key_input_budget(
    schema: &TableDefinition,
    columns: &[String],
    row: &Row,
) -> Result<()> {
    let mut bytes = 32_usize;
    for column in columns {
        let value = row
            .get(column)
            .ok_or_else(|| EngineError::column_not_found(column, &schema.name))?;
        bytes = checked_add(bytes, 16)?;
        bytes = checked_add(bytes, checked_mul(column.len(), 2)?)?;
        // `group_key` JSON-escapes text into a tagged component and then encodes the component
        // vector. Control characters can therefore transiently expand to roughly thirteen times
        // their input length while both encodings coexist; fourteen is a conservative ceiling.
        bytes = checked_add(bytes, checked_mul(owned_value_bytes(value)?, 14)?)?;
    }
    ensure_work_budget(bytes, MAX_AGGREGATE_WORK_BYTES)
}

fn projected_row_bytes(plan: &AggregatePlan, state: &GroupState) -> Result<usize> {
    let mut bytes = 32_usize;
    let mut aggregate_index = 0;
    for item in &plan.items {
        bytes = checked_add(bytes, 64)?;
        bytes = checked_add(bytes, checked_mul(item.output.len(), 2)?)?;
        let value_bytes = match &item.expression {
            SelectExpression::Column(column) => owned_value_bytes(
                state
                    .grouped_values
                    .get(column)
                    .ok_or_else(|| EngineError::column_not_found(column, "aggregate group"))?,
            )?,
            SelectExpression::Aggregate { .. } => {
                let bytes = state.aggregates[aggregate_index].result_value_bytes()?;
                aggregate_index += 1;
                bytes
            }
        };
        bytes = checked_add(bytes, checked_mul(value_bytes, 2)?)?;
    }
    Ok(bytes)
}

#[cfg(test)]
fn owned_row_bytes(row: &Row) -> Result<usize> {
    let mut bytes = 32_usize;
    for (key, value) in row {
        bytes = checked_add(bytes, 64)?;
        bytes = checked_add(bytes, checked_mul(key.len(), 2)?)?;
        bytes = checked_add(bytes, checked_mul(owned_value_bytes(value)?, 2)?)?;
    }
    Ok(bytes)
}

fn owned_value_bytes(value: &Value) -> Result<usize> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => Ok(16),
        Value::String(value) => checked_add(24, value.len()),
        Value::Array(values) => {
            let mut bytes = 32_usize;
            for value in values {
                bytes = checked_add(bytes, 32)?;
                bytes = checked_add(bytes, owned_value_bytes(value)?)?;
            }
            Ok(bytes)
        }
        Value::Object(values) => {
            let mut bytes = 32_usize;
            for (key, value) in values {
                bytes = checked_add(bytes, 64)?;
                bytes = checked_add(bytes, checked_mul(key.len(), 2)?)?;
                bytes = checked_add(bytes, owned_value_bytes(value)?)?;
            }
            Ok(bytes)
        }
    }
}

fn replace_budget_charge(
    total: usize,
    previous: usize,
    next: usize,
    limit: usize,
) -> Result<usize> {
    let total = total.checked_sub(previous).ok_or_else(work_limit_error)?;
    let total = checked_add(total, next)?;
    ensure_work_budget(total, limit)?;
    Ok(total)
}

fn checked_add(left: usize, right: usize) -> Result<usize> {
    left.checked_add(right).ok_or_else(work_limit_error)
}

fn checked_mul(left: usize, right: usize) -> Result<usize> {
    left.checked_mul(right).ok_or_else(work_limit_error)
}

fn ensure_work_budget(bytes: usize, limit: usize) -> Result<()> {
    if bytes > limit {
        Err(work_limit_error())
    } else {
        Ok(())
    }
}

fn ensure_result_budget(bytes: usize) -> Result<()> {
    if bytes > MAX_AGGREGATE_RESULT_BYTES {
        Err(EngineError::new(
            "QUERY_WORK_LIMIT_EXCEEDED",
            format!(
                "An aggregate query cannot materialize more than {MAX_AGGREGATE_RESULT_BYTES} bytes of results"
            ),
        ))
    } else {
        Ok(())
    }
}

fn work_limit_error() -> EngineError {
    EngineError::new(
        "QUERY_WORK_LIMIT_EXCEEDED",
        format!(
            "An aggregate query cannot retain more than {MAX_AGGREGATE_WORK_BYTES} bytes of working state"
        ),
    )
}

fn project_group(plan: &AggregatePlan, state: &GroupState) -> Result<Row> {
    let mut projected = Map::new();
    let mut aggregate_index = 0;
    for item in &plan.items {
        let value = match &item.expression {
            SelectExpression::Column(column) => {
                state.grouped_values.get(column).cloned().ok_or_else(|| {
                    EngineError::invalid_query(format!(
                        "Grouped column `{column}` has no representative row"
                    ))
                })?
            }
            SelectExpression::Aggregate { .. } => {
                let value = state.aggregates[aggregate_index].finish()?;
                aggregate_index += 1;
                value
            }
        };
        projected.insert(item.output.clone(), value);
    }
    Ok(projected)
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

fn group_key(schema: &TableDefinition, columns: &[String], row: &Row) -> Result<String> {
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
    schema: &'a TableDefinition,
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
        if crate::query::prepared_parameter_index(&value).is_some() {
            return Ok(0);
        }
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

    use crate::{Engine, Row, RowChange, StorageDriver, StorageReader};

    fn row(value: Value) -> Row {
        value
            .as_object()
            .expect("test row must be an object")
            .clone()
    }

    fn seed_rows(engine: &mut Engine, table: &str, rows: Vec<Row>) {
        let mut storage = std::mem::take(engine).into_storage();
        for row in rows {
            storage
                .apply_row_changes_unrevisioned(vec![RowChange::Upsert {
                    table: table.to_owned(),
                    row,
                }])
                .unwrap();
        }
        storage.advance_revision().unwrap();
        *engine = Engine::new(storage);
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
    fn streams_rows_without_using_the_compatibility_collector() {
        let storage = sales().into_storage();
        let before = storage.visitor_counts();
        let plan = super::parse_sql(
            "SELECT region, COUNT(*) AS rows, SUM(amount) AS total FROM sales GROUP BY region",
            &[],
        )
        .unwrap();

        let result = super::execute(&storage, &plan).unwrap();

        assert_eq!(result.rows.len(), 3);
        let after = storage.visitor_counts();
        assert_eq!(after.0 - before.0, 4);
        assert_eq!(after.1, before.1);
    }

    #[test]
    fn limit_zero_validates_without_reading_rows() {
        let storage = sales().into_storage();
        let before = storage.visitor_counts();
        let plan = super::parse_sql("SELECT COUNT(*) AS rows FROM sales LIMIT 0", &[]).unwrap();

        let result = super::execute(&storage, &plan).unwrap();

        assert!(result.rows.is_empty());
        assert_eq!(
            result.fields,
            vec![crate::ResultField::new("rows", crate::ColumnType::Integer)]
        );
        assert_eq!(storage.visitor_counts(), before);
    }

    #[test]
    fn aggregate_memory_budget_accounts_for_state_and_projected_aliases() {
        let plan = super::parse_sql(
            "SELECT region AS a, region AS b, MIN(label) AS first_label FROM sales GROUP BY region",
            &[],
        )
        .unwrap();
        let schema = sales().into_storage().table_schema("sales").unwrap();
        let representative = row(json!({"region": "r", "label": "small"}));
        let mut state = super::GroupState::new(&plan, &schema, Some(&representative)).unwrap();
        state.update(&representative).unwrap();

        let before = state.estimated_bytes().unwrap();
        let larger = row(json!({"region": "r", "label": "a much longer value"}));
        state.update(&larger).unwrap();
        let after = state.estimated_bytes().unwrap();
        assert!(after > before);

        let projected = super::project_group(&plan, &state).unwrap();
        assert!(super::owned_row_bytes(&projected).unwrap() > 2 * "r".len());
    }

    #[test]
    fn aggregate_budget_arithmetic_fails_closed() {
        assert_eq!(
            super::checked_add(usize::MAX, 1).unwrap_err().code,
            "QUERY_WORK_LIMIT_EXCEEDED"
        );
        assert_eq!(
            super::checked_mul(usize::MAX, 2).unwrap_err().code,
            "QUERY_WORK_LIMIT_EXCEEDED"
        );
        assert_eq!(
            super::replace_budget_charge(0, 1, 0, 1).unwrap_err().code,
            "QUERY_WORK_LIMIT_EXCEEDED"
        );
        assert_eq!(
            super::ensure_result_budget(super::MAX_AGGREGATE_RESULT_BYTES + 1)
                .unwrap_err()
                .code,
            "QUERY_WORK_LIMIT_EXCEEDED"
        );
    }

    #[test]
    fn aggregate_budgets_large_borrowed_values_before_cloning() {
        let huge = "z".repeat(crate::storage::MAX_LOGICAL_ROW_BYTES - 256);
        let mut engine = Engine::default();
        engine
            .execute_sql(
                "CREATE TABLE large_values (id INTEGER PRIMARY KEY, group_name TEXT, value TEXT)",
                &[],
            )
            .unwrap();
        seed_rows(
            &mut engine,
            "large_values",
            vec![row(json!({"id": 1, "group_name": huge, "value": "small"}))],
        );
        let result = engine
            .query_sql(
                "SELECT group_name, COUNT(*) AS rows FROM large_values GROUP BY group_name",
                &[],
            )
            .unwrap();
        assert_eq!(result.rows[0]["rows"], json!(1));

        let controls = "\0".repeat(crate::storage::MAX_LOGICAL_ROW_BYTES / 7);
        let mut control_keys = Engine::default();
        control_keys
            .execute_sql(
                "CREATE TABLE control_keys (id INTEGER PRIMARY KEY, group_name TEXT)",
                &[],
            )
            .unwrap();
        seed_rows(
            &mut control_keys,
            "control_keys",
            (0..17)
                .map(|id| {
                    row(json!({
                        "id": id,
                        "group_name": format!("{id}{controls}"),
                    }))
                })
                .collect(),
        );
        assert_eq!(
            control_keys
                .query_sql(
                    "SELECT group_name, COUNT(*) AS rows FROM control_keys GROUP BY group_name",
                    &[],
                )
                .unwrap_err()
                .code,
            "QUERY_WORK_LIMIT_EXCEEDED"
        );

        let mut extrema = Engine::default();
        extrema
            .execute_sql(
                "CREATE TABLE extrema (id INTEGER PRIMARY KEY, value TEXT)",
                &[],
            )
            .unwrap();
        seed_rows(
            &mut extrema,
            "extrema",
            (0..17)
                .map(|id| {
                    row(json!({
                        "id": id,
                        "value": format!(
                            "{id}{}",
                            "a".repeat(crate::storage::MAX_LOGICAL_ROW_BYTES - 256),
                        ),
                    }))
                })
                .collect(),
        );
        assert_eq!(
            extrema
                .query_sql("SELECT MAX(value) AS largest FROM extrema", &[])
                .unwrap()
                .rows
                .len(),
            1
        );

        let repeated = (0..super::MAX_SELECT_ITEMS)
            .map(|index| format!("value AS value_{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut aliases = Engine::default();
        aliases
            .execute_sql(
                "CREATE TABLE aliases (id INTEGER PRIMARY KEY, value TEXT)",
                &[],
            )
            .unwrap();
        aliases
            .execute_sql(
                "INSERT INTO aliases (id, value) VALUES (1, $1)",
                &[json!("x".repeat(40 * 1024))],
            )
            .unwrap();
        assert_eq!(
            aliases
                .query_sql(
                    &format!("SELECT {repeated} FROM aliases GROUP BY value"),
                    &[],
                )
                .unwrap_err()
                .code,
            "QUERY_WORK_LIMIT_EXCEEDED"
        );
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
        let global = engine
            .query_sql(
                "SELECT COUNT(*) AS rows, SUM(amount) AS total, AVG(score) AS mean, \
                 MIN(label) AS first_label, MAX(label) AS last_label FROM sales WHERE id > 99",
                &[],
            )
            .unwrap();
        assert_eq!(
            global.fields,
            vec![
                crate::ResultField::new("rows", crate::ColumnType::Integer),
                crate::ResultField::new("total", crate::ColumnType::Integer),
                crate::ResultField::new("mean", crate::ColumnType::Float),
                crate::ResultField::new("first_label", crate::ColumnType::Text),
                crate::ResultField::new("last_label", crate::ColumnType::Text),
            ]
        );
        assert_eq!(
            global.rows,
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
