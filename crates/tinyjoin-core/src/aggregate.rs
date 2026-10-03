use std::borrow::Cow;
use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

use serde_json::{Map, Number, Value};

use crate::hash::{KeyHasher, KeyMap, KeySet};
use crate::query::{
    Filter, ParseMode, Token, bind_parameter, column_definition, is_distinct_keyword_at,
    is_reserved_keyword, number_literal, pagination_value, parse_predicate_at, sort_rows_by,
    validate_predicate_columns, validate_predicate_types,
};
use crate::row::{RowRef, ValueRef};
use crate::storage::{StorageReader, column_type_name};
use crate::{
    ColumnType, EngineError, NullOrder, OrderBy, OrderDirection, Predicate, QueryResult, Result,
    ResultField, Row, TableDefinition, VisitControl,
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

#[derive(Clone, Debug, Eq, PartialEq)]
enum AggregateArgument {
    Star,
    Column(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
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
    /// `SELECT DISTINCT`, planned as a grouping by every projected column with no aggregates.
    distinct: bool,
    items: Vec<SelectItem>,
    /// Whether the outputs are keyed by position because their names repeat.
    positional: bool,
    predicate: Option<Predicate>,
    group_by: Vec<String>,
    order_by: Vec<OrderBy>,
    limit: Option<usize>,
    offset: usize,
}

/// Keys an aggregate query's outputs by position if their names repeat. Its `ORDER BY` names
/// outputs, so each of those takes the key of the output it names.
pub(crate) fn position_outputs(plan: &mut AggregatePlan) -> Result<()> {
    let items = &mut plan.items;
    plan.positional = crate::query::names_repeat(items.len(), &|index| &items[index].output);
    if !plan.positional {
        return Ok(());
    }
    for (index, item) in items.iter_mut().enumerate() {
        crate::query::position_name(index, &mut item.output);
    }
    for order in &mut plan.order_by {
        let mut named = plan
            .items
            .iter()
            .filter(|item| item.output[3..] == order.column);
        let first = named
            .next()
            .ok_or_else(|| EngineError::column_not_found(&order.column, "aggregate output"))?;
        if named.any(|other| other.expression != first.expression) {
            return Err(crate::query::ambiguous_order(&order.column));
        }
        order.column.clone_from(&first.output);
    }
    Ok(())
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

#[cfg(test)]
pub(crate) fn parse_sql(sql: &str, params: &[Value]) -> Result<AggregatePlan> {
    crate::query::validate_sql_input(sql, params)?;
    let tokens = crate::query::tokenize(sql)?;
    crate::query::validate_parameter_expansion(&tokens, params)?;
    parse_tokens(tokens, params, ParseMode::Bound)
}

pub(crate) fn parse_tokens(
    tokens: Vec<Token>,
    params: &[Value],
    mode: ParseMode,
) -> Result<AggregatePlan> {
    Parser::new(tokens, params, mode).parse()
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
    let mut groups = Groups::default();
    let mut scanned = 0_usize;
    let filter = Filter::new(plan.predicate.as_ref(), &schema, &plan.table)?;
    let group_columns = plan
        .group_by
        .iter()
        .map(|column| SourceColumn::new(&schema, column, &plan.table))
        .collect::<Result<Vec<_>>>()?;
    // Grouping never stops early, so narrowing only removes rows the predicate would reject
    // anyway; the per-row filter below remains the authority on membership.
    let read = read_columns(plan, &schema)?;
    crate::query::visit_aggregate_candidates(
        storage,
        &plan.table,
        plan.predicate.as_ref(),
        &schema,
        &read,
        &mut |row| {
            scanned = scanned.saturating_add(1);
            if scanned > MAX_SCAN_ROWS {
                return Err(EngineError::new(
                    "QUERY_WORK_LIMIT_EXCEEDED",
                    format!("An aggregate query cannot scan more than {MAX_SCAN_ROWS} rows"),
                ));
            }
            if filter.matches(row)? {
                if let Some(state) = &mut global {
                    working_bytes = state.update_within(row, working_bytes)?;
                    return Ok(VisitControl::Continue);
                }
                if let Some(index) = groups.find(&group_columns, row)? {
                    let state = &mut groups.entries[index].state;
                    working_bytes = state.update_within(row, working_bytes)?;
                    return Ok(VisitControl::Continue);
                }
                let group_count = groups.entries.len();
                if group_count >= MAX_GROUPS {
                    return Err(EngineError::invalid_query(format!(
                        "A grouped query cannot produce more than {MAX_GROUPS} groups"
                    )));
                }
                if (group_count + 1).saturating_mul(aggregate_count) > MAX_AGGREGATE_CELLS {
                    return Err(EngineError::invalid_query(format!(
                        "A grouped query cannot materialize more than {MAX_AGGREGATE_CELLS} aggregate cells"
                    )));
                }
                ensure_group_key_input_budget(&group_columns, row)?;
                let key = group_key(&group_columns, row)?;
                let key_bytes = checked_mul(key.len(), 2)?;
                let mut state = GroupState::new(plan, &schema, Some(row))?;
                let charge = checked_add(
                    checked_add(256, key_bytes)?,
                    state.estimated_bytes_after(row)?,
                )?;
                let next_total = checked_add(working_bytes, charge)?;
                ensure_work_budget(next_total, MAX_AGGREGATE_WORK_BYTES)?;
                state.update(row)?;
                working_bytes = next_total;
                groups.insert(&group_columns, row, key, state)?;
            }
            Ok(VisitControl::Continue)
        },
    )?;

    let states = if let Some(global) = global {
        vec![global]
    } else {
        groups.into_states()
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
        sort_rows_by(&mut rows, &plan.order_by);
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

/// The schema position of every column the plan reads from a row: its groups', its items', and its
/// predicate's.
fn read_columns(plan: &AggregatePlan, schema: &TableDefinition) -> Result<Vec<usize>> {
    fn predicate_columns<'a>(predicate: &'a Predicate, names: &mut Vec<&'a str>) {
        match predicate {
            Predicate::Comparison { column, .. }
            | Predicate::IsNull { column, .. }
            | Predicate::In { column, .. }
            | Predicate::Like { column, .. } => names.push(column),
            Predicate::And { predicates } | Predicate::Or { predicates } => {
                for predicate in predicates {
                    predicate_columns(predicate, names);
                }
            }
            Predicate::Not { predicate } => predicate_columns(predicate, names),
        }
    }
    let mut names: Vec<&str> = plan.group_by.iter().map(String::as_str).collect();
    for item in &plan.items {
        match &item.expression {
            SelectExpression::Column(column)
            | SelectExpression::Aggregate {
                argument: AggregateArgument::Column(column),
                ..
            } => names.push(column),
            SelectExpression::Aggregate {
                argument: AggregateArgument::Star,
                ..
            } => {}
        }
    }
    if let Some(predicate) = &plan.predicate {
        predicate_columns(predicate, &mut names);
    }
    let mut positions = Vec::with_capacity(names.len());
    for name in names {
        positions.push(
            schema
                .columns
                .iter()
                .position(|column| column.name == name)
                .ok_or_else(|| EngineError::column_not_found(name, &plan.table))?,
        );
    }
    Ok(positions)
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
            Ok(ResultField::new(
                crate::query::field_name(&item.output, plan.positional),
                data_type,
            ))
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

    let mut groups = KeySet::default();
    for column in &plan.group_by {
        if !groups.insert(column.as_str()) {
            return Err(EngineError::invalid_query(format!(
                "GROUP BY names column `{column}` more than once"
            )));
        }
        let definition = column_definition(schema, column, &plan.table)?;
        if definition.data_type == ColumnType::Json {
            return Err(EngineError::type_mismatch(format!(
                "JSON column `{column}` in `{}` cannot be {}",
                plan.table,
                if plan.distinct {
                    "compared by SELECT DISTINCT"
                } else {
                    "grouped"
                }
            )));
        }
    }

    let mut outputs = KeySet::default();
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

/// A column an aggregate reads: its name, its position in the schema, and its type.
struct SourceColumn {
    name: String,
    index: usize,
    data_type: ColumnType,
}

impl SourceColumn {
    fn new(schema: &TableDefinition, column: &str, table: &str) -> Result<Self> {
        let index = schema
            .columns
            .iter()
            .position(|definition| definition.name == column)
            .ok_or_else(|| EngineError::column_not_found(column, table))?;
        Ok(Self {
            name: column.to_owned(),
            index,
            data_type: schema.columns[index].data_type,
        })
    }

    /// The column's value in `row`, or `None` when it is `NULL`.
    fn non_null<'r>(&self, row: &RowRef<'r>) -> Result<Option<ValueRef<'r>>> {
        let value = row.get(self.index)?;
        Ok((!value.is_null()).then_some(value))
    }
}

struct GroupState {
    grouped_values: Row,
    aggregates: Vec<AggregateAccumulator>,
    /// Whether the state's size is settled once it exists: only an extremum's value can grow.
    settled: bool,
}

impl GroupState {
    fn new(
        plan: &AggregatePlan,
        schema: &TableDefinition,
        representative: Option<&RowRef<'_>>,
    ) -> Result<Self> {
        let mut grouped_values = Map::new();
        let mut aggregates = Vec::new();
        for item in &plan.items {
            match &item.expression {
                SelectExpression::Column(column) => {
                    let Some(row) = representative else {
                        return Err(EngineError::invalid_query(format!(
                            "Grouped column `{column}` has no representative row"
                        )));
                    };
                    let source = SourceColumn::new(schema, column, &plan.table)?;
                    grouped_values.insert(column.clone(), row.get(source.index)?.into_value());
                }
                SelectExpression::Aggregate { function, argument } => {
                    aggregates.push(AggregateAccumulator::new(*function, argument, schema)?)
                }
            }
        }
        let settled = aggregates
            .iter()
            .all(|aggregate| !matches!(aggregate, AggregateAccumulator::Extremum { .. }));
        Ok(Self {
            grouped_values,
            aggregates,
            settled,
        })
    }

    fn update(&mut self, row: &RowRef<'_>) -> Result<()> {
        for aggregate in &mut self.aggregates {
            aggregate.update(row)?;
        }
        Ok(())
    }

    /// Adds a row to the state, charging any growth to a work budget that holds `total` bytes, and
    /// returns the budget's new total.
    fn update_within(&mut self, row: &RowRef<'_>, total: usize) -> Result<usize> {
        if self.settled {
            self.update(row)?;
            return Ok(total);
        }
        let before = self.estimated_bytes()?;
        let after = self.estimated_bytes_after(row)?;
        let total = replace_budget_charge(total, before, after, MAX_AGGREGATE_WORK_BYTES)?;
        self.update(row)?;
        Ok(total)
    }

    fn estimated_bytes_after(&self, row: &RowRef<'_>) -> Result<usize> {
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
        column: Option<SourceColumn>,
        count: u64,
    },
    IntegerSum {
        column: SourceColumn,
        sum: i128,
        seen: bool,
    },
    FloatSum {
        column: SourceColumn,
        sum: f64,
        seen: bool,
    },
    Average {
        column: SourceColumn,
        sum: f64,
        count: u64,
    },
    Extremum {
        column: SourceColumn,
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
            AggregateArgument::Column(column) => {
                Some(SourceColumn::new(schema, column, &schema.name)?)
            }
        };
        if function == AggregateFunction::Count {
            return Ok(Self::Count { column, count: 0 });
        }
        let column = column.ok_or_else(|| {
            EngineError::unsupported_sql("Only COUNT accepts `*` as an aggregate argument")
        })?;
        match function {
            AggregateFunction::Count => unreachable!("COUNT returned above"),
            AggregateFunction::Sum if column.data_type == ColumnType::Integer => {
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
                sum: 0.0,
                count: 0,
            }),
            AggregateFunction::Min | AggregateFunction::Max => Ok(Self::Extremum {
                column,
                value: None,
                minimum: function == AggregateFunction::Min,
            }),
        }
    }

    fn update(&mut self, row: &RowRef<'_>) -> Result<()> {
        match self {
            Self::Count { column, count } => {
                let counts = match column {
                    None => true,
                    Some(column) => column.non_null(row)?.is_some(),
                };
                if counts {
                    *count = count.checked_add(1).ok_or_else(|| {
                        EngineError::new("NUMERIC_OVERFLOW", "COUNT result overflowed")
                    })?;
                }
            }
            Self::IntegerSum { column, sum, seen } => {
                if let Some(value) = column.non_null(row)? {
                    *sum = sum.checked_add(integer_input(&value)?).ok_or_else(|| {
                        EngineError::new("NUMERIC_OVERFLOW", "SUM result overflowed")
                    })?;
                    *seen = true;
                }
            }
            Self::FloatSum { column, sum, seen } => {
                if let Some(value) = column.non_null(row)? {
                    *sum += float_input(&value)?;
                    *seen = true;
                }
            }
            Self::Average { column, sum, count } => {
                if let Some(value) = column.non_null(row)? {
                    *sum += if column.data_type == ColumnType::Integer {
                        integer_input(&value)? as f64
                    } else {
                        float_input(&value)?
                    };
                    *count = count.checked_add(1).ok_or_else(|| {
                        EngineError::new("NUMERIC_OVERFLOW", "AVG count overflowed")
                    })?;
                }
            }
            Self::Extremum {
                column,
                value: selected,
                minimum,
            } => {
                if let Some(value) = column.non_null(row)?
                    && replaces_extremum(column.data_type, &value, selected.as_ref(), *minimum)
                {
                    *selected = Some(value.into_value());
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
            Self::Count { column, .. } => (column.as_ref(), None),
            Self::IntegerSum { column, .. }
            | Self::FloatSum { column, .. }
            | Self::Average { column, .. } => (Some(column), None),
            Self::Extremum { column, value, .. } => (Some(column), value.as_ref()),
        };
        let mut bytes = 96_usize;
        if let Some(column) = column {
            bytes = checked_add(bytes, checked_mul(column.name.len(), 2)?)?;
        }
        if let Some(value) = selected {
            bytes = checked_add(bytes, checked_mul(owned_value_bytes(value)?, 2)?)?;
        }
        Ok(bytes)
    }

    fn estimated_bytes_after(&self, row: &RowRef<'_>) -> Result<usize> {
        let mut bytes = self.estimated_bytes()?;
        let Self::Extremum {
            column,
            value: selected,
            minimum,
        } = self
        else {
            return Ok(bytes);
        };
        let Some(incoming) = column.non_null(row)? else {
            return Ok(bytes);
        };
        if replaces_extremum(column.data_type, &incoming, selected.as_ref(), *minimum) {
            if let Some(selected) = selected {
                bytes = bytes
                    .checked_sub(checked_mul(owned_value_bytes(selected)?, 2)?)
                    .ok_or_else(work_limit_error)?;
            }
            bytes = checked_add(
                bytes,
                checked_mul(incoming.owned_bytes(owned_value_bytes)?, 2)?,
            )?;
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

/// Whether a non-null input replaces the selected minimum or maximum.
fn replaces_extremum(
    data_type: ColumnType,
    incoming: &ValueRef<'_>,
    selected: Option<&Value>,
    minimum: bool,
) -> bool {
    selected.is_none_or(|selected| {
        // As `compare_typed` orders the equivalent JSON values.
        let ordering = match (data_type, incoming) {
            (_, ValueRef::Json(incoming)) => compare_typed(data_type, incoming, selected),
            (ColumnType::Integer | ColumnType::Float, incoming) => {
                let incoming = match incoming {
                    ValueRef::Integer(value) => Some(*value as f64),
                    ValueRef::Float(value) => Some(*value),
                    _ => None,
                };
                incoming
                    .partial_cmp(&selected.as_f64())
                    .unwrap_or(Ordering::Equal)
            }
            (ColumnType::Text, ValueRef::Text(incoming)) => incoming
                .as_ref()
                .cmp(selected.as_str().expect("typed text was validated")),
            _ => Ordering::Equal,
        };
        if minimum {
            ordering == Ordering::Less
        } else {
            ordering == Ordering::Greater
        }
    })
}

/// An integer aggregate input.
fn integer_input(value: &ValueRef<'_>) -> Result<i128> {
    match value {
        ValueRef::Integer(value) => Ok(i128::from(*value)),
        ValueRef::Json(value) => integer_value(value),
        _ => Err(EngineError::type_mismatch("Expected a typed integer value")),
    }
}

/// A float aggregate input.
fn float_input(value: &ValueRef<'_>) -> Result<f64> {
    match value {
        ValueRef::Float(value) => Ok(*value),
        ValueRef::Integer(value) => Ok(*value as f64),
        ValueRef::Json(value) => value
            .as_f64()
            .ok_or_else(|| EngineError::type_mismatch("Expected a typed float value")),
        _ => Err(EngineError::type_mismatch("Expected a typed float value")),
    }
}

fn ensure_group_key_input_budget(columns: &[SourceColumn], row: &RowRef<'_>) -> Result<()> {
    let mut bytes = 32_usize;
    for column in columns {
        let value = row.get(column.index)?;
        bytes = checked_add(bytes, 16)?;
        bytes = checked_add(bytes, checked_mul(column.name.len(), 2)?)?;
        // `group_key` JSON-escapes text into a tagged component and then encodes the component
        // vector. Control characters can therefore transiently expand to roughly thirteen times
        // their input length while both encodings coexist; fourteen is a conservative ceiling.
        bytes = checked_add(
            bytes,
            checked_mul(value.owned_bytes(owned_value_bytes)?, 14)?,
        )?;
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

/// A query's groups, found for each row by the values of its grouping columns without encoding
/// them, and ordered in the end by their encoded keys, as a map of those keys would order them.
#[derive(Default)]
struct Groups {
    entries: Vec<Group>,
    /// Each group's position, by the hash of its grouped values.
    index: KeyMap<u64, Vec<usize>>,
}

struct Group {
    values: Vec<GroupValue<'static>>,
    /// The group's encoded key as bytes, which order as the key does.
    key: Vec<u8>,
    state: GroupState,
}

impl Groups {
    /// The position of the group `row` belongs to, if it exists.
    fn find(&self, columns: &[SourceColumn], row: &RowRef<'_>) -> Result<Option<usize>> {
        let Some(candidates) = self.index.get(&group_hash(columns, row)?) else {
            return Ok(None);
        };
        for index in candidates {
            let mut same = true;
            for (column, value) in columns.iter().zip(&self.entries[*index].values) {
                if group_value(column, row.get(column.index)?)? != *value {
                    same = false;
                    break;
                }
            }
            if same {
                return Ok(Some(*index));
            }
        }
        Ok(None)
    }

    fn insert(
        &mut self,
        columns: &[SourceColumn],
        row: &RowRef<'_>,
        key: String,
        state: GroupState,
    ) -> Result<()> {
        let mut values = Vec::with_capacity(columns.len());
        for column in columns {
            values.push(group_value(column, row.get(column.index)?)?.into_owned());
        }
        self.index
            .entry(group_hash(columns, row)?)
            .or_default()
            .push(self.entries.len());
        self.entries.push(Group {
            values,
            key: key.into_bytes(),
            state,
        });
        Ok(())
    }

    fn into_states(self) -> Vec<GroupState> {
        // Sorting (key, position) pairs shares its code with the sort that builds indexes.
        let mut order = Vec::with_capacity(self.entries.len());
        let mut states = Vec::with_capacity(self.entries.len());
        for (position, group) in self.entries.into_iter().enumerate() {
            order.push((group.key, position));
            states.push(Some(group.state));
        }
        order.sort_unstable();
        order
            .into_iter()
            .filter_map(|(_, position)| states[position].take())
            .collect()
    }
}

/// A grouped value as [`group_key_part`] encodes it: two values group together exactly when they
/// encode alike.
#[derive(Debug, Hash, PartialEq)]
enum GroupValue<'a> {
    Null,
    Boolean(bool),
    Integer(i128),
    Float(u64),
    Text(Cow<'a, str>),
}

impl GroupValue<'_> {
    fn into_owned(self) -> GroupValue<'static> {
        match self {
            Self::Null => GroupValue::Null,
            Self::Boolean(value) => GroupValue::Boolean(value),
            Self::Integer(value) => GroupValue::Integer(value),
            Self::Float(bits) => GroupValue::Float(bits),
            Self::Text(text) => GroupValue::Text(Cow::Owned(text.into_owned())),
        }
    }
}

fn group_value<'a>(column: &SourceColumn, value: ValueRef<'a>) -> Result<GroupValue<'a>> {
    let float = |number: f64| GroupValue::Float(if number == 0.0 { 0.0 } else { number }.to_bits());
    Ok(match (column.data_type, value) {
        (_, ValueRef::Null) => GroupValue::Null,
        (ColumnType::Boolean, ValueRef::Boolean(value)) => GroupValue::Boolean(value),
        (ColumnType::Integer, ValueRef::Integer(value)) => GroupValue::Integer(i128::from(value)),
        (ColumnType::Float, ValueRef::Float(value)) => float(value),
        (ColumnType::Text, ValueRef::Text(text)) => GroupValue::Text(text),
        (data_type, ValueRef::Json(value)) => match (data_type, value.as_ref()) {
            (_, Value::Null) => GroupValue::Null,
            (ColumnType::Boolean, Value::Bool(value)) => GroupValue::Boolean(*value),
            (ColumnType::Integer, value) => GroupValue::Integer(integer_value(value)?),
            (ColumnType::Float, Value::Number(value)) => {
                float(value.as_f64().expect("typed float was validated"))
            }
            (ColumnType::Text, Value::String(text)) => GroupValue::Text(Cow::Owned(text.clone())),
            _ => return Err(incompatible_group_value(&column.name)),
        },
        _ => return Err(incompatible_group_value(&column.name)),
    })
}

fn incompatible_group_value(column: &str) -> EngineError {
    EngineError::type_mismatch(format!(
        "Column `{column}` contains a value incompatible with its catalog type"
    ))
}

fn group_hash(columns: &[SourceColumn], row: &RowRef<'_>) -> Result<u64> {
    let mut hasher = KeyHasher::default();
    for column in columns {
        group_value(column, row.get(column.index)?)?.hash(&mut hasher);
    }
    Ok(hasher.finish())
}

fn group_key(columns: &[SourceColumn], row: &RowRef<'_>) -> Result<String> {
    let mut parts = Vec::with_capacity(columns.len());
    for column in columns {
        let value = row.get(column.index)?;
        parts.push(match value {
            ValueRef::Json(value) => group_key_part(column.data_type, &value, &column.name)?,
            value => group_key_part_ref(column.data_type, &value, &column.name)?,
        });
    }
    encode_group_key(&parts)
}

/// [`group_key_part`] for a typed value read from a row.
fn group_key_part_ref(data_type: ColumnType, value: &ValueRef<'_>, column: &str) -> Result<String> {
    Ok(match (data_type, value) {
        (_, ValueRef::Null) => "null".to_owned(),
        (ColumnType::Boolean, ValueRef::Boolean(value)) => format!("b:{value}"),
        (ColumnType::Integer, ValueRef::Integer(value)) => format!("i:{value}"),
        (ColumnType::Float, ValueRef::Float(value)) => {
            let number = if *value == 0.0 { 0.0 } else { *value };
            format!("f:{:016x}", number.to_bits())
        }
        (ColumnType::Text, ValueRef::Text(value)) => {
            format!(
                "s:{}",
                serde_json::to_string(value.as_ref()).expect("strings encode")
            )
        }
        _ => {
            return Err(EngineError::type_mismatch(format!(
                "Column `{column}` contains a value incompatible with its catalog type"
            )));
        }
    })
}

/// Encodes one grouped value so that values SQL considers equal encode identically: `NULL`s group
/// together, integers ignore their JSON spelling, and floating-point zero ignores its sign.
pub(crate) fn group_key_part(data_type: ColumnType, value: &Value, column: &str) -> Result<String> {
    Ok(match (data_type, value) {
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
    })
}

pub(crate) fn encode_group_key(parts: &[String]) -> Result<String> {
    serde_json::to_string(parts)
        .map_err(|error| EngineError::invalid_query(format!("Could not encode group key: {error}")))
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
    mode: ParseMode,
}

impl<'a> Parser<'a> {
    fn new(tokens: Vec<Token>, params: &'a [Value], mode: ParseMode) -> Self {
        Self {
            tokens,
            position: 0,
            params,
            mode,
        }
    }

    fn parse(mut self) -> Result<AggregatePlan> {
        self.expect_keyword("select")?;
        let distinct = is_distinct_keyword_at(&self.tokens, self.position);
        if distinct {
            self.position += 1;
            if self.consume_keyword("on") {
                return Err(EngineError::unsupported_sql(
                    "SELECT DISTINCT ON is not supported",
                ));
            }
            if self.consume_star() {
                return Err(EngineError::unsupported_sql(
                    "SELECT DISTINCT requires an explicit column list",
                ));
            }
        }
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
        let mut group_by = if self.consume_keyword("group") {
            self.expect_keyword("by")?;
            self.parse_identifier_list(MAX_GROUP_COLUMNS, "GROUP BY")?
        } else {
            vec![]
        };
        if distinct {
            if !group_by.is_empty()
                || items
                    .iter()
                    .any(|item| matches!(item.expression, SelectExpression::Aggregate { .. }))
            {
                return Err(EngineError::unsupported_sql(
                    "SELECT DISTINCT cannot be combined with GROUP BY or aggregate functions",
                ));
            }
            for item in &items {
                let SelectExpression::Column(column) = &item.expression else {
                    unreachable!("aggregate items were rejected above");
                };
                if !group_by.contains(column) {
                    if group_by.len() == MAX_GROUP_COLUMNS {
                        return Err(EngineError::invalid_query(format!(
                            "SELECT DISTINCT cannot compare more than {MAX_GROUP_COLUMNS} columns"
                        )));
                    }
                    group_by.push(column.clone());
                }
            }
        }
        let order_by = if self.consume_keyword("order") {
            self.expect_keyword("by")?;
            self.parse_order_by(&items)?
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
                "This aggregate subset does not support HAVING, FILTER, windows, expressions, or joins",
            ));
        }
        Ok(AggregatePlan {
            table,
            distinct,
            items,
            positional: false,
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
            } else if is_distinct_keyword_at(&self.tokens, self.position) {
                return Err(EngineError::unsupported_sql(
                    "Aggregate DISTINCT, such as COUNT(DISTINCT column), is not supported",
                ));
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

    /// An aggregate query is ordered by its outputs. A plain name is an output's, and a name
    /// behind the table's qualifier is the table's column, so it stands for the output that
    /// returns that column.
    fn parse_order_by(&mut self, items: &[SelectItem]) -> Result<Vec<OrderBy>> {
        let mut orders = Vec::new();
        loop {
            if orders.len() >= MAX_ORDER_COLUMNS {
                return Err(EngineError::invalid_query(format!(
                    "A query cannot order by more than {MAX_ORDER_COLUMNS} columns"
                )));
            }
            let column = if let Some(Token::Column(column)) = self.tokens.get(self.position) {
                let output = items
                    .iter()
                    .find(|item| {
                        matches!(&item.expression, SelectExpression::Column(name) if name == column)
                    })
                    .map(|item| item.output.clone())
                    .ok_or_else(|| EngineError::column_not_found(column, "aggregate output"))?;
                self.position += 1;
                output
            } else {
                self.parse_identifier()?
            };
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
        pagination_value(&value, self.mode)
    }

    fn parse_value(&mut self) -> Result<Value> {
        let Some(token) = self.next() else {
            return Err(EngineError::parse_error("Expected a SQL value"));
        };
        match token {
            Token::Number(value) => number_literal(&value).map(Value::Number).ok_or_else(|| {
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

    use crate::row::RowRef;
    use crate::{ColumnType, Engine, Row, RowChange, StorageDriver, StorageReader};

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
    fn groups_and_orders_rows_as_their_encoded_keys_do() {
        // Groups are found by their values, but come out in the order of their encoded keys, as
        // a map of those keys orders them: the result must match that model exactly.
        let mut engine = Engine::default();
        engine
            .execute_sql(
                "CREATE TABLE grouped (id INTEGER PRIMARY KEY, i INTEGER, t TEXT, b BOOLEAN, \
                 f DOUBLE PRECISION)",
                &[],
            )
            .unwrap();
        let texts = [
            None,
            Some("a"),
            Some("b"),
            Some("é"),
            Some("a\"b"),
            Some(""),
            Some("10"),
        ];
        let floats = [None, Some(0.0), Some(-0.0), Some(1.5), Some(-2.25)];
        let mut rows = Vec::new();
        for id in 0..300_i64 {
            let i = (id % 7 != 0).then_some((id * 37) % 11 - 5);
            let t = texts[(id * 13 % 7) as usize];
            let b = (id % 5 != 0).then_some(id % 3 == 0);
            let f = floats[(id * 7 % 5) as usize];
            rows.push((id, i, t, b, f));
        }
        let literal = |value: Option<String>| value.unwrap_or_else(|| "NULL".to_owned());
        let values = rows
            .iter()
            .map(|(id, i, t, b, f)| {
                format!(
                    "({id}, {}, {}, {}, {})",
                    literal(i.map(|i| i.to_string())),
                    literal(t.map(|t| format!("'{}'", t.replace('\'', "''")))),
                    literal(b.map(|b| b.to_string())),
                    literal(f.map(|f| format!("{f:?}"))),
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        engine
            .execute_sql(
                &format!("INSERT INTO grouped (id, i, t, b, f) VALUES {values}"),
                &[],
            )
            .unwrap();

        let value = |value: Option<serde_json::Value>| value.unwrap_or(Value::Null);
        for (columns, types) in [
            (vec!["i", "t"], vec![ColumnType::Integer, ColumnType::Text]),
            (
                vec!["b", "f", "t"],
                vec![ColumnType::Boolean, ColumnType::Float, ColumnType::Text],
            ),
            (vec!["f"], vec![ColumnType::Float]),
        ] {
            let mut model = std::collections::BTreeMap::<String, (Vec<Value>, i64)>::new();
            for (_, i, t, b, f) in &rows {
                let row_values = columns
                    .iter()
                    .map(|column| match *column {
                        "i" => value(i.map(|i| json!(i))),
                        "t" => value(t.map(|t| json!(t))),
                        "b" => value(b.map(|b| json!(b))),
                        _ => value(f.map(|f| json!(f))),
                    })
                    .collect::<Vec<_>>();
                let parts = columns
                    .iter()
                    .zip(&types)
                    .zip(&row_values)
                    .map(|((column, data_type), value)| {
                        super::group_key_part(*data_type, value, column).unwrap()
                    })
                    .collect::<Vec<_>>();
                model
                    .entry(super::encode_group_key(&parts).unwrap())
                    .or_insert((row_values, 0))
                    .1 += 1;
            }
            let listed = columns.join(", ");
            let result = engine
                .query_sql(
                    &format!("SELECT {listed}, COUNT(*) AS rows FROM grouped GROUP BY {listed}"),
                    &[],
                )
                .unwrap();
            let expected = model
                .into_values()
                .map(|(values, count)| {
                    let mut row = columns
                        .iter()
                        .map(|column| column.to_string())
                        .zip(values)
                        .collect::<Row>();
                    row.insert("rows".to_owned(), json!(count));
                    row
                })
                .collect::<Vec<_>>();
            assert_eq!(result.rows.len(), expected.len(), "{listed}");
            for (actual, expected) in result.rows.iter().zip(&expected) {
                for (column, expected) in expected {
                    let actual = &actual[column];
                    // A group's FLOAT zero is whichever zero came first; both are one group.
                    let same = actual == expected
                        || actual
                            .as_f64()
                            .zip(expected.as_f64())
                            .is_some_and(|(a, e)| a == e);
                    assert!(same, "{listed}: {column} {actual} != {expected}");
                }
            }
        }
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
        let representative = RowRef::map(&representative, &schema);
        let mut state = super::GroupState::new(&plan, &schema, Some(&representative)).unwrap();
        state.update(&representative).unwrap();

        let before = state.estimated_bytes().unwrap();
        let larger = row(json!({"region": "r", "label": "a much longer value"}));
        state.update(&RowRef::map(&larger, &schema)).unwrap();
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
    fn select_distinct_removes_duplicate_projected_rows() {
        let mut engine = sales();
        engine
            .execute_sql(
                "INSERT INTO sales (id, region, amount, score, label, active) VALUES \
                 (5, 'east', 10, -0.0, 'b', true), (6, NULL, 5, 0.0, 'e', false)",
                &[],
            )
            .unwrap();
        fn query(engine: &Engine, sql: &str, params: &[Value]) -> Vec<Row> {
            engine.query_sql(sql, params).unwrap().rows
        }

        // NULLs compare as equal, and so do an integer or float spelled differently.
        assert_eq!(
            query(
                &engine,
                "SELECT DISTINCT region FROM sales ORDER BY region NULLS FIRST",
                &[]
            ),
            vec![
                row(json!({"region": null})),
                row(json!({"region": "east"})),
                row(json!({"region": "west"})),
            ]
        );
        assert_eq!(
            query(
                &engine,
                "SELECT DISTINCT region AS area, amount FROM sales \
                 WHERE amount IS NOT NULL ORDER BY area, amount DESC",
                &[],
            ),
            vec![
                row(json!({"area": "east", "amount": 20})),
                row(json!({"area": "east", "amount": 10})),
                row(json!({"area": null, "amount": 5})),
            ]
        );
        assert_eq!(
            query(
                &engine,
                "SELECT DISTINCT score FROM sales WHERE score <= $1 ORDER BY score",
                &[json!(0)],
            )
            .len(),
            1
        );
        // A repeated column adds no distinguishing value, and LIMIT/OFFSET apply to distinct rows.
        assert_eq!(
            query(
                &engine,
                "SELECT DISTINCT active, active AS again FROM sales ORDER BY active LIMIT 1 OFFSET 1",
                &[],
            ),
            vec![row(json!({"active": true, "again": true}))]
        );
        let empty = engine
            .query_sql("SELECT DISTINCT label FROM sales WHERE id > 99", &[])
            .unwrap();
        assert_eq!(
            empty.fields,
            vec![crate::ResultField::new("label", crate::ColumnType::Text)]
        );
        assert!(empty.rows.is_empty());

        // `distinct` stays usable as a column name wherever it cannot be the keyword.
        engine
            .execute_sql(
                "CREATE TABLE words (id INTEGER PRIMARY KEY, distinct TEXT)",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO words VALUES (1, 'x'), (2, 'x'), (3, NULL)",
                &[],
            )
            .unwrap();
        for (sql, expected) in [
            ("SELECT distinct FROM words ORDER BY id", 3),
            ("SELECT distinct, id FROM words", 3),
            ("SELECT distinct AS word FROM words", 3),
            ("SELECT DISTINCT distinct FROM words", 2),
            ("SELECT COUNT(distinct) AS n FROM words", 1),
        ] {
            assert_eq!(query(&engine, sql, &[]).len(), expected, "{sql}");
        }
        assert_eq!(
            query(&engine, "SELECT COUNT(distinct) AS n FROM words", &[]),
            vec![row(json!({"n": 2}))]
        );

        for (sql, code) in [
            ("SELECT DISTINCT * FROM sales", "UNSUPPORTED_SQL"),
            (
                "SELECT DISTINCT ON (region) region FROM sales",
                "UNSUPPORTED_SQL",
            ),
            (
                "SELECT DISTINCT COUNT(*) AS n FROM sales",
                "UNSUPPORTED_SQL",
            ),
            (
                "SELECT DISTINCT region FROM sales GROUP BY region",
                "UNSUPPORTED_SQL",
            ),
            (
                "SELECT COUNT(DISTINCT region) AS n FROM sales",
                "UNSUPPORTED_SQL",
            ),
            ("SELECT DISTINCT metadata FROM sales", "TYPE_MISMATCH"),
            (
                "SELECT DISTINCT region FROM sales ORDER BY amount",
                "COLUMN_NOT_FOUND",
            ),
            (
                "SELECT DISTINCT region AS r, label AS r FROM sales",
                "INVALID_QUERY",
            ),
        ] {
            assert_eq!(
                engine.query_sql(sql, &[]).unwrap_err().code,
                code,
                "query was `{sql}`"
            );
        }

        // The distinct-column bound counts source columns, so repeating one column never reaches it.
        let repeated = (0..=super::MAX_GROUP_COLUMNS)
            .map(|index| format!("region AS r{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        engine
            .query_sql(&format!("SELECT DISTINCT {repeated} FROM sales"), &[])
            .unwrap();
        let columns = (0..=super::MAX_GROUP_COLUMNS)
            .map(|index| format!("c{index}"))
            .collect::<Vec<_>>();
        engine
            .execute_sql(
                &format!(
                    "CREATE TABLE wide (id INTEGER PRIMARY KEY, {} INTEGER)",
                    columns.join(" INTEGER, ")
                ),
                &[],
            )
            .unwrap();
        assert_eq!(
            engine
                .query_sql(
                    &format!("SELECT DISTINCT {} FROM wide", columns.join(", ")),
                    &[]
                )
                .unwrap_err()
                .code,
            "INVALID_QUERY"
        );
        engine
            .query_sql(
                &format!("SELECT DISTINCT {} FROM wide", columns[1..].join(", ")),
                &[],
            )
            .unwrap();
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

    /// Counts how a grouped query reached its rows, so index narrowing is observable rather than
    /// inferred from timing.
    #[derive(Clone, Debug)]
    struct CountingStorage {
        inner: crate::InMemoryStorage,
        table_scans: std::cell::Cell<usize>,
        index_scans: std::cell::Cell<usize>,
        rows_visited: std::cell::Cell<usize>,
    }

    impl CountingStorage {
        fn new(inner: crate::InMemoryStorage) -> Self {
            Self {
                inner,
                table_scans: std::cell::Cell::new(0),
                index_scans: std::cell::Cell::new(0),
                rows_visited: std::cell::Cell::new(0),
            }
        }
    }

    impl StorageReader for CountingStorage {
        fn visit_table(
            &self,
            table: &str,
            visitor: &mut dyn FnMut(&crate::row::RowRef<'_>) -> crate::Result<crate::VisitControl>,
        ) -> crate::Result<crate::VisitOutcome> {
            self.table_scans.set(self.table_scans.get() + 1);
            self.inner.visit_table(table, &mut |row| {
                self.rows_visited.set(self.rows_visited.get() + 1);
                visitor(row)
            })
        }

        fn visit_index(
            &self,
            table: &str,
            columns: &[String],
            key: &Row,
            visitor: &mut dyn FnMut(&crate::row::RowRef<'_>) -> crate::Result<crate::VisitControl>,
        ) -> crate::Result<Option<crate::VisitOutcome>> {
            let outcome = self.inner.visit_index(table, columns, key, &mut |row| {
                self.rows_visited.set(self.rows_visited.get() + 1);
                visitor(row)
            })?;
            if outcome.is_some() {
                self.index_scans.set(self.index_scans.get() + 1);
            }
            Ok(outcome)
        }

        fn table_row_count(&self, table: &str) -> crate::Result<usize> {
            self.inner.table_row_count(table)
        }

        fn lookup_primary_key(&self, table: &str, key: &Row) -> crate::Result<Option<Row>> {
            self.inner.lookup_primary_key(table, key)
        }

        fn index_definition(&self, name: &str) -> Option<crate::IndexDefinition> {
            self.inner.index_definition(name)
        }

        fn indexes_for_table(&self, table: &str) -> crate::Result<Vec<crate::IndexDefinition>> {
            self.inner.indexes_for_table(table)
        }

        fn table_schema(&self, table: &str) -> crate::Result<std::rc::Rc<crate::TableDefinition>> {
            self.inner.table_schema(table)
        }

        fn revision(&self) -> u64 {
            self.inner.revision()
        }
    }

    /// The atom-tree child listing a sync connector issues is
    /// `SELECT child FROM t WHERE parent = $1 GROUP BY child`. It must cost the matching
    /// subtree, not the whole table, or listing one parent's children scans every atom.
    fn atoms() -> Engine<CountingStorage> {
        let mut engine = Engine::default();
        engine
            .execute_sql(
                "CREATE TABLE atoms (address TEXT PRIMARY KEY, parent TEXT, child TEXT)",
                &[],
            )
            .unwrap();
        engine
            .execute_sql("CREATE INDEX atoms_parent ON atoms (parent)", &[])
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO atoms (address, parent, child) VALUES \
                 ('a/x', 'a', 'x'), ('a/x2', 'a', 'x'), ('a/y', 'a', 'y'), \
                 ('b/p', 'b', 'p'), ('b/q', 'b', 'q'), ('b/r', 'b', 'r')",
                &[],
            )
            .unwrap();
        Engine::new(CountingStorage::new(engine.into_storage()))
    }

    #[test]
    fn a_grouped_query_narrows_through_an_index_on_its_equality_predicate() {
        let engine = atoms();
        let result = engine
            .query_sql(
                "SELECT child FROM atoms WHERE parent = $1 GROUP BY child ORDER BY child",
                &[json!("a")],
            )
            .unwrap();

        assert_eq!(
            result.rows,
            vec![row(json!({"child": "x"})), row(json!({"child": "y"}))]
        );
        let storage = engine.into_storage();
        assert_eq!(storage.index_scans.get(), 1);
        assert_eq!(storage.table_scans.get(), 0);
        // Only the three rows under `a`, not all six in the table.
        assert_eq!(storage.rows_visited.get(), 3);
    }

    #[test]
    fn a_grouped_query_without_a_usable_index_still_scans_the_table() {
        let engine = atoms();
        let result = engine
            .query_sql(
                "SELECT parent FROM atoms WHERE child = $1 GROUP BY parent",
                &[json!("x")],
            )
            .unwrap();

        assert_eq!(result.rows, vec![row(json!({"parent": "a"}))]);
        let storage = engine.into_storage();
        assert_eq!(storage.index_scans.get(), 0);
        assert_eq!(storage.table_scans.get(), 1);
        assert_eq!(storage.rows_visited.get(), 6);
    }

    #[test]
    fn index_narrowing_does_not_change_aggregate_results() {
        let engine = atoms();
        let narrowed = engine
            .query_sql(
                "SELECT COUNT(*) AS rows, MIN(child) AS lowest FROM atoms WHERE parent = $1",
                &[json!("a")],
            )
            .unwrap();
        assert_eq!(narrowed.rows, vec![row(json!({"rows": 3, "lowest": "x"}))]);

        // The same predicate expressed so no index covers it must agree.
        let scanned = engine
            .query_sql(
                "SELECT COUNT(*) AS rows, MIN(child) AS lowest FROM atoms \
                 WHERE parent >= $1 AND parent <= $1",
                &[json!("a")],
            )
            .unwrap();
        assert_eq!(scanned.rows, narrowed.rows);
    }

    /// Paged storage, counting the rows and the index entries a query reads.
    struct EntryProbe {
        storage: crate::PagedStorage<crate::MemoryPageDevice>,
        rows: std::cell::Cell<usize>,
        entries: std::cell::Cell<usize>,
    }

    impl EntryProbe {
        fn new(sql: &str) -> Self {
            let mut storage =
                crate::PagedStorage::open(crate::MemoryPageDevice::new(0).unwrap()).unwrap();
            let statements = sql
                .split(';')
                .map(|sql| crate::statement::parse(sql, &[]))
                .collect::<crate::Result<Vec<_>>>()
                .unwrap();
            storage.execute_script(statements).unwrap();
            Self {
                storage,
                rows: std::cell::Cell::new(0),
                entries: std::cell::Cell::new(0),
            }
        }

        /// The query's rows, and how many rows and index entries it read.
        fn query(&self, sql: &str) -> (Vec<Row>, usize, usize) {
            self.rows.set(0);
            self.entries.set(0);
            let crate::statement::Statement::Aggregate(plan) =
                crate::statement::parse(sql, &[]).unwrap()
            else {
                panic!("not an aggregate: {sql}");
            };
            let rows = super::execute(self, &plan).unwrap().rows;
            (rows, self.rows.get(), self.entries.get())
        }

        fn counted<'v>(
            &'v self,
            visitor: &'v mut dyn FnMut(&RowRef<'_>) -> crate::Result<crate::VisitControl>,
        ) -> impl FnMut(&RowRef<'_>) -> crate::Result<crate::VisitControl> + 'v {
            move |row| {
                self.rows.set(self.rows.get() + 1);
                visitor(row)
            }
        }
    }

    impl StorageReader for EntryProbe {
        fn visit_table(
            &self,
            table: &str,
            visitor: &mut dyn FnMut(&RowRef<'_>) -> crate::Result<crate::VisitControl>,
        ) -> crate::Result<crate::VisitOutcome> {
            self.storage.visit_table(table, &mut self.counted(visitor))
        }

        fn visits_in_key_order(&self, table: &str) -> bool {
            self.storage.visits_in_key_order(table)
        }

        fn visit_table_range(
            &self,
            table: &str,
            range: &crate::storage::KeyRange,
            order: crate::storage::KeyOrder,
            visitor: &mut dyn FnMut(&RowRef<'_>) -> crate::Result<crate::VisitControl>,
        ) -> crate::Result<crate::VisitOutcome> {
            self.storage
                .visit_table_range(table, range, order, &mut self.counted(visitor))
        }

        fn table_row_count(&self, table: &str) -> crate::Result<usize> {
            self.storage.table_row_count(table)
        }

        fn lookup_primary_key(&self, table: &str, key: &Row) -> crate::Result<Option<Row>> {
            self.storage.lookup_primary_key(table, key)
        }

        fn index_definition(&self, name: &str) -> Option<crate::IndexDefinition> {
            self.storage.index_definition(name)
        }

        fn indexes_for_table(&self, table: &str) -> crate::Result<Vec<crate::IndexDefinition>> {
            self.storage.indexes_for_table(table)
        }

        fn visit_index(
            &self,
            table: &str,
            columns: &[String],
            key: &Row,
            visitor: &mut dyn FnMut(&RowRef<'_>) -> crate::Result<crate::VisitControl>,
        ) -> crate::Result<Option<crate::VisitOutcome>> {
            self.storage
                .visit_index(table, columns, key, &mut self.counted(visitor))
        }

        fn visit_index_range(
            &self,
            table: &str,
            columns: &[String],
            range: &crate::storage::KeyRange,
            limit: usize,
            visitor: &mut dyn FnMut(&RowRef<'_>) -> crate::Result<crate::VisitControl>,
        ) -> crate::Result<Option<crate::VisitOutcome>> {
            self.storage
                .visit_index_range(table, columns, range, limit, &mut self.counted(visitor))
        }

        fn visit_index_entries(
            &self,
            table: &str,
            columns: &[String],
            range: &crate::storage::KeyRange,
            layout: &crate::paged_codec::IndexEntryLayout,
            visitor: &mut dyn FnMut(&RowRef<'_>) -> crate::Result<crate::VisitControl>,
        ) -> crate::Result<Option<crate::VisitOutcome>> {
            self.storage
                .visit_index_entries(table, columns, range, layout, &mut |row| {
                    self.entries.set(self.entries.get() + 1);
                    visitor(row)
                })
        }

        fn table_schema(&self, table: &str) -> crate::Result<std::rc::Rc<crate::TableDefinition>> {
            self.storage.table_schema(table)
        }

        fn revision(&self) -> u64 {
            self.storage.revision()
        }
    }

    #[test]
    fn an_index_holding_every_column_a_query_reads_answers_it_from_its_entries() {
        let mut sql = String::from(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, b INTEGER, c TEXT NOT NULL, f FLOAT, g INTEGER); \
             CREATE INDEX t_b ON t (b); CREATE INDEX t_c ON t (c)",
        );
        for id in 0..60 {
            let b = if id % 7 == 0 {
                "NULL".to_owned()
            } else {
                (id % 45).to_string()
            };
            // Text with an embedded NUL exercises the key encoding's escape.
            let c = format!("{}{}", ["alpha", "b\u{0}eta", "gamma"][id % 3], id % 5);
            sql.push_str(&format!(
                "; INSERT INTO t VALUES ({id}, {b}, '{c}', {}, {})",
                f64::from(id as u32) / 4.0 - 3.0,
                id % 4
            ));
        }
        let probe = EntryProbe::new(&sql);
        for (covered, scanned) in [
            (
                "SELECT count(*) AS n, sum(b) AS s, avg(b) AS a, min(b) AS lo, max(b) AS hi \
                 FROM t WHERE b >= 10 AND b < 40",
                "SELECT count(*) AS n, sum(b) AS s, avg(b) AS a, min(b) AS lo, max(b) AS hi \
                 FROM t WHERE (b >= 10 AND b < 40) OR g = 99",
            ),
            (
                "SELECT b, count(*) AS n, max(id) AS top FROM t WHERE b >= 30 GROUP BY b ORDER BY b",
                "SELECT b, count(*) AS n, max(id) AS top FROM t WHERE b >= 30 OR g = 99 \
                 GROUP BY b ORDER BY b",
            ),
            (
                "SELECT min(c) AS lo, max(c) AS hi, count(*) AS n FROM t WHERE c >= 'b'",
                "SELECT min(c) AS lo, max(c) AS hi, count(*) AS n FROM t WHERE c >= 'b' OR g = 99",
            ),
        ] {
            let (rows, read, entries) = probe.query(covered);
            assert_eq!(read, 0, "{covered}");
            assert!(entries > 0, "{covered}");
            let (expected, read, entries) = probe.query(scanned);
            assert_eq!((read, entries), (60, 0), "{scanned}");
            assert_eq!(rows, expected, "{covered}");
            assert!(
                rows.iter()
                    .any(|row| row.values().any(|value| !value.is_null()))
            );
        }

        // A column the index does not hold is read from the rows.
        let (_, read, entries) = probe.query("SELECT sum(g) AS s FROM t WHERE b >= 10 AND b < 40");
        assert!(read > 0);
        assert_eq!(entries, 0);
    }
}
