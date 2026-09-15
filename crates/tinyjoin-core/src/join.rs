use std::cmp::Ordering;
use std::collections::HashSet;
use std::str::FromStr;

use serde_json::{Map, Number, Value};

use crate::query::{
    ParseMode, Token, bind_parameter, is_reserved_keyword, matches_predicate, pagination_value,
    parse_predicate_at,
};
use crate::storage::StorageReader;
use crate::{
    ColumnDefinition, ColumnType, ComparisonOperator, EngineError, NullOrder, OrderDirection,
    Predicate, QueryResult, Result, ResultField, Row, TableDefinition, VisitControl, VisitOutcome,
};

const MAX_PROJECTIONS: usize = 256;
const MAX_ON_TERMS: usize = 32;
const MAX_ORDER_COLUMNS: usize = 32;
const MAX_JOIN_SOURCES: usize = 8;
const MAX_JOIN_BUILD_ROWS: usize = 100_000;
const MAX_JOIN_PAIRS: usize = 1_000_000;
const MAX_SCAN_ROWS: usize = 1_000_000;
const MAX_RESULT_ROWS: usize = 100_000;
const MAX_JOIN_WORK_BYTES: usize = 16 * 1024 * 1024;
const MAX_JOIN_RESULT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ColumnRef {
    qualifier: Option<String>,
    column: String,
}

#[derive(Clone, Debug)]
struct Source {
    table: String,
    alias: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JoinKind {
    Inner,
    Left,
}

#[derive(Clone, Debug)]
struct JoinCondition {
    left: ColumnRef,
    right: ColumnRef,
}

#[derive(Clone, Debug)]
struct JoinStage {
    source: Source,
    kind: JoinKind,
    conditions: Vec<JoinCondition>,
}

#[derive(Clone, Debug)]
struct Projection {
    source: ColumnRef,
    output: String,
}

#[derive(Clone, Debug)]
struct JoinOrder {
    source: OrderSource,
    direction: OrderDirection,
    nulls: NullOrder,
}

#[derive(Clone, Debug)]
enum OrderSource {
    Column(ColumnRef),
    Output(String),
}

#[derive(Clone, Debug)]
pub(crate) struct JoinPlan {
    projections: Vec<Projection>,
    first: Source,
    joins: Vec<JoinStage>,
    predicate: Option<Predicate>,
    order_by: Vec<JoinOrder>,
    limit: Option<usize>,
    offset: usize,
}

#[derive(Clone)]
struct Relation {
    source: Source,
    schema: TableDefinition,
}

struct OrderedJoinedRow {
    row: Row,
    keys: Vec<Value>,
    projected_bytes: usize,
    ordinal: usize,
}

#[derive(Default)]
struct WorkBudget {
    retained: usize,
}

impl WorkBudget {
    fn retain(&mut self, bytes: usize) -> Result<()> {
        let retained = checked_add(self.retained, bytes)?;
        ensure_work_budget(retained)?;
        self.retained = retained;
        Ok(())
    }

    fn ensure_transient(&self, bytes: usize) -> Result<()> {
        ensure_work_budget(checked_add(self.retained, bytes)?)
    }
}

pub(crate) fn is_join_select(tokens: &[Token]) -> bool {
    tokens.iter().any(|token| {
        matches!(
            token,
            Token::Identifier {
                value,
                quoted: false,
            } if value.eq_ignore_ascii_case("join")
        )
    })
}

#[cfg(test)]
pub(crate) fn parse_sql(sql: &str, params: &[Value]) -> Result<JoinPlan> {
    crate::query::validate_sql_input(sql, params)?;
    let tokens = crate::query::tokenize(sql)?;
    crate::query::validate_parameter_expansion(&tokens, params)?;
    parse_tokens(tokens, params, ParseMode::Bound)
}

pub(crate) fn parse_tokens(
    tokens: Vec<Token>,
    params: &[Value],
    mode: ParseMode,
) -> Result<JoinPlan> {
    Parser::new(tokens, params, mode).parse()
}

pub(crate) fn bind_plan_parameters(
    plan: &JoinPlan,
    params: &[Value],
    limit_parameter: Option<usize>,
    offset_parameter: Option<usize>,
) -> Result<JoinPlan> {
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

pub(crate) fn execute(storage: &dyn StorageReader, plan: &JoinPlan) -> Result<QueryResult> {
    let mut relations = Vec::with_capacity(plan.joins.len() + 1);
    relations.push(Relation {
        schema: storage.table_schema(&plan.first.table)?,
        source: plan.first.clone(),
    });
    for join in &plan.joins {
        relations.push(Relation {
            schema: storage.table_schema(&join.source.table)?,
            source: join.source.clone(),
        });
    }
    let (conditions, fields) = validate_plan(plan, &relations)?;

    // LIMIT 0 remains a validation-only operation, matching the other SELECT
    // executors. Other joins preflight table sizes and count candidate pairs
    // as they are examined, including across successive join stages.
    if plan.limit == Some(0) {
        return Ok(QueryResult {
            revision: storage.revision(),
            fields,
            rows: Vec::new(),
        });
    }

    let counts = relations
        .iter()
        .map(|relation| storage.table_row_count(&relation.source.table))
        .collect::<Result<Vec<_>>>()?;
    let scan_rows = counts
        .iter()
        .fold(0_usize, |total, count| total.saturating_add(*count));
    if scan_rows > MAX_SCAN_ROWS {
        return Err(scan_limit_error());
    }
    preflight_build_rows(&counts)?;

    let rows = if plan.order_by.is_empty() {
        execute_unordered(storage, plan, &relations, &conditions)?
    } else {
        execute_ordered(storage, plan, &relations, &conditions)?
    };

    Ok(QueryResult {
        revision: storage.revision(),
        fields,
        rows,
    })
}

fn preflight_build_rows(counts: &[usize]) -> Result<()> {
    if counts[1..]
        .iter()
        .fold(0_usize, |total, count| total.saturating_add(*count))
        > MAX_JOIN_BUILD_ROWS
    {
        return Err(build_rows_limit_error());
    }
    Ok(())
}

fn execute_unordered(
    storage: &dyn StorageReader,
    plan: &JoinPlan,
    relations: &[Relation],
    conditions: &[Vec<ResolvedCondition>],
) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    let mut skipped = 0_usize;
    let mut result_bytes = 0_usize;
    visit_joined_rows(
        storage,
        plan,
        relations,
        conditions,
        &mut |bindings, budget| {
            if !matches_joined_predicate(bindings, plan, relations, budget)? {
                return Ok(VisitControl::Continue);
            }
            if skipped < plan.offset {
                skipped += 1;
                return Ok(VisitControl::Continue);
            }
            if rows.len() == MAX_RESULT_ROWS {
                return Err(result_rows_limit_error());
            }
            let projected_bytes = projected_row_bytes(bindings, plan, relations)?;
            let next_result_bytes = checked_add(result_bytes, projected_bytes)?;
            ensure_result_budget(next_result_bytes)?;
            rows.push(project_joined_row(bindings, plan, relations)?);
            result_bytes = next_result_bytes;
            if plan.limit.is_some_and(|limit| rows.len() == limit) {
                Ok(VisitControl::Stop)
            } else {
                Ok(VisitControl::Continue)
            }
        },
    )?;
    Ok(rows)
}

fn execute_ordered(
    storage: &dyn StorageReader,
    plan: &JoinPlan,
    relations: &[Relation],
    conditions: &[Vec<ResolvedCondition>],
) -> Result<Vec<Row>> {
    let mut joined_rows = Vec::new();
    visit_joined_rows(
        storage,
        plan,
        relations,
        conditions,
        &mut |bindings, budget| {
            if !matches_joined_predicate(bindings, plan, relations, budget)? {
                return Ok(VisitControl::Continue);
            }
            if joined_rows.len() == MAX_RESULT_ROWS {
                return Err(result_rows_limit_error());
            }
            let projected_bytes = projected_row_bytes(bindings, plan, relations)?;
            budget.retain(ordered_row_bytes(
                projected_bytes,
                order_keys_bytes(bindings, plan, relations)?,
            )?)?;
            joined_rows.push(OrderedJoinedRow {
                row: project_joined_row(bindings, plan, relations)?,
                keys: order_keys(bindings, plan, relations)?,
                projected_bytes,
                ordinal: joined_rows.len(),
            });
            Ok(VisitControl::Continue)
        },
    )?;

    joined_rows.sort_unstable_by(|first, second| compare_joined_rows(first, second, plan));

    let mut rows = Vec::new();
    let mut result_bytes = 0_usize;
    for joined in joined_rows
        .into_iter()
        .skip(plan.offset)
        .take(plan.limit.unwrap_or(usize::MAX))
    {
        let next_result_bytes = checked_add(result_bytes, joined.projected_bytes)?;
        ensure_result_budget(next_result_bytes)?;
        rows.push(joined.row);
        result_bytes = next_result_bytes;
    }
    Ok(rows)
}

#[allow(clippy::too_many_arguments)]
fn visit_joined_rows(
    storage: &dyn StorageReader,
    plan: &JoinPlan,
    relations: &[Relation],
    conditions: &[Vec<ResolvedCondition>],
    visitor: &mut impl for<'a> FnMut(&[Option<&'a Row>], &mut WorkBudget) -> Result<VisitControl>,
) -> Result<()> {
    let mut budget = WorkBudget::default();
    let mut scanned = 0_usize;
    let mut build_count = 0_usize;
    let mut build_tables = Vec::with_capacity(plan.joins.len());
    for relation in &relations[1..] {
        let mut rows = Vec::new();
        let outcome = storage.visit_table(&relation.source.table, &mut |row| {
            count_scanned_row(&mut scanned)?;
            build_count = build_count.saturating_add(1);
            if build_count > MAX_JOIN_BUILD_ROWS {
                return Err(build_rows_limit_error());
            }
            budget.retain(owned_row_bytes(row)?)?;
            rows.push(row.clone());
            Ok(VisitControl::Continue)
        })?;
        if outcome != VisitOutcome::Complete {
            return Err(malformed_reader_error(
                "A join build visitor stopped before completing",
            ));
        }
        build_tables.push(rows);
    }

    let mut pairs = 0_usize;
    let mut stop_requested = false;
    let probe_outcome = storage.visit_table(&relations[0].source.table, &mut |probe| {
        if stop_requested {
            return Ok(VisitControl::Stop);
        }
        count_scanned_row(&mut scanned)?;
        let mut bindings = [None; MAX_JOIN_SOURCES];
        bindings[0] = Some(probe);
        if visit_extensions(
            storage,
            0,
            plan,
            &build_tables,
            conditions,
            &mut bindings[..relations.len()],
            &mut pairs,
            &mut budget,
            visitor,
        )? == VisitControl::Stop
        {
            stop_requested = true;
            return Ok(VisitControl::Stop);
        }
        Ok(VisitControl::Continue)
    })?;
    match (probe_outcome, stop_requested) {
        (VisitOutcome::Complete, false) | (VisitOutcome::Stopped, true) => Ok(()),
        (VisitOutcome::Stopped, false) => Err(malformed_reader_error(
            "The left-side table visitor stopped without being asked",
        )),
        (VisitOutcome::Complete, true) => Err(malformed_reader_error(
            "The left-side table visitor ignored a stop request",
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn visit_extensions<'a>(
    storage: &dyn StorageReader,
    stage: usize,
    plan: &JoinPlan,
    build_tables: &'a [Vec<Row>],
    conditions: &[Vec<ResolvedCondition>],
    bindings: &mut [Option<&'a Row>],
    pairs: &mut usize,
    budget: &mut WorkBudget,
    visitor: &mut impl for<'b> FnMut(&[Option<&'b Row>], &mut WorkBudget) -> Result<VisitControl>,
) -> Result<VisitControl> {
    if stage == plan.joins.len() {
        return visitor(bindings, budget);
    }
    let source = stage + 1;
    let mut matched = false;
    for build in &build_tables[stage] {
        count_join_pair(pairs)?;
        storage.charge_work(1)?;
        bindings[source] = Some(build);
        if conditions_match(&conditions[stage], bindings)? {
            matched = true;
            if visit_extensions(
                storage,
                stage + 1,
                plan,
                build_tables,
                conditions,
                bindings,
                pairs,
                budget,
                visitor,
            )? == VisitControl::Stop
            {
                bindings[source] = None;
                return Ok(VisitControl::Stop);
            }
        }
    }
    if plan.joins[stage].kind == JoinKind::Left && !matched {
        bindings[source] = None;
        if visit_extensions(
            storage,
            stage + 1,
            plan,
            build_tables,
            conditions,
            bindings,
            pairs,
            budget,
            visitor,
        )? == VisitControl::Stop
        {
            return Ok(VisitControl::Stop);
        }
    }
    bindings[source] = None;
    Ok(VisitControl::Continue)
}

fn matches_joined_predicate(
    bindings: &[Option<&Row>],
    plan: &JoinPlan,
    relations: &[Relation],
    budget: &WorkBudget,
) -> Result<bool> {
    let Some(predicate) = &plan.predicate else {
        return Ok(true);
    };
    budget.ensure_transient(flattened_row_bytes(bindings, relations)?)?;
    let flattened = flattened_row(bindings, relations);
    matches_predicate(&flattened, Some(predicate), "joined row")
}

#[derive(Clone)]
struct ResolvedCondition {
    left_source: usize,
    left_column: String,
    right_source: usize,
    right_column: String,
    data_type: ColumnType,
}

fn resolved_condition(
    condition: &JoinCondition,
    relations: &[Relation],
    new_source: usize,
) -> Result<ResolvedCondition> {
    let (left_source, _, left_definition) =
        resolve_column(&condition.left, &relations[..=new_source])?;
    let (right_source, _, right_definition) =
        resolve_column(&condition.right, &relations[..=new_source])?;
    if (left_source == new_source) == (right_source == new_source) {
        return Err(EngineError::invalid_query(
            "Every ON equality must connect the new table to an earlier table",
        ));
    }
    if !compatible_join_types(left_definition.data_type, right_definition.data_type) {
        return Err(EngineError::type_mismatch(format!(
            "JOIN cannot compare {} with {}",
            type_name(left_definition.data_type),
            type_name(right_definition.data_type)
        )));
    }
    if matches!(left_definition.data_type, ColumnType::Json)
        || matches!(right_definition.data_type, ColumnType::Json)
    {
        return Err(EngineError::type_mismatch(
            "JSON columns cannot be used as join keys",
        ));
    }
    Ok(ResolvedCondition {
        left_source,
        left_column: condition.left.column.clone(),
        right_source,
        right_column: condition.right.column.clone(),
        data_type: if matches!(
            (left_definition.data_type, right_definition.data_type),
            (ColumnType::Integer, ColumnType::Float) | (ColumnType::Float, ColumnType::Integer)
        ) {
            ColumnType::Float
        } else {
            left_definition.data_type
        },
    })
}

fn conditions_match(conditions: &[ResolvedCondition], bindings: &[Option<&Row>]) -> Result<bool> {
    for condition in conditions {
        if !condition_matches(condition, bindings)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn condition_matches(condition: &ResolvedCondition, bindings: &[Option<&Row>]) -> Result<bool> {
    let (Some(left_row), Some(right_row)) = (
        bindings[condition.left_source],
        bindings[condition.right_source],
    ) else {
        return Ok(false);
    };
    let left_value = left_row
        .get(&condition.left_column)
        .ok_or_else(|| missing_join_column_error(&condition.left_column))?;
    let right_value = right_row
        .get(&condition.right_column)
        .ok_or_else(|| missing_join_column_error(&condition.right_column))?;
    if left_value == &Value::Null || right_value == &Value::Null {
        return Ok(false);
    }
    Ok(values_equal(condition.data_type, left_value, right_value))
}

fn values_equal(data_type: ColumnType, left: &Value, right: &Value) -> bool {
    match data_type {
        ColumnType::Integer | ColumnType::Float => left
            .as_f64()
            .zip(right.as_f64())
            .is_some_and(|(left, right)| left == right),
        _ => left == right,
    }
}

fn validate_plan(
    plan: &JoinPlan,
    relations: &[Relation],
) -> Result<(Vec<Vec<ResolvedCondition>>, Vec<ResultField>)> {
    let mut aliases = HashSet::new();
    for relation in relations {
        validate_relation_shape(relation)?;
        if !aliases.insert(relation.source.alias.as_str()) {
            return Err(EngineError::invalid_query(format!(
                "Table alias `{}` is used more than once",
                relation.source.alias
            )));
        }
    }
    let conditions = plan
        .joins
        .iter()
        .enumerate()
        .map(|(stage, join)| {
            join.conditions
                .iter()
                .map(|condition| resolved_condition(condition, relations, stage + 1))
                .collect()
        })
        .collect::<Result<Vec<_>>>()?;

    let mut outputs = HashSet::new();
    let mut fields = Vec::with_capacity(plan.projections.len());
    for projection in &plan.projections {
        let (_, _, definition) = resolve_column(&projection.source, relations)?;
        if !outputs.insert(projection.output.as_str()) {
            return Err(EngineError::invalid_query(format!(
                "SELECT produces output column `{}` more than once; use distinct AS aliases",
                projection.output
            )));
        }
        fields.push(ResultField::new(&projection.output, definition.data_type));
    }
    if let Some(predicate) = &plan.predicate {
        validate_join_predicate(predicate, relations)?;
    }
    for order in &plan.order_by {
        match &order.source {
            OrderSource::Column(column) => {
                let (_, _, definition) = resolve_column(column, relations)?;
                if definition.data_type == ColumnType::Json {
                    return Err(EngineError::type_mismatch("JSON columns cannot be ordered"));
                }
            }
            OrderSource::Output(output) => {
                let projection = plan
                    .projections
                    .iter()
                    .find(|projection| projection.output == *output)
                    .ok_or_else(|| EngineError::column_not_found(output, "joined output"))?;
                let (_, _, definition) = resolve_column(&projection.source, relations)?;
                if definition.data_type == ColumnType::Json {
                    return Err(EngineError::type_mismatch(format!(
                        "JSON output column `{output}` cannot be ordered"
                    )));
                }
            }
        }
    }
    Ok((conditions, fields))
}

fn validate_relation_shape(relation: &Relation) -> Result<()> {
    if relation.source.alias.contains('.')
        || relation
            .schema
            .columns
            .iter()
            .any(|definition| definition.name.contains('.'))
    {
        return Err(EngineError::unsupported_sql(
            "JOIN does not support dots inside quoted aliases or column names",
        ));
    }
    Ok(())
}

fn validate_join_predicate(predicate: &Predicate, relations: &[Relation]) -> Result<()> {
    match predicate {
        Predicate::Comparison {
            column,
            operator,
            value,
        } => {
            let reference = parse_column_ref_text(column);
            let (_, _, definition) = resolve_column(&reference, relations)?;
            validate_literal(definition, *operator, value, column)
        }
        Predicate::IsNull { column, .. } => {
            resolve_column(&parse_column_ref_text(column), relations)?;
            Ok(())
        }
        Predicate::In { column, values } => {
            let reference = parse_column_ref_text(column);
            let (_, _, definition) = resolve_column(&reference, relations)?;
            for value in values {
                validate_literal(definition, ComparisonOperator::Eq, value, column)?;
            }
            Ok(())
        }
        Predicate::And { predicates } | Predicate::Or { predicates } => {
            for predicate in predicates {
                validate_join_predicate(predicate, relations)?;
            }
            Ok(())
        }
        Predicate::Not { predicate } => validate_join_predicate(predicate, relations),
    }
}

fn validate_literal(
    definition: &ColumnDefinition,
    operator: ComparisonOperator,
    value: &Value,
    column: &str,
) -> Result<()> {
    if value == &Value::Null {
        return Ok(());
    }
    let valid = match definition.data_type {
        ColumnType::Boolean => value.is_boolean(),
        ColumnType::Integer | ColumnType::Float => value.is_number(),
        ColumnType::Text => value.is_string(),
        ColumnType::Json => matches!(operator, ComparisonOperator::Eq | ComparisonOperator::Neq),
    };
    if valid {
        Ok(())
    } else {
        Err(EngineError::type_mismatch(format!(
            "Column `{column}` cannot be compared with that value"
        )))
    }
}

fn resolve_column<'a>(
    reference: &ColumnRef,
    relations: &'a [Relation],
) -> Result<(usize, usize, &'a ColumnDefinition)> {
    if let Some(qualifier) = &reference.qualifier {
        if let Some((source, relation)) = relations
            .iter()
            .enumerate()
            .find(|(_, relation)| qualifier == &relation.source.alias)
        {
            return resolve_in_relation(reference, relation)
                .map(|(index, definition)| (source, index, definition));
        }
        return Err(EngineError::invalid_query(format!(
            "Unknown table qualifier `{qualifier}`"
        )));
    }
    let mut found = None;
    for (source, relation) in relations.iter().enumerate() {
        if let Ok((index, definition)) = resolve_in_relation(reference, relation) {
            if found.is_some() {
                return Err(EngineError::invalid_query(format!(
                    "Column `{}` is ambiguous; qualify it with a table alias",
                    reference.column
                )));
            }
            found = Some((source, index, definition));
        }
    }
    found.ok_or_else(|| EngineError::column_not_found(&reference.column, "joined tables"))
}

fn resolve_in_relation<'a>(
    reference: &ColumnRef,
    relation: &'a Relation,
) -> Result<(usize, &'a ColumnDefinition)> {
    relation
        .schema
        .columns
        .iter()
        .enumerate()
        .find(|(_, definition)| definition.name == reference.column)
        .ok_or_else(|| EngineError::column_not_found(&reference.column, &relation.source.table))
}

fn compatible_join_types(left: ColumnType, right: ColumnType) -> bool {
    left == right
        || matches!(
            (left, right),
            (ColumnType::Integer, ColumnType::Float) | (ColumnType::Float, ColumnType::Integer)
        )
}

fn flattened_row(bindings: &[Option<&Row>], relations: &[Relation]) -> Row {
    let mut flattened = Map::new();
    for (source, relation) in relations.iter().enumerate() {
        for definition in &relation.schema.columns {
            let value = bindings[source]
                .and_then(|row| row.get(&definition.name))
                .cloned()
                .unwrap_or(Value::Null);
            flattened.insert(
                format!("{}.{}", relation.source.alias, definition.name),
                value.clone(),
            );
            if column_is_unique(&definition.name, relations) {
                flattened.insert(definition.name.clone(), value);
            }
        }
    }
    flattened
}

fn column_is_unique(column: &str, relations: &[Relation]) -> bool {
    relations
        .iter()
        .filter(|relation| {
            relation
                .schema
                .columns
                .iter()
                .any(|definition| definition.name == column)
        })
        .count()
        == 1
}

fn project_joined_row(
    bindings: &[Option<&Row>],
    plan: &JoinPlan,
    relations: &[Relation],
) -> Result<Row> {
    let mut projected = Map::new();
    for projection in &plan.projections {
        projected.insert(
            projection.output.clone(),
            joined_value(bindings, &projection.source, relations)?.clone(),
        );
    }
    Ok(projected)
}

fn joined_value<'a>(
    bindings: &[Option<&'a Row>],
    reference: &ColumnRef,
    relations: &[Relation],
) -> Result<&'a Value> {
    static NULL_VALUE: Value = Value::Null;
    let (source, _, _) = resolve_column(reference, relations)?;
    Ok(bindings[source]
        .and_then(|row| row.get(&reference.column))
        .unwrap_or(&NULL_VALUE))
}

fn order_keys(
    bindings: &[Option<&Row>],
    plan: &JoinPlan,
    relations: &[Relation],
) -> Result<Vec<Value>> {
    plan.order_by
        .iter()
        .map(|order| match &order.source {
            OrderSource::Column(reference) => joined_value(bindings, reference, relations).cloned(),
            OrderSource::Output(output) => {
                let projection = plan
                    .projections
                    .iter()
                    .find(|projection| projection.output == *output)
                    .expect("output alias was validated");
                joined_value(bindings, &projection.source, relations).cloned()
            }
        })
        .collect()
}

fn order_keys_bytes(
    bindings: &[Option<&Row>],
    plan: &JoinPlan,
    relations: &[Relation],
) -> Result<usize> {
    let mut bytes = 0;
    for order in &plan.order_by {
        let value = match &order.source {
            OrderSource::Column(reference) => joined_value(bindings, reference, relations)?,
            OrderSource::Output(output) => {
                let projection = plan
                    .projections
                    .iter()
                    .find(|projection| projection.output == *output)
                    .expect("output alias was validated");
                joined_value(bindings, &projection.source, relations)?
            }
        };
        bytes = checked_add(bytes, 32)?;
        bytes = checked_add(bytes, owned_value_bytes(value)?)?;
    }
    Ok(bytes)
}

fn compare_joined_rows(
    first: &OrderedJoinedRow,
    second: &OrderedJoinedRow,
    plan: &JoinPlan,
) -> Ordering {
    for ((first_value, second_value), order) in
        first.keys.iter().zip(&second.keys).zip(&plan.order_by)
    {
        let ordering = compare_values(first_value, second_value, order);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    first.ordinal.cmp(&second.ordinal)
}

fn compare_values(left: &Value, right: &Value, order: &JoinOrder) -> Ordering {
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

fn projected_row_bytes(
    bindings: &[Option<&Row>],
    plan: &JoinPlan,
    relations: &[Relation],
) -> Result<usize> {
    let mut bytes = 32_usize;
    for projection in &plan.projections {
        bytes = checked_add(bytes, 64)?;
        bytes = checked_add(bytes, checked_mul(projection.output.len(), 2)?)?;
        bytes = checked_add(
            bytes,
            checked_mul(
                owned_value_bytes(joined_value(bindings, &projection.source, relations)?)?,
                2,
            )?,
        )?;
    }
    Ok(bytes)
}

fn ordered_row_bytes(projected_bytes: usize, key_bytes: usize) -> Result<usize> {
    checked_add(checked_add(64, projected_bytes)?, key_bytes)
}

fn flattened_row_bytes(bindings: &[Option<&Row>], relations: &[Relation]) -> Result<usize> {
    let mut bytes = 32_usize;
    for (source, relation) in relations.iter().enumerate() {
        for definition in &relation.schema.columns {
            bytes = checked_add(bytes, 128)?;
            bytes = checked_add(
                bytes,
                checked_mul(relation.source.alias.len() + 1 + definition.name.len(), 2)?,
            )?;
            bytes = checked_add(bytes, checked_mul(definition.name.len(), 2)?)?;
            let value = bindings[source]
                .and_then(|row| row.get(&definition.name))
                .unwrap_or(&Value::Null);
            bytes = checked_add(bytes, checked_mul(owned_value_bytes(value)?, 4)?)?;
        }
    }
    Ok(bytes)
}

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

fn count_scanned_row(scanned: &mut usize) -> Result<()> {
    *scanned = scanned.saturating_add(1);
    if *scanned > MAX_SCAN_ROWS {
        Err(scan_limit_error())
    } else {
        Ok(())
    }
}

fn count_join_pair(pairs: &mut usize) -> Result<()> {
    *pairs = pairs.saturating_add(1);
    if *pairs > MAX_JOIN_PAIRS {
        Err(join_pairs_limit_error())
    } else {
        Ok(())
    }
}

fn join_pairs_limit_error() -> EngineError {
    EngineError::invalid_query(format!(
        "A join cannot examine more than {MAX_JOIN_PAIRS} candidate pairs"
    ))
}

fn build_rows_limit_error() -> EngineError {
    EngineError::new(
        "QUERY_WORK_LIMIT_EXCEEDED",
        format!("A join cannot retain more than {MAX_JOIN_BUILD_ROWS} build rows"),
    )
}

fn ensure_work_budget(bytes: usize) -> Result<()> {
    if bytes > MAX_JOIN_WORK_BYTES {
        Err(work_limit_error())
    } else {
        Ok(())
    }
}

fn ensure_result_budget(bytes: usize) -> Result<()> {
    if bytes > MAX_JOIN_RESULT_BYTES {
        Err(EngineError::new(
            "QUERY_WORK_LIMIT_EXCEEDED",
            format!("A join cannot materialize more than {MAX_JOIN_RESULT_BYTES} bytes of results"),
        ))
    } else {
        Ok(())
    }
}

fn checked_add(left: usize, right: usize) -> Result<usize> {
    left.checked_add(right).ok_or_else(work_limit_error)
}

fn checked_mul(left: usize, right: usize) -> Result<usize> {
    left.checked_mul(right).ok_or_else(work_limit_error)
}

fn work_limit_error() -> EngineError {
    EngineError::new(
        "QUERY_WORK_LIMIT_EXCEEDED",
        format!("A join cannot retain more than {MAX_JOIN_WORK_BYTES} bytes of working state"),
    )
}

fn scan_limit_error() -> EngineError {
    EngineError::new(
        "QUERY_WORK_LIMIT_EXCEEDED",
        format!("A join cannot scan more than {MAX_SCAN_ROWS} rows"),
    )
}

fn result_rows_limit_error() -> EngineError {
    EngineError::invalid_query(format!(
        "A join cannot materialize more than {MAX_RESULT_ROWS} rows"
    ))
}

fn malformed_reader_error(message: impl Into<String>) -> EngineError {
    EngineError::new("STORAGE_CORRUPT", message)
}

fn missing_join_column_error(column: &str) -> EngineError {
    malformed_reader_error(format!(
        "A joined storage row is missing resolved ON column `{column}`"
    ))
}

fn parse_column_ref_text(value: &str) -> ColumnRef {
    value.split_once('.').map_or_else(
        || ColumnRef {
            qualifier: None,
            column: value.to_owned(),
        },
        |(qualifier, column)| ColumnRef {
            qualifier: Some(qualifier.to_owned()),
            column: column.to_owned(),
        },
    )
}

fn type_name(data_type: ColumnType) -> &'static str {
    match data_type {
        ColumnType::Boolean => "boolean",
        ColumnType::Integer => "integer",
        ColumnType::Float => "float",
        ColumnType::Text => "text",
        ColumnType::Json => "json",
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

    fn parse(mut self) -> Result<JoinPlan> {
        self.expect_keyword("select")?;
        let projections = self.parse_projections()?;
        self.expect_keyword("from")?;
        let first = self.parse_source()?;
        let mut joins = Vec::new();
        let mut on_terms = 0_usize;
        loop {
            let kind = if self.consume_keyword("left") {
                self.consume_keyword("outer");
                self.expect_keyword("join")?;
                Some(JoinKind::Left)
            } else if self.consume_keyword("inner") {
                self.expect_keyword("join")?;
                Some(JoinKind::Inner)
            } else if self.consume_keyword("join") {
                Some(JoinKind::Inner)
            } else {
                None
            };
            let Some(kind) = kind else {
                if joins.is_empty() {
                    return Err(EngineError::parse_error("Expected JOIN"));
                }
                break;
            };
            if joins.len() + 1 >= MAX_JOIN_SOURCES {
                return Err(EngineError::unsupported_sql(format!(
                    "A JOIN cannot contain more than {MAX_JOIN_SOURCES} sources"
                )));
            }
            let source = self.parse_source()?;
            self.expect_keyword("on")?;
            let conditions = self.parse_conditions()?;
            on_terms = on_terms.saturating_add(conditions.len());
            if on_terms > MAX_ON_TERMS {
                return Err(EngineError::invalid_query(format!(
                    "A JOIN cannot contain more than {MAX_ON_TERMS} ON equalities"
                )));
            }
            joins.push(JoinStage {
                source,
                kind,
                conditions,
            });
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
        let order_by = if self.consume_keyword("order") {
            self.expect_keyword("by")?;
            self.parse_order_by(&projections)?
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
                "This join subset supports equijoin ON clauses, WHERE, ORDER BY, LIMIT, and OFFSET",
            ));
        }
        Ok(JoinPlan {
            projections,
            first,
            joins,
            predicate,
            order_by,
            limit,
            offset,
        })
    }

    fn parse_projections(&mut self) -> Result<Vec<Projection>> {
        if self.consume_star() {
            return Err(EngineError::unsupported_sql(
                "JOIN requires an explicit projection because JSON rows cannot represent duplicate column names",
            ));
        }
        let mut projections = Vec::new();
        loop {
            if projections.len() >= MAX_PROJECTIONS {
                return Err(EngineError::invalid_query(format!(
                    "A JOIN projection cannot contain more than {MAX_PROJECTIONS} columns"
                )));
            }
            let source = self.parse_column_ref()?;
            let output = if self.consume_keyword("as") {
                self.parse_identifier()?
            } else {
                source.column.clone()
            };
            projections.push(Projection { source, output });
            if !self.consume_comma() {
                break;
            }
        }
        Ok(projections)
    }

    fn parse_source(&mut self) -> Result<Source> {
        let table = self.parse_table_name()?;
        let default_alias = table
            .rsplit_once('.')
            .map_or_else(|| table.clone(), |(_, table)| table.to_owned());
        let has_as = self.consume_keyword("as");
        let alias = if has_as || self.peek_alias_identifier() {
            self.parse_identifier()?
        } else {
            default_alias
        };
        Ok(Source { table, alias })
    }

    fn parse_conditions(&mut self) -> Result<Vec<JoinCondition>> {
        let mut conditions = Vec::new();
        loop {
            if conditions.len() >= MAX_ON_TERMS {
                return Err(EngineError::invalid_query(format!(
                    "A JOIN cannot contain more than {MAX_ON_TERMS} ON equalities"
                )));
            }
            let left = self.parse_column_ref()?;
            if !self.consume_eq() {
                return Err(EngineError::unsupported_sql(
                    "JOIN ON supports only equality between columns",
                ));
            }
            let right = self.parse_column_ref()?;
            conditions.push(JoinCondition { left, right });
            if !self.consume_keyword("and") {
                break;
            }
        }
        Ok(conditions)
    }

    fn parse_order_by(&mut self, projections: &[Projection]) -> Result<Vec<JoinOrder>> {
        let mut orders = Vec::new();
        loop {
            if orders.len() >= MAX_ORDER_COLUMNS {
                return Err(EngineError::invalid_query(format!(
                    "A query cannot order by more than {MAX_ORDER_COLUMNS} columns"
                )));
            }
            let reference = self.parse_column_ref()?;
            let source = if reference.qualifier.is_none()
                && projections
                    .iter()
                    .any(|projection| projection.output == reference.column)
            {
                OrderSource::Output(reference.column)
            } else {
                OrderSource::Column(reference)
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
            orders.push(JoinOrder {
                source,
                direction,
                nulls,
            });
            if !self.consume_comma() {
                break;
            }
        }
        Ok(orders)
    }

    fn parse_column_ref(&mut self) -> Result<ColumnRef> {
        let first = self.parse_identifier()?;
        if !self.consume_dot() {
            return Ok(ColumnRef {
                qualifier: None,
                column: first,
            });
        }
        let column = self.parse_identifier()?;
        if self.consume_dot() {
            return Err(EngineError::unsupported_sql(
                "Column references can contain at most one table qualifier",
            ));
        }
        Ok(ColumnRef {
            qualifier: Some(first),
            column,
        })
    }

    fn parse_table_name(&mut self) -> Result<String> {
        let first = self.parse_identifier()?;
        if !self.consume_dot() {
            return Ok(first);
        }
        let second = self.parse_identifier()?;
        if self.consume_dot() {
            return Err(EngineError::unsupported_sql(
                "Table names can contain at most one schema qualifier",
            ));
        }
        Ok(format!("{first}.{second}"))
    }

    fn parse_limit(&mut self) -> Result<usize> {
        let Some(token) = self.next() else {
            return Err(EngineError::parse_error("Expected LIMIT or OFFSET value"));
        };
        let value = match token {
            Token::Number(value) => Number::from_str(&value).map(Value::Number).map_err(|_| {
                EngineError::invalid_query(format!("Invalid number literal `{value}`"))
            })?,
            Token::Placeholder(index) => bind_parameter(&index, self.params)?,
            _ => {
                return Err(EngineError::invalid_query(
                    "LIMIT and OFFSET must be non-negative integers",
                ));
            }
        };
        pagination_value(&value, self.mode)
    }

    fn parse_identifier(&mut self) -> Result<String> {
        let Some(Token::Identifier { value, quoted }) = self.next() else {
            return Err(EngineError::parse_error("Expected a SQL identifier"));
        };
        if value.is_empty() || (!quoted && is_reserved_keyword(&value)) {
            return Err(EngineError::parse_error("Expected a valid SQL identifier"));
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

    fn peek_alias_identifier(&self) -> bool {
        match self.tokens.get(self.position) {
            Some(Token::Identifier { quoted: true, .. }) => true,
            Some(Token::Identifier {
                value,
                quoted: false,
            }) => !is_reserved_keyword(value),
            _ => false,
        }
    }

    fn peek_keyword(&self, keyword: &str) -> bool {
        matches!(
            self.tokens.get(self.position),
            Some(Token::Identifier { value, quoted: false }) if value.eq_ignore_ascii_case(keyword)
        )
    }

    fn consume_keyword(&mut self, keyword: &str) -> bool {
        if self.peek_keyword(keyword) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn consume_eq(&mut self) -> bool {
        self.consume_if(|token| matches!(token, Token::Eq))
    }

    fn consume_star(&mut self) -> bool {
        self.consume_if(|token| matches!(token, Token::Star))
    }

    fn consume_comma(&mut self) -> bool {
        self.consume_if(|token| matches!(token, Token::Comma))
    }

    fn consume_dot(&mut self) -> bool {
        self.consume_if(|token| matches!(token, Token::Dot))
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

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{
        Engine, InMemoryStorage, IndexDefinition, Result, Row, RowChange, StorageDriver,
        StorageReader, TableDefinition, VisitControl, VisitOutcome,
    };

    #[derive(Clone, Copy)]
    enum ReaderFault {
        None,
        StopRight,
        OmitLeftJoinKey,
    }

    struct AdversarialStorage {
        inner: InMemoryStorage,
        fault: ReaderFault,
    }

    impl StorageReader for AdversarialStorage {
        fn visit_table(
            &self,
            table: &str,
            visitor: &mut dyn FnMut(&Row) -> Result<VisitControl>,
        ) -> Result<VisitOutcome> {
            if matches!(self.fault, ReaderFault::StopRight) && table == "right_items" {
                return Ok(VisitOutcome::Stopped);
            }
            if matches!(self.fault, ReaderFault::OmitLeftJoinKey) && table == "left_items" {
                return self.inner.visit_table(table, &mut |row| {
                    let mut malformed = row.clone();
                    malformed.remove("k1");
                    visitor(&malformed)
                });
            }
            self.inner.visit_table(table, visitor)
        }

        fn table_row_count(&self, table: &str) -> Result<usize> {
            self.inner.table_schema(table)?;
            // Deliberately stale low hint: execution must use it only for
            // preflight, never as proof that a relation is empty.
            Ok(0)
        }

        fn scan_table(&self, _table: &str) -> Result<Vec<Row>> {
            panic!("join execution must use visit_table")
        }

        fn lookup_primary_key(&self, table: &str, key: &Row) -> Result<Option<Row>> {
            self.inner.lookup_primary_key(table, key)
        }

        fn index_definition(&self, name: &str) -> Option<IndexDefinition> {
            self.inner.index_definition(name)
        }

        fn indexes_for_table(&self, table: &str) -> Result<Vec<IndexDefinition>> {
            self.inner.indexes_for_table(table)
        }

        fn visit_index(
            &self,
            table: &str,
            columns: &[String],
            key: &Row,
            visitor: &mut dyn FnMut(&Row) -> Result<VisitControl>,
        ) -> Result<Option<VisitOutcome>> {
            self.inner.visit_index(table, columns, key, visitor)
        }

        fn table_schema(&self, table: &str) -> Result<TableDefinition> {
            self.inner.table_schema(table)
        }

        fn revision(&self) -> u64 {
            self.inner.revision()
        }
    }

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

    fn large_join_database(tables: &[(&str, usize)]) -> Engine {
        let mut engine = Engine::default();
        for (table, count) in tables {
            engine
                .execute_sql(
                    &format!(
                        "CREATE TABLE {table} (id INTEGER PRIMARY KEY, join_key INTEGER NOT NULL)"
                    ),
                    &[],
                )
                .unwrap();
            seed_rows(
                &mut engine,
                table,
                (0..*count)
                    .map(|id| row(json!({"id": id, "join_key": 1})))
                    .collect(),
            );
        }
        engine
    }

    fn database() -> Engine {
        let mut engine = Engine::default();
        engine
            .execute_sql(
                "CREATE TABLE left_items (\
                    id INTEGER PRIMARY KEY, k1 INTEGER, k2 TEXT, label TEXT NOT NULL\
                )",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "CREATE TABLE right_items (\
                    rid INTEGER PRIMARY KEY, k1 INTEGER, k2 TEXT, label TEXT NOT NULL, score INTEGER\
                )",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO left_items (id, k1, k2, label) VALUES \
                 (1, 10, 'a', 'L1'), (2, 10, 'b', 'L2'), (3, 20, 'a', 'L3'), \
                 (4, NULL, 'a', 'L4'), (5, 30, NULL, 'L5'), (6, 99, 'z', 'L6')",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO right_items (rid, k1, k2, label, score) VALUES \
                 (101, 10, 'a', 'R1', 3), (102, 10, 'a', 'R2', 2), \
                 (103, 10, 'b', 'R3', 1), (104, 20, 'a', 'R4', 4), \
                 (105, NULL, 'a', 'RN', 5), (106, 30, NULL, 'Rnull2', 6), \
                 (107, 40, 'x', 'R5', 7)",
                &[],
            )
            .unwrap();
        engine
    }

    fn multi_database() -> Engine {
        let mut engine = Engine::default();
        engine
            .execute_sql(
                "CREATE TABLE chain_a (id INTEGER PRIMARY KEY, join_key INTEGER NOT NULL)",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "CREATE TABLE chain_b (id INTEGER PRIMARY KEY, a_key INTEGER NOT NULL)",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "CREATE TABLE chain_c (\
                    id INTEGER PRIMARY KEY, b_id INTEGER, a_id INTEGER NOT NULL\
                )",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO chain_a (id, join_key) VALUES (1, 10), (2, 10), (3, 30)",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO chain_b (id, a_key) VALUES (11, 10), (12, 10), (13, 99)",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO chain_c (id, b_id, a_id) VALUES \
                 (101, 11, 1), (102, 11, 2), (103, 12, 1), (104, NULL, 3)",
                &[],
            )
            .unwrap();
        engine
    }

    #[test]
    fn inner_join_multiplies_duplicate_keys_and_null_never_matches() {
        let result = database()
            .query_sql(
                "SELECT l.id AS left_id, r.rid AS right_id \
                 FROM left_items AS l INNER JOIN right_items AS r ON l.k1 = r.k1 \
                 ORDER BY left_id, right_id",
                &[],
            )
            .unwrap();
        assert_eq!(
            result.rows,
            vec![
                row(json!({"left_id": 1, "right_id": 101})),
                row(json!({"left_id": 1, "right_id": 102})),
                row(json!({"left_id": 1, "right_id": 103})),
                row(json!({"left_id": 2, "right_id": 101})),
                row(json!({"left_id": 2, "right_id": 102})),
                row(json!({"left_id": 2, "right_id": 103})),
                row(json!({"left_id": 3, "right_id": 104})),
                row(json!({"left_id": 5, "right_id": 106})),
            ]
        );
    }

    #[test]
    fn three_table_inner_join_multiplies_each_left_deep_stage() {
        let result = multi_database()
            .query_sql(
                "SELECT a.id AS a_id, b.id AS b_id, c.id AS c_id \
                 FROM chain_a a JOIN chain_b b ON a.join_key = b.a_key \
                 JOIN chain_c c ON b.id = c.b_id \
                 ORDER BY a_id, b_id, c_id",
                &[],
            )
            .unwrap();
        assert_eq!(
            result.rows,
            vec![
                row(json!({"a_id": 1, "b_id": 11, "c_id": 101})),
                row(json!({"a_id": 1, "b_id": 11, "c_id": 102})),
                row(json!({"a_id": 1, "b_id": 12, "c_id": 103})),
                row(json!({"a_id": 2, "b_id": 11, "c_id": 101})),
                row(json!({"a_id": 2, "b_id": 11, "c_id": 102})),
                row(json!({"a_id": 2, "b_id": 12, "c_id": 103})),
            ]
        );
    }

    #[test]
    fn chained_left_joins_extend_only_the_new_source() {
        let engine = multi_database();
        let result = engine
            .query_sql(
                "SELECT a.id AS a_id, b.id AS b_id, c.id AS c_id \
                 FROM chain_a a LEFT JOIN chain_b b ON a.join_key = b.a_key \
                 LEFT JOIN chain_c c ON b.id = c.b_id \
                 ORDER BY a_id, b_id, c_id",
                &[],
            )
            .unwrap();
        assert_eq!(
            result.rows,
            vec![
                row(json!({"a_id": 1, "b_id": 11, "c_id": 101})),
                row(json!({"a_id": 1, "b_id": 11, "c_id": 102})),
                row(json!({"a_id": 1, "b_id": 12, "c_id": 103})),
                row(json!({"a_id": 2, "b_id": 11, "c_id": 101})),
                row(json!({"a_id": 2, "b_id": 11, "c_id": 102})),
                row(json!({"a_id": 2, "b_id": 12, "c_id": 103})),
                row(json!({"a_id": 3, "b_id": null, "c_id": null})),
            ]
        );

        let later_inner = engine
            .query_sql(
                "SELECT a.id AS a_id, b.id AS b_id, c.id AS c_id \
                 FROM chain_a a LEFT JOIN chain_b b ON a.join_key = b.a_key \
                 JOIN chain_c c ON a.id = c.a_id \
                 ORDER BY a_id, b_id, c_id",
                &[],
            )
            .unwrap();
        assert_eq!(
            later_inner.rows,
            vec![
                row(json!({"a_id": 1, "b_id": 11, "c_id": 101})),
                row(json!({"a_id": 1, "b_id": 11, "c_id": 103})),
                row(json!({"a_id": 1, "b_id": 12, "c_id": 101})),
                row(json!({"a_id": 1, "b_id": 12, "c_id": 103})),
                row(json!({"a_id": 2, "b_id": 11, "c_id": 102})),
                row(json!({"a_id": 2, "b_id": 12, "c_id": 102})),
                row(json!({"a_id": 3, "b_id": null, "c_id": 104})),
            ]
        );
    }

    #[test]
    fn multi_join_resolution_rejects_ambiguity_future_and_nonconnecting_terms() {
        let engine = multi_database();
        for sql in [
            "SELECT id FROM chain_a a JOIN chain_b b ON a.join_key = b.a_key \
             JOIN chain_c c ON b.id = c.b_id",
            "SELECT a.id AS id FROM chain_a a JOIN chain_b b ON a.join_key = c.b_id \
             JOIN chain_c c ON b.id = c.b_id",
            "SELECT a.id AS id FROM chain_a a JOIN chain_b b ON a.join_key = b.a_key \
             JOIN chain_c c ON a.id = b.id",
            "SELECT a.id AS id FROM chain_a a JOIN chain_b b ON a.join_key = b.a_key \
             JOIN chain_c c ON c.id = c.b_id",
        ] {
            assert_eq!(
                engine.query_sql(sql, &[]).unwrap_err().code,
                "INVALID_QUERY",
                "query was `{sql}`"
            );
        }
    }

    #[test]
    fn accepts_eight_sources_and_rejects_nine() {
        fn chain(source_count: usize) -> String {
            let mut sql = String::from("SELECT a1.id AS id FROM chain_a a1");
            for source in 2..=source_count {
                sql.push_str(&format!(" JOIN chain_a a{source} ON a1.id = a{source}.id"));
            }
            sql
        }

        let engine = multi_database();
        assert_eq!(engine.query_sql(&chain(8), &[]).unwrap().rows.len(), 3);
        assert_eq!(
            engine.query_sql(&chain(9), &[]).unwrap_err().code,
            "UNSUPPORTED_SQL"
        );
    }

    #[test]
    fn streams_with_visitors_and_stops_the_probe_after_an_unordered_limit() {
        let storage = database().into_storage();
        let before = storage.visitor_counts();
        let plan = super::parse_sql(
            "SELECT l.id AS left_id, r.rid AS right_id \
             FROM left_items l LEFT JOIN right_items r ON l.k1 = r.k1 LIMIT 1",
            &[],
        )
        .unwrap();

        let result = super::execute(&storage, &plan).unwrap();

        assert_eq!(
            result.rows,
            vec![row(json!({"left_id": 1, "right_id": 101}))]
        );
        let after = storage.visitor_counts();
        // LEFT JOIN retains the seven right rows, then stops the streamed left
        // side after its first row produces the requested result.
        assert_eq!(after.0 - before.0, 8);
        assert_eq!(after.1, before.1);
    }

    #[test]
    fn inner_join_preserves_left_major_streaming_without_collectors() {
        let storage = database().into_storage();
        let before = storage.visitor_counts();
        let plan = super::parse_sql(
            "SELECT l.id AS left_id, r.rid AS right_id \
             FROM left_items l JOIN right_items r ON l.k1 = r.k1 LIMIT 1",
            &[],
        )
        .unwrap();

        let result = super::execute(&storage, &plan).unwrap();

        assert_eq!(
            result.rows,
            vec![row(json!({"left_id": 1, "right_id": 101}))]
        );
        let after = storage.visitor_counts();
        // The seven-row right side is retained and the left-side visitor stops
        // on its first row. In particular, scan_table was never called.
        assert_eq!(after.0 - before.0, 8);
        assert_eq!(after.1, before.1);
    }

    #[test]
    fn preserves_left_major_order_for_unordered_pagination_and_order_ties() {
        let engine = database();
        let expected = vec![
            row(json!({"left_id": 1, "right_id": 103})),
            row(json!({"left_id": 2, "right_id": 101})),
            row(json!({"left_id": 2, "right_id": 102})),
            row(json!({"left_id": 2, "right_id": 103})),
        ];
        assert_eq!(
            engine
                .query_sql(
                    "SELECT l.id AS left_id, r.rid AS right_id \
                     FROM left_items l JOIN right_items r ON l.k1 = r.k1 \
                     LIMIT 4 OFFSET 2",
                    &[],
                )
                .unwrap()
                .rows,
            expected
        );
        assert_eq!(
            engine
                .query_sql(
                    "SELECT l.id AS left_id, r.rid AS right_id \
                     FROM left_items l JOIN right_items r ON l.k1 = r.k1 \
                     ORDER BY l.k1 LIMIT 4 OFFSET 2",
                    &[],
                )
                .unwrap()
                .rows,
            expected
        );
    }

    #[test]
    fn stale_count_hints_do_not_skip_rows_and_malformed_visitors_fail_closed() {
        let plan = super::parse_sql(
            "SELECT l.id AS left_id, r.rid AS right_id \
             FROM left_items l JOIN right_items r ON l.k1 = r.k1",
            &[],
        )
        .unwrap();
        let storage = AdversarialStorage {
            inner: database().into_storage(),
            fault: ReaderFault::None,
        };
        assert_eq!(super::execute(&storage, &plan).unwrap().rows.len(), 8);

        for (fault, expected_message) in [
            (ReaderFault::StopRight, "stopped before completing"),
            (ReaderFault::OmitLeftJoinKey, "missing resolved ON column"),
        ] {
            let storage = AdversarialStorage {
                inner: database().into_storage(),
                fault,
            };
            let error = super::execute(&storage, &plan).unwrap_err();
            assert_eq!(error.code, "STORAGE_CORRUPT");
            assert!(error.message.contains(expected_message), "{error:?}");
        }
    }

    #[test]
    fn composite_left_join_null_extends_unmatched_rows() {
        let result = database()
            .query_sql(
                "SELECT l.id AS left_id, r.rid AS right_id \
                 FROM left_items l LEFT OUTER JOIN right_items r \
                 ON r.k1 = l.k1 AND l.k2 = r.k2 \
                 ORDER BY left_id, right_id NULLS LAST",
                &[],
            )
            .unwrap();
        assert_eq!(
            result.rows,
            vec![
                row(json!({"left_id": 1, "right_id": 101})),
                row(json!({"left_id": 1, "right_id": 102})),
                row(json!({"left_id": 2, "right_id": 103})),
                row(json!({"left_id": 3, "right_id": 104})),
                row(json!({"left_id": 4, "right_id": null})),
                row(json!({"left_id": 5, "right_id": null})),
                row(json!({"left_id": 6, "right_id": null})),
            ]
        );
    }

    #[test]
    fn where_after_left_join_filters_null_extended_rows() {
        let engine = database();
        let result = engine
            .query_sql(
                "SELECT l.id AS left_id, r.rid AS right_id \
                 FROM left_items l LEFT JOIN right_items r \
                 ON l.k1 = r.k1 AND l.k2 = r.k2 \
                 WHERE r.score >= $1 ORDER BY left_id, right_id",
                &[json!(3)],
            )
            .unwrap();
        assert_eq!(
            result.rows,
            vec![
                row(json!({"left_id": 1, "right_id": 101})),
                row(json!({"left_id": 3, "right_id": 104})),
            ]
        );

        let unmatched = engine
            .query_sql(
                "SELECT l.id AS left_id FROM left_items l LEFT JOIN right_items r \
                 ON l.k1 = r.k1 AND l.k2 = r.k2 \
                 WHERE r.rid IS NULL ORDER BY left_id",
                &[],
            )
            .unwrap();
        assert_eq!(
            unmatched.rows,
            vec![
                row(json!({"left_id": 4})),
                row(json!({"left_id": 5})),
                row(json!({"left_id": 6})),
            ]
        );
    }

    #[test]
    fn qualified_where_order_and_pagination_work_with_aliases() {
        let result = database()
            .execute_sql(
                "SELECT l.label AS left_label, r.label AS right_label, r.score AS score \
                 FROM left_items AS l JOIN right_items AS r \
                 ON l.k1 = r.k1 AND l.k2 = r.k2 \
                 WHERE l.id = 1 AND r.score >= 2 \
                 ORDER BY score DESC LIMIT 1 OFFSET 1",
                &[],
            )
            .unwrap();
        assert_eq!(result.command, "SELECT");
        assert_eq!(
            result.rows,
            vec![row(json!({
                "left_label": "L1",
                "right_label": "R2",
                "score": 2
            }))]
        );

        let quoted_aliases = database()
            .query_sql(
                "SELECT \"left\".id AS left_id, \"join\".rid AS right_id \
                 FROM left_items \"left\" JOIN right_items \"join\" \
                 ON \"left\".k1 = \"join\".k1 ORDER BY left_id, right_id",
                &[],
            )
            .unwrap();
        assert_eq!(quoted_aliases.rows.len(), 8);

        let alias_precedence = database()
            .query_sql(
                "SELECT r.score AS id, l.id AS left_id \
                 FROM left_items l JOIN right_items r ON l.k1 = r.k1 \
                 ORDER BY id DESC, left_id LIMIT 2",
                &[],
            )
            .unwrap();
        assert_eq!(
            alias_precedence.rows,
            vec![
                row(json!({"id": 6, "left_id": 5})),
                row(json!({"id": 4, "left_id": 3})),
            ]
        );
    }

    #[test]
    fn staged_transaction_rows_are_visible_to_joins() {
        let mut engine = database();
        engine.begin_transaction().unwrap();
        engine
            .execute_sql(
                "INSERT INTO right_items (rid, k1, k2, label, score) VALUES \
                 (108, 99, 'z', 'staged', 8)",
                &[],
            )
            .unwrap();
        assert_eq!(
            engine
                .query_sql(
                    "SELECT l.id AS id, r.rid AS rid FROM left_items l \
                     JOIN right_items r ON l.k1 = r.k1 AND l.k2 = r.k2 \
                     WHERE r.rid = 108",
                    &[],
                )
                .unwrap()
                .rows,
            vec![row(json!({"id": 6, "rid": 108}))]
        );
        engine.rollback_transaction().unwrap();
        assert!(
            engine
                .query_sql(
                    "SELECT l.id AS id, r.rid AS rid FROM left_items l \
                     JOIN right_items r ON l.k1 = r.k1 AND l.k2 = r.k2 \
                     WHERE r.rid = 108",
                    &[],
                )
                .unwrap()
                .rows
                .is_empty()
        );
    }

    #[test]
    fn staged_rows_are_visible_across_multiple_join_builds() {
        let mut engine = multi_database();
        engine.begin_transaction().unwrap();
        engine
            .execute_sql("INSERT INTO chain_b (id, a_key) VALUES (14, 30)", &[])
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO chain_c (id, b_id, a_id) VALUES (105, 14, 3)",
                &[],
            )
            .unwrap();
        let sql = "SELECT a.id AS a_id, b.id AS b_id, c.id AS c_id \
                   FROM chain_a a JOIN chain_b b ON a.join_key = b.a_key \
                   JOIN chain_c c ON b.id = c.b_id WHERE c.id = 105";
        assert_eq!(
            engine.query_sql(sql, &[]).unwrap().rows,
            vec![row(json!({"a_id": 3, "b_id": 14, "c_id": 105}))]
        );
        engine.rollback_transaction().unwrap();
        assert!(engine.query_sql(sql, &[]).unwrap().rows.is_empty());
    }

    #[test]
    fn integer_and_float_join_keys_compare_numerically() {
        let mut engine = Engine::default();
        engine
            .execute_sql(
                "CREATE TABLE integers (id INTEGER PRIMARY KEY, join_key INTEGER NOT NULL)",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "CREATE TABLE floats (id INTEGER PRIMARY KEY, join_key DOUBLE PRECISION NOT NULL)",
                &[],
            )
            .unwrap();
        engine
            .execute_sql("INSERT INTO integers (id, join_key) VALUES (1, 10)", &[])
            .unwrap();
        engine
            .execute_sql("INSERT INTO floats (id, join_key) VALUES (2, 10.0)", &[])
            .unwrap();
        assert_eq!(
            engine
                .query_sql(
                    "SELECT i.id AS integer_id, f.id AS float_id \
                     FROM integers i JOIN floats f ON i.join_key = f.join_key",
                    &[],
                )
                .unwrap()
                .rows,
            vec![row(json!({"integer_id": 1, "float_id": 2}))]
        );
    }

    #[test]
    fn rejects_ambiguous_unsafe_and_unbounded_join_shapes() {
        let engine = database();
        for (sql, code) in [
            (
                "SELECT label FROM left_items l JOIN right_items r ON l.k1 = r.k1",
                "INVALID_QUERY",
            ),
            (
                "SELECT l.id AS id FROM left_items l JOIN right_items r ON l.k1 = r.k1 \
                 WHERE label = 'L1'",
                "INVALID_QUERY",
            ),
            (
                "SELECT l.id AS id FROM left_items l JOIN right_items r ON k1 = r.k1",
                "INVALID_QUERY",
            ),
            (
                "SELECT l.label, r.label FROM left_items l JOIN right_items r ON l.k1 = r.k1",
                "INVALID_QUERY",
            ),
            (
                "SELECT * FROM left_items l JOIN right_items r ON l.k1 = r.k1",
                "UNSUPPORTED_SQL",
            ),
            (
                "SELECT l.id AS id FROM left_items l JOIN right_items r ON l.k1 > r.k1",
                "UNSUPPORTED_SQL",
            ),
            (
                "SELECT l.id AS id FROM left_items l JOIN right_items r ON l.k1 = 10",
                "SQL_PARSE_ERROR",
            ),
            (
                "SELECT x.id AS id FROM left_items l JOIN right_items r ON l.k1 = r.k1",
                "INVALID_QUERY",
            ),
            (
                "SELECT left_items.id AS id FROM left_items l JOIN right_items r \
                 ON l.k1 = r.k1",
                "INVALID_QUERY",
            ),
            (
                "SELECT l.id AS id FROM left_items l JOIN right_items l ON l.k1 = l.k1",
                "INVALID_QUERY",
            ),
            (
                "SELECT l.id AS id FROM left_items l JOIN right_items r ON l.id = l.k1",
                "INVALID_QUERY",
            ),
            (
                "SELECT l.id AS id FROM left_items l JOIN right_items r ON l.k1 = r.label",
                "TYPE_MISMATCH",
            ),
            (
                "SELECT l.id AS id FROM left_items l JOIN right_items r ON l.k1 = r.k1 \
                 WHERE r.score = 'not a number' LIMIT 0",
                "TYPE_MISMATCH",
            ),
        ] {
            assert_eq!(
                engine.query_sql(sql, &[]).unwrap_err().code,
                code,
                "query was `{sql}`"
            );
        }

        assert_eq!(
            engine
                .query_sql(
                    "SELECT \"l.dot\".id AS id FROM left_items AS \"l.dot\" \
                     JOIN right_items r ON \"l.dot\".k1 = r.k1",
                    &[],
                )
                .unwrap_err()
                .code,
            "UNSUPPORTED_SQL"
        );

        let mut json_engine = Engine::default();
        json_engine
            .execute_sql(
                "CREATE TABLE json_left (id INTEGER PRIMARY KEY, payload JSON NOT NULL)",
                &[],
            )
            .unwrap();
        json_engine
            .execute_sql(
                "CREATE TABLE json_right (id INTEGER PRIMARY KEY, left_id INTEGER NOT NULL)",
                &[],
            )
            .unwrap();
        for order in ["l.payload", "payload_alias"] {
            let sql = format!(
                "SELECT l.payload AS payload_alias FROM json_left l \
                 JOIN json_right r ON l.id = r.left_id ORDER BY {order}"
            );
            assert_eq!(
                json_engine.query_sql(&sql, &[]).unwrap_err().code,
                "TYPE_MISMATCH"
            );
        }
    }

    #[test]
    fn bounds_join_parse_complexity() {
        let projections = (0..=super::MAX_PROJECTIONS)
            .map(|index| format!("l.id AS output_{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(
            super::parse_sql(
                &format!(
                    "SELECT {projections} FROM left_items l JOIN right_items r ON l.k1 = r.k1"
                ),
                &[],
            )
            .unwrap_err()
            .code,
            "INVALID_QUERY"
        );

        let conditions = (0..=super::MAX_ON_TERMS)
            .map(|_| "l.k1 = r.k1")
            .collect::<Vec<_>>()
            .join(" AND ");
        assert_eq!(
            super::parse_sql(
                &format!("SELECT l.id AS id FROM left_items l JOIN right_items r ON {conditions}"),
                &[],
            )
            .unwrap_err()
            .code,
            "INVALID_QUERY"
        );

        let sixteen_left = (0..16)
            .map(|_| "a.id = b.id")
            .collect::<Vec<_>>()
            .join(" AND ");
        let sixteen_right = (0..16)
            .map(|_| "b.id = c.id")
            .collect::<Vec<_>>()
            .join(" AND ");
        super::parse_sql(
            &format!("SELECT a.id AS id FROM a JOIN b ON {sixteen_left} JOIN c ON {sixteen_right}"),
            &[],
        )
        .unwrap();
        assert_eq!(
            super::parse_sql(
                &format!(
                    "SELECT a.id AS id FROM a JOIN b ON {sixteen_left} \
                     JOIN c ON {sixteen_right} AND b.id = c.id"
                ),
                &[],
            )
            .unwrap_err()
            .code,
            "INVALID_QUERY"
        );
    }

    #[test]
    fn selective_three_table_joins_count_only_actual_candidate_extensions() {
        let engine = large_join_database(&[("a", 100), ("b", 100), ("c", 100)]);
        // The Cartesian estimate is 10,000 + 1,000,000, but each first-stage
        // match extends only once: these joins examine 20,000 actual pairs.
        for kind in ["JOIN", "LEFT JOIN"] {
            for order in ["", " ORDER BY a.id DESC"] {
                let extra_condition = if kind == "LEFT JOIN" {
                    " AND b.id = c.join_key"
                } else {
                    ""
                };
                let sql = format!(
                    "SELECT a.id AS a_id, b.id AS b_id, c.id AS c_id \
                     FROM a {kind} b ON a.id = b.id \
                     {kind} c ON b.id = c.id{extra_condition}{order}"
                );
                let mut expected = (0..100)
                    .map(|id| {
                        let c_id = if kind == "LEFT JOIN" && id != 1 {
                            Value::Null
                        } else {
                            json!(id)
                        };
                        row(json!({"a_id": id, "b_id": id, "c_id": c_id}))
                    })
                    .collect::<Vec<_>>();
                if !order.is_empty() {
                    expected.reverse();
                }
                let mut actual = engine.query_sql(&sql, &[]).unwrap().rows;
                if order.is_empty() {
                    actual.sort_by_key(|row| row["a_id"].as_i64().unwrap());
                }
                assert_eq!(actual, expected, "{sql}");
            }
        }
    }

    #[test]
    fn actual_candidate_pair_limit_allows_early_limits_and_rejects_exhaustion() {
        let storage =
            large_join_database(&[("many_left", 1_001), ("many_right", 1_001)]).into_storage();
        let query = "SELECT l.id AS left_id FROM many_left l JOIN many_right r \
                     ON l.join_key = r.join_key";
        let before = storage.visitor_counts();
        let zero = super::execute(
            &storage,
            &super::parse_sql(&format!("{query} LIMIT 0"), &[]).unwrap(),
        )
        .unwrap();
        assert!(zero.rows.is_empty());
        assert_eq!(
            zero.fields,
            vec![crate::ResultField::new(
                "left_id",
                crate::ColumnType::Integer
            )]
        );
        assert_eq!(storage.visitor_counts(), before);

        let limited = super::execute(
            &storage,
            &super::parse_sql(&format!("{query} LIMIT 1"), &[]).unwrap(),
        )
        .unwrap();
        assert_eq!(limited.rows, vec![row(json!({"left_id": 0}))]);
        // The build side is retained, then one probe and one pair suffice.
        assert_eq!(storage.visitor_counts().0 - before.0, 1_002);

        for order in ["", " ORDER BY l.id LIMIT 1"] {
            let before = storage.visitor_counts();
            // Reject every output row so the result-row cap cannot mask the
            // candidate-pair guard. ORDER BY must finish even with LIMIT 1.
            let plan = super::parse_sql(&format!("{query} WHERE l.id < 0{order}"), &[]).unwrap();
            let error = super::execute(&storage, &plan).unwrap_err();
            assert_eq!(error.code, "INVALID_QUERY");
            assert!(
                error.message.contains("1000000 candidate pairs"),
                "{error:?}"
            );
            assert!(storage.visitor_counts().0 > before.0);
        }
    }

    #[test]
    fn candidate_pair_budget_is_shared_across_join_stages() {
        let storage = large_join_database(&[("a", 600), ("b", 1_000), ("c", 1)]).into_storage();
        // Each stage would examine 600,000 pairs, so a per-stage budget would
        // pass. The shared one-million-pair budget must reject the whole join.
        let plan = super::parse_sql(
            "SELECT a.id AS id FROM a JOIN b ON a.join_key = b.join_key \
             JOIN c ON b.join_key = c.join_key WHERE a.id < 0",
            &[],
        )
        .unwrap();
        let error = super::execute(&storage, &plan).unwrap_err();
        assert_eq!(error.code, "INVALID_QUERY");
        assert!(
            error.message.contains("1000000 candidate pairs"),
            "{error:?}"
        );
    }

    #[test]
    fn build_row_preflight_is_global() {
        assert_eq!(
            super::preflight_build_rows(&[1, 50_000, 50_001])
                .unwrap_err()
                .code,
            "QUERY_WORK_LIMIT_EXCEEDED"
        );
        super::preflight_build_rows(&[900_000, 50_000, 50_000]).unwrap();
    }

    #[test]
    fn multiple_build_tables_share_one_work_budget() {
        let mut engine = Engine::default();
        engine
            .execute_sql(
                "CREATE TABLE work_a (id INTEGER PRIMARY KEY, join_key INTEGER NOT NULL)",
                &[],
            )
            .unwrap();
        for table in ["work_b", "work_c"] {
            engine
                .execute_sql(
                    &format!(
                        "CREATE TABLE {table} (\
                            id INTEGER PRIMARY KEY, join_key INTEGER NOT NULL, payload TEXT NOT NULL\
                        )"
                    ),
                    &[],
                )
                .unwrap();
        }
        engine
            .execute_sql("INSERT INTO work_a (id, join_key) VALUES (1, 1)", &[])
            .unwrap();
        let payload = "x".repeat(crate::storage::MAX_LOGICAL_ROW_BYTES - 256);
        for (table, count) in [("work_b", 5), ("work_c", 4)] {
            seed_rows(
                &mut engine,
                table,
                (0..count)
                    .map(|id| {
                        row(json!({
                            "id": id + if table == "work_b" { 10 } else { 20 },
                            "join_key": 1,
                            "payload": payload,
                        }))
                    })
                    .collect(),
            );
        }
        assert_eq!(
            engine
                .query_sql(
                    "SELECT a.id AS id FROM work_a a \
                     JOIN work_b b ON a.join_key = b.join_key \
                     JOIN work_c c ON b.join_key = c.join_key",
                    &[],
                )
                .unwrap_err()
                .code,
            "QUERY_WORK_LIMIT_EXCEEDED"
        );
    }

    #[test]
    fn byte_budgets_are_checked_before_retaining_rows() {
        let mut budget = super::WorkBudget::default();
        budget.retain(super::MAX_JOIN_WORK_BYTES).unwrap();
        assert_eq!(budget.retained, super::MAX_JOIN_WORK_BYTES);
        assert_eq!(
            budget.retain(1).unwrap_err().code,
            "QUERY_WORK_LIMIT_EXCEEDED"
        );
        assert_eq!(budget.retained, super::MAX_JOIN_WORK_BYTES);

        super::ensure_result_budget(super::MAX_JOIN_RESULT_BYTES).unwrap();
        assert_eq!(
            super::ensure_result_budget(super::MAX_JOIN_RESULT_BYTES + 1)
                .unwrap_err()
                .code,
            "QUERY_WORK_LIMIT_EXCEEDED"
        );
    }

    #[test]
    fn execution_rejects_oversized_build_and_result_rows_before_cloning_them() {
        fn engine_with_payloads(left_payloads: Vec<String>, right_payloads: Vec<String>) -> Engine {
            let mut engine = Engine::default();
            for table in ["budget_left", "budget_right"] {
                engine
                    .execute_sql(
                        &format!(
                            "CREATE TABLE {table} (\
                                id INTEGER PRIMARY KEY, join_key INTEGER NOT NULL, \
                                payload TEXT NOT NULL\
                            )"
                        ),
                        &[],
                    )
                    .unwrap();
            }
            seed_rows(
                &mut engine,
                "budget_left",
                left_payloads
                    .into_iter()
                    .enumerate()
                    .map(|(id, payload)| row(json!({"id": id, "join_key": 1, "payload": payload})))
                    .collect(),
            );
            seed_rows(
                &mut engine,
                "budget_right",
                right_payloads
                    .into_iter()
                    .enumerate()
                    .map(|(id, payload)| {
                        row(json!({"id": id + 100, "join_key": 1, "payload": payload}))
                    })
                    .collect(),
            );
            engine
        }

        let oversized = "x".repeat(crate::storage::MAX_LOGICAL_ROW_BYTES - 256);
        let build_error = engine_with_payloads(vec![String::new()], vec![oversized.clone(); 17])
            .query_sql(
                "SELECT l.id AS id FROM budget_left l JOIN budget_right r \
                 ON l.join_key = r.join_key",
                &[],
            )
            .unwrap_err();
        assert_eq!(build_error.code, "QUERY_WORK_LIMIT_EXCEEDED");

        let result_error = engine_with_payloads(vec![oversized; 17], vec![String::new()])
            .query_sql(
                "SELECT l.payload AS payload FROM budget_left l LEFT JOIN budget_right r \
                 ON l.join_key = r.join_key",
                &[],
            )
            .unwrap_err();
        assert_eq!(result_error.code, "QUERY_WORK_LIMIT_EXCEEDED");
    }
}
