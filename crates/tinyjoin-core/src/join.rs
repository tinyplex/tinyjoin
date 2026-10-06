use std::borrow::Cow;

use serde_json::{Map, Value};

use crate::aggregate::{encode_group_key, group_key_part};
use crate::expression::{
    Expression, Names, bind_expression, evaluate, parse_expression_at, value_type,
};
use crate::hash::{KeyMap, KeySet};
use crate::query::{
    Filter, ParseMode, Token, is_distinct_keyword_at, is_reserved_keyword, parse_predicate_at,
};
use crate::row::{Columns, RowRef, ValueRef};
use crate::storage::{KeyOrder, StorageReader};
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

#[derive(Clone, Debug, PartialEq)]
struct Source {
    table: String,
    alias: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JoinKind {
    Inner,
    Left,
}

#[derive(Clone, Debug, PartialEq)]
struct JoinCondition {
    left: ColumnRef,
    right: ColumnRef,
}

#[derive(Clone, Debug, PartialEq)]
struct JoinStage {
    source: Source,
    kind: JoinKind,
    conditions: Vec<JoinCondition>,
}

#[derive(Clone, Debug, PartialEq)]
struct Projection {
    source: ColumnRef,
    output: String,
    /// An output worked out from the joined row rather than read from `source`, which is then
    /// empty.
    expression: Option<Expression>,
    /// `*`, or `alias.*` where `source` has a qualifier: every column of every table, or of one,
    /// which execution lists once it reads the tables' schemas.
    star: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct JoinOrder {
    source: OrderSource,
    direction: OrderDirection,
    nulls: NullOrder,
}

#[derive(Clone, Debug, PartialEq)]
enum OrderSource {
    Column(ColumnRef),
    Output(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct JoinPlan {
    /// `SELECT DISTINCT`: joined rows with equal projected values are returned once.
    distinct: bool,
    projections: Vec<Projection>,
    /// Whether the outputs are keyed by position because their names repeat.
    positional: bool,
    /// Whether rows are read as arrays, so that outputs `*` lists may repeat names.
    array_rows: bool,
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
    schema: std::rc::Rc<TableDefinition>,
}

struct OrderedJoinedRow {
    row: Row,
    keys: Vec<Value>,
    projected_bytes: usize,
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

/// Keys a join's outputs by position if their names repeat. An `ORDER BY` name that is an
/// output's takes the key of that output.
pub(crate) fn position_outputs(plan: &mut JoinPlan) -> Result<()> {
    // The names a `*` lists are known only once the tables are read, which positions them.
    plan.array_rows = true;
    if plan.projections.iter().any(|projection| projection.star) {
        return Ok(());
    }
    let projections = &mut plan.projections;
    plan.positional =
        crate::query::names_repeat(projections.len(), &|index| &projections[index].output);
    if !plan.positional {
        return Ok(());
    }
    for (index, projection) in projections.iter_mut().enumerate() {
        crate::query::position_name(index, &mut projection.output);
    }
    for order in &mut plan.order_by {
        let OrderSource::Output(output) = &mut order.source else {
            continue;
        };
        let mut named = plan
            .projections
            .iter()
            .filter(|projection| projection.output[3..] == *output);
        if let Some(first) = named.next() {
            if named
                .any(|other| other.source != first.source || other.expression != first.expression)
            {
                return Err(crate::query::ambiguous_order(output));
            }
            output.clone_from(&first.output);
        }
    }
    Ok(())
}

/// Whether a statement joins tables, as a subquery it holds may without it doing so.
pub(crate) fn is_join_select(tokens: &[Token]) -> bool {
    let mut index = 0;
    while index < tokens.len() {
        if crate::query::is_keyword(tokens.get(index), "join") {
            return true;
        }
        index = crate::query::next_outer(tokens, index);
    }
    false
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
    for projection in &mut plan.projections {
        if let Some(expression) = &mut projection.expression {
            bind_expression(expression, params)?;
        }
    }
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
    if let Some(predicate) = crate::query::resolved_subqueries(storage, plan.predicate.as_ref())? {
        let mut plan = plan.clone();
        plan.predicate = Some(predicate);
        return execute(storage, &plan);
    }
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
    if plan.projections.iter().any(|projection| projection.star) {
        return execute(storage, &listed_stars(plan, &relations)?);
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
            values: None,
        });
    }

    let access = plan_access(storage, plan, &relations, &conditions)?;
    // Relations read whole, with no pushed terms to narrow them, preflight their sizes: the probe,
    // and every hash-joined relation, which is also retained.
    let mut counts = Vec::with_capacity(relations.len());
    for (source, relation) in relations.iter().enumerate() {
        let hashed = source == 0 || matches!(access.stages[source - 1], StageAccess::Hash);
        counts.push(if hashed && access.pushed[source].is_none() {
            storage.table_row_count(&relation.source.table)?
        } else {
            0
        });
    }
    let scan_rows = counts
        .iter()
        .fold(0_usize, |total, count| total.saturating_add(*count));
    if scan_rows > MAX_SCAN_ROWS {
        return Err(scan_limit_error());
    }
    preflight_build_rows(&counts)?;

    // The WHERE clause reads each relation's columns in place, numbered relation by relation.
    let offsets = relations
        .iter()
        .scan(0, |next, relation| {
            let offset = *next;
            *next += relation.schema.columns.len();
            Some(offset)
        })
        .collect::<Vec<_>>();
    let filter = JoinFilter {
        filter: Filter::resolved(plan.predicate.as_ref(), "joined row", &|column| {
            let (source, index, _) = resolve_column(&parse_column_ref_text(column), &relations)?;
            Ok(offsets[source] + index)
        })?,
        offsets: &offsets,
    };

    let join = Join {
        plan,
        relations: &relations,
        conditions: &conditions,
        access: &access,
    };
    let rows = if plan.order_by.is_empty() {
        execute_unordered(storage, &join, &filter)?
    } else {
        execute_ordered(storage, &join, &filter)?
    };

    Ok(QueryResult {
        revision: storage.revision(),
        fields,
        rows,
        values: None,
    })
}

/// The plan with each `*` replaced by the columns it reads, every table's in the order the query
/// joins them and each table's in the order it declares them, as in PostgreSQL.
fn listed_stars(plan: &JoinPlan, relations: &[Relation]) -> Result<JoinPlan> {
    let mut listed = plan.clone();
    listed.projections.clear();
    for projection in &plan.projections {
        if !projection.star {
            listed.projections.push(projection.clone());
            continue;
        }
        let mut found = false;
        for relation in relations {
            if projection
                .source
                .qualifier
                .as_ref()
                .is_some_and(|qualifier| *qualifier != relation.source.alias)
            {
                continue;
            }
            found = true;
            for column in &relation.schema.columns {
                listed.projections.push(Projection {
                    source: ColumnRef {
                        qualifier: Some(relation.source.alias.clone()),
                        column: column.name.clone(),
                    },
                    output: column.name.clone(),
                    expression: None,
                    star: false,
                });
            }
        }
        if !found {
            return Err(EngineError::invalid_query(format!(
                "No table of the query is named `{}`",
                projection.source.qualifier.as_deref().unwrap_or_default()
            )));
        }
    }
    if listed.projections.len() > MAX_PROJECTIONS {
        return Err(EngineError::invalid_query(format!(
            "A JOIN projection cannot contain more than {MAX_PROJECTIONS} columns"
        )));
    }
    if plan.array_rows {
        position_outputs(&mut listed)?;
    }
    Ok(listed)
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
    join: &Join<'_>,
    filter: &JoinFilter<'_>,
) -> Result<Vec<Row>> {
    let Join {
        plan, relations, ..
    } = *join;
    let mut rows = Vec::new();
    let mut skipped = 0_usize;
    let mut result_bytes = 0_usize;
    let mut seen = KeySet::default();
    visit_joined_rows(storage, join, &mut |bindings, budget| {
        if !filter.matches(bindings, relations)?
            || !first_distinct_row(&mut seen, bindings, plan, relations, budget)?
        {
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
    })?;
    Ok(rows)
}

fn execute_ordered(
    storage: &dyn StorageReader,
    join: &Join<'_>,
    filter: &JoinFilter<'_>,
) -> Result<Vec<Row>> {
    let Join {
        plan, relations, ..
    } = *join;
    let mut joined_rows = Vec::new();
    let mut seen = KeySet::default();
    visit_joined_rows(storage, join, &mut |bindings, budget| {
        // DISTINCT may keep the first of several equal rows because every ordering key is a
        // projected value, which validation guarantees, so equal rows also sort equally.
        if !filter.matches(bindings, relations)?
            || !first_distinct_row(&mut seen, bindings, plan, relations, budget)?
        {
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
        });
        Ok(VisitControl::Continue)
    })?;

    let mut order = Vec::with_capacity(plan.order_by.len());
    for term in &plan.order_by {
        order.push((term.direction, term.nulls));
    }
    let positions = {
        let mut keys = Vec::with_capacity(joined_rows.len() * order.len());
        for joined in &joined_rows {
            keys.extend(&joined.keys);
        }
        crate::query::ordered_positions(&keys, &order)
    };
    let mut rows = Vec::new();
    let mut result_bytes = 0_usize;
    for position in positions
        .into_iter()
        .skip(plan.offset)
        .take(plan.limit.unwrap_or(usize::MAX))
    {
        let joined = &mut joined_rows[position];
        let next_result_bytes = checked_add(result_bytes, joined.projected_bytes)?;
        ensure_result_budget(next_result_bytes)?;
        rows.push(std::mem::take(&mut joined.row));
        result_bytes = next_result_bytes;
    }
    Ok(rows)
}

/// Receives each joined combination of rows, one binding per relation in join order.
type JoinedRowVisitor<'v> =
    dyn for<'a> FnMut(&[Option<&'a Row>], &mut WorkBudget) -> Result<VisitControl> + 'v;

/// A validated join and how it reads each relation.
#[derive(Clone, Copy)]
struct Join<'a> {
    plan: &'a JoinPlan,
    relations: &'a [Relation],
    conditions: &'a [Vec<ResolvedCondition>],
    access: &'a Access,
}

/// How a join reads its relations. The probe, and each relation joined without `LEFT`, first
/// drops the rows that fail the `WHERE` terms that read only that relation, and uses those terms to
/// narrow its reads as a single-table query would. Each later relation is then read by looking up
/// the rows matching each combination of earlier rows, through its primary key or an index its
/// `ON` equalities fix, or else from a hash table of its rows keyed by those equalities.
///
/// Every path finds a combination's matches in primary-key order, as scanning the relation and
/// testing each row would, so output keeps left-major order. The complete `WHERE` clause is still
/// evaluated on every joined row.
struct Access {
    /// The `WHERE` terms each relation's rows must meet, in that relation's own column names.
    pushed: Vec<Option<Predicate>>,
    stages: Vec<StageAccess>,
}

enum StageAccess {
    /// Rows whose `columns` equal values of earlier relations: the complete primary key when
    /// `index` is `None`, or else every column of the index on `index`.
    Lookup {
        index: Option<Vec<String>>,
        columns: Vec<KeyColumn>,
    },
    Hash,
}

/// A column a lookup fixes, and the earlier relation's column whose value it takes.
struct KeyColumn {
    column: String,
    data_type: ColumnType,
    source: usize,
    source_column: String,
}

fn plan_access(
    storage: &dyn StorageReader,
    plan: &JoinPlan,
    relations: &[Relation],
    conditions: &[Vec<ResolvedCondition>],
) -> Result<Access> {
    let mut terms = vec![Vec::new(); relations.len()];
    let conjuncts = match &plan.predicate {
        Some(Predicate::And { predicates }) => predicates.iter().collect(),
        Some(predicate) => vec![predicate],
        None => Vec::new(),
    };
    for conjunct in conjuncts {
        // A relation a LEFT JOIN may null-extend must keep every row: its null extension is
        // decided by the ON clause alone, and WHERE then sees it.
        if let Some(source) = single_source(conjunct, relations)?
            && (source == 0 || plan.joins[source - 1].kind == JoinKind::Inner)
        {
            terms[source].push(renamed_to_source(conjunct));
        }
    }
    let pushed = terms
        .into_iter()
        .map(|mut terms| match terms.len() {
            0 => None,
            1 => terms.pop(),
            _ => Some(Predicate::And { predicates: terms }),
        })
        .collect();

    let mut stages = Vec::with_capacity(plan.joins.len());
    for (stage, conditions) in conditions.iter().enumerate() {
        let source = stage + 1;
        let relation = &relations[source];
        // The earlier column each of this relation's columns is equal to.
        let fixed = |column: &str| {
            conditions.iter().find_map(|condition| {
                let (own, other) = if condition.right_source == source {
                    (
                        &condition.right_column,
                        (condition.left_source, &condition.left_column),
                    )
                } else {
                    (
                        &condition.left_column,
                        (condition.right_source, &condition.right_column),
                    )
                };
                (own == column).then_some(other)
            })
        };
        let key = |columns: &[String]| {
            let mut key = Vec::with_capacity(columns.len());
            for column in columns {
                let (source, source_column) = fixed(column)?;
                let data_type = relation
                    .schema
                    .columns
                    .iter()
                    .find(|definition| definition.name == *column)?
                    .data_type;
                key.push(KeyColumn {
                    column: column.clone(),
                    data_type,
                    source,
                    source_column: source_column.clone(),
                });
            }
            Some(key)
        };
        let mut access = key(&relation.schema.primary_key).map(|columns| StageAccess::Lookup {
            index: None,
            columns,
        });
        if access.is_none() && storage.visits_indexes(&relation.source.table) {
            for definition in storage.indexes_for_table(&relation.source.table)? {
                if let Some(columns) = key(&definition.columns) {
                    access = Some(StageAccess::Lookup {
                        index: Some(definition.columns),
                        columns,
                    });
                    break;
                }
            }
        }
        stages.push(access.unwrap_or(StageAccess::Hash));
    }
    Ok(Access { pushed, stages })
}

/// The one relation every column a predicate reads belongs to, if there is one.
fn single_source(predicate: &Predicate, relations: &[Relation]) -> Result<Option<usize>> {
    fn visit(
        predicate: &Predicate,
        relations: &[Relation],
        found: &mut Option<Option<usize>>,
    ) -> Result<()> {
        match predicate {
            Predicate::Comparison { column, .. }
            | Predicate::IsNull { column, .. }
            | Predicate::In { column, .. }
            | Predicate::Like { column, .. }
            | Predicate::Subquery { column, .. } => {
                let (source, _, _) = resolve_column(&parse_column_ref_text(column), relations)?;
                *found = match *found {
                    None => Some(Some(source)),
                    Some(Some(existing)) if existing == source => Some(Some(source)),
                    Some(_) => Some(None),
                };
            }
            Predicate::And { predicates } | Predicate::Or { predicates } => {
                for predicate in predicates {
                    visit(predicate, relations, found)?;
                }
            }
            Predicate::Not { predicate } => visit(predicate, relations, found)?,
            Predicate::Expressions { left, right, .. } => {
                let mut names = Vec::new();
                left.column_names(&mut names);
                right.column_names(&mut names);
                for name in names {
                    let (source, _, _) = resolve_column(&parse_column_ref_text(name), relations)?;
                    *found = match *found {
                        None => Some(Some(source)),
                        Some(Some(existing)) if existing == source => Some(Some(source)),
                        Some(_) => Some(None),
                    };
                }
            }
        }
        Ok(())
    }
    let mut found = None;
    visit(predicate, relations, &mut found)?;
    Ok(found.flatten())
}

/// A predicate reading one relation, with its columns named as that relation names them.
fn renamed_to_source(predicate: &Predicate) -> Predicate {
    let mut predicate = predicate.clone();
    fn rename(predicate: &mut Predicate) {
        match predicate {
            Predicate::Comparison { column, .. }
            | Predicate::IsNull { column, .. }
            | Predicate::In { column, .. }
            | Predicate::Like { column, .. }
            | Predicate::Subquery { column, .. } => {
                *column = parse_column_ref_text(column).column;
            }
            Predicate::And { predicates } | Predicate::Or { predicates } => {
                predicates.iter_mut().for_each(rename)
            }
            Predicate::Not { predicate } => rename(predicate),
            Predicate::Expressions { left, right, .. } => {
                let source = |name: &str| parse_column_ref_text(name).column;
                left.rename_columns(&source);
                right.rename_columns(&source);
            }
        }
    }
    rename(&mut predicate);
    predicate
}

/// One part of a hash-join key: equal values encode equally, numbers by their `f64` value with
/// zero's sign ignored, as join equality compares them.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum KeyPart {
    Number(u64),
    Text(String),
    Boolean(bool),
}

impl KeyPart {
    /// The part for a value, or `None` for `NULL`, which never matches.
    fn new(value: &Value) -> Option<Self> {
        match value {
            Value::Number(number) => {
                let number = number.as_f64()?;
                Some(Self::Number(if number == 0.0 {
                    0
                } else {
                    number.to_bits()
                }))
            }
            Value::String(text) => Some(Self::Text(text.clone())),
            Value::Bool(value) => Some(Self::Boolean(*value)),
            _ => None,
        }
    }
}

/// A hash-joined relation's rows that can match, grouped by the values of its `ON` columns.
struct HashTable {
    rows: Vec<Row>,
    buckets: KeyMap<Vec<KeyPart>, Vec<usize>>,
}

/// Which side of each of a stage's `ON` equalities belongs to its new relation: its own column,
/// then the earlier relation and column it equals.
fn stage_sides(conditions: &[ResolvedCondition], source: usize) -> Vec<(&str, usize, &str)> {
    conditions
        .iter()
        .map(|condition| {
            if condition.right_source == source {
                (
                    condition.right_column.as_str(),
                    condition.left_source,
                    condition.left_column.as_str(),
                )
            } else {
                (
                    condition.left_column.as_str(),
                    condition.right_source,
                    condition.right_column.as_str(),
                )
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn build_hash_table(
    storage: &dyn StorageReader,
    join: &Join<'_>,
    source: usize,
    filter: &Filter<'_>,
    scanned: &mut usize,
    build_count: &mut usize,
    budget: &mut WorkBudget,
) -> Result<HashTable> {
    let relation = &join.relations[source];
    let sides = stage_sides(&join.conditions[source - 1], source);
    let mut table = HashTable {
        rows: Vec::new(),
        buckets: KeyMap::default(),
    };
    let outcome = crate::query::visit_predicate_candidates(
        storage,
        &relation.source.table,
        join.access.pushed[source].as_ref(),
        &relation.schema,
        KeyOrder::Ascending,
        filter,
        &mut |rows| count_scanned_rows(scanned, rows),
        &mut |row| {
            let row = row.to_row()?;
            // A row with a NULL key matches nothing, so it need not be kept.
            let mut key = Vec::with_capacity(sides.len());
            for (column, ..) in &sides {
                let value = row
                    .get(*column)
                    .ok_or_else(|| missing_join_column_error(column))?;
                let Some(part) = KeyPart::new(value) else {
                    return Ok(VisitControl::Continue);
                };
                key.push(part);
            }
            *build_count = build_count.saturating_add(1);
            if *build_count > MAX_JOIN_BUILD_ROWS {
                return Err(build_rows_limit_error());
            }
            let key_bytes = key.iter().try_fold(64_usize, |bytes, part| {
                checked_add(
                    bytes,
                    match part {
                        KeyPart::Text(text) => checked_add(32, text.len())?,
                        _ => 16,
                    },
                )
            })?;
            budget.retain(checked_add(owned_row_bytes(&row)?, key_bytes)?)?;
            table.buckets.entry(key).or_default().push(table.rows.len());
            table.rows.push(row);
            Ok(VisitControl::Continue)
        },
    )?;
    if outcome != VisitOutcome::Complete {
        return Err(malformed_reader_error(
            "A join build visitor stopped before completing",
        ));
    }
    Ok(table)
}

fn visit_joined_rows(
    storage: &dyn StorageReader,
    join: &Join<'_>,
    visitor: &mut JoinedRowVisitor<'_>,
) -> Result<()> {
    let Join {
        plan, relations, ..
    } = *join;
    let mut filters = Vec::with_capacity(relations.len());
    for (relation, pushed) in relations.iter().zip(&join.access.pushed) {
        filters.push(Filter::new(
            pushed.as_ref(),
            &relation.schema,
            &relation.source.table,
        )?);
    }
    let mut budget = WorkBudget::default();
    let mut scanned = 0_usize;
    let mut build_count = 0_usize;
    let mut tables = Vec::with_capacity(plan.joins.len());
    for (stage, access) in join.access.stages.iter().enumerate() {
        tables.push(match access {
            StageAccess::Hash => Some(build_hash_table(
                storage,
                join,
                stage + 1,
                &filters[stage + 1],
                &mut scanned,
                &mut build_count,
                &mut budget,
            )?),
            StageAccess::Lookup { .. } => None,
        });
    }

    let extend = Extend {
        storage,
        join,
        tables: &tables,
        filters: &filters,
    };
    let mut pairs = 0_usize;
    let mut stop_requested = false;
    let probe = &relations[0];
    let probe_outcome = crate::query::visit_predicate_candidates(
        storage,
        &probe.source.table,
        join.access.pushed[0].as_ref(),
        &probe.schema,
        KeyOrder::Ascending,
        &filters[0],
        &mut |rows| count_scanned_rows(&mut scanned, rows),
        &mut |row| {
            if stop_requested {
                return Ok(VisitControl::Stop);
            }
            let row = row.to_row()?;
            let mut bindings = [None; MAX_JOIN_SOURCES];
            bindings[0] = Some(&row);
            if extend.visit(
                0,
                &bindings[..relations.len()],
                &mut pairs,
                &mut budget,
                visitor,
            )? == VisitControl::Stop
            {
                stop_requested = true;
                return Ok(VisitControl::Stop);
            }
            Ok(VisitControl::Continue)
        },
    )?;
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

/// Extends combinations of joined rows one stage at a time.
struct Extend<'a> {
    storage: &'a dyn StorageReader,
    join: &'a Join<'a>,
    tables: &'a [Option<HashTable>],
    filters: &'a [Filter<'a>],
}

impl Extend<'_> {
    fn visit(
        &self,
        stage: usize,
        bindings: &[Option<&Row>],
        pairs: &mut usize,
        budget: &mut WorkBudget,
        visitor: &mut JoinedRowVisitor<'_>,
    ) -> Result<VisitControl> {
        let plan = self.join.plan;
        if stage == plan.joins.len() {
            return visitor(bindings, budget);
        }
        let source = stage + 1;
        let relation = &self.join.relations[source];
        let conditions = &self.join.conditions[stage];
        // A hash-joined relation's candidates are its rows with equal key values. A looked-up
        // relation's are the rows its lookup finds, which must also meet its pushed terms.
        let table = self.tables[stage].as_ref();
        let found = match &self.join.access.stages[stage] {
            StageAccess::Lookup { index, columns } => match lookup_key(columns, bindings)? {
                Some(key) => self.lookup(relation, index.as_deref(), &key)?,
                None => Vec::new(),
            },
            StageAccess::Hash => Vec::new(),
        };
        let mut candidates: Vec<&Row> = Vec::new();
        match table {
            Some(table) => {
                // A NULL in the key matches nothing.
                let sides = stage_sides(conditions, source);
                let mut key = Vec::with_capacity(sides.len());
                for (_, other, column) in sides {
                    match earlier_value(bindings, other, column)?.and_then(KeyPart::new) {
                        Some(part) => key.push(part),
                        None => break,
                    }
                }
                if key.len() == conditions.len()
                    && let Some(bucket) = table.buckets.get(&key)
                {
                    for index in bucket {
                        candidates.push(&table.rows[*index]);
                    }
                }
            }
            None => candidates.extend(&found),
        }
        let mut local = [None; MAX_JOIN_SOURCES];
        local[..bindings.len()].copy_from_slice(bindings);
        let mut matched = false;
        for row in candidates {
            count_join_pair(pairs)?;
            self.storage.charge_work(1)?;
            if table.is_none()
                && !self.filters[source].matches(&RowRef::map(row, &relation.schema))?
            {
                continue;
            }
            local[source] = Some(row);
            if !conditions_match(conditions, &local[..bindings.len()])? {
                continue;
            }
            matched = true;
            if self.visit(stage + 1, &local[..bindings.len()], pairs, budget, visitor)?
                == VisitControl::Stop
            {
                return Ok(VisitControl::Stop);
            }
        }
        if plan.joins[stage].kind == JoinKind::Left && !matched {
            local[source] = None;
            if self.visit(stage + 1, &local[..bindings.len()], pairs, budget, visitor)?
                == VisitControl::Stop
            {
                return Ok(VisitControl::Stop);
            }
        }
        Ok(VisitControl::Continue)
    }

    /// The rows of `relation` with the complete primary key or index tuple `key`, in primary-key
    /// order.
    fn lookup(&self, relation: &Relation, index: Option<&[String]>, key: &Row) -> Result<Vec<Row>> {
        let table = &relation.source.table;
        let Some(columns) = index else {
            return Ok(self
                .storage
                .lookup_primary_key(table, key)?
                .into_iter()
                .collect());
        };
        let mut rows = Vec::new();
        let outcome = self.storage.visit_index(table, columns, key, &mut |row| {
            rows.push(row.to_row()?);
            Ok(VisitControl::Continue)
        })?;
        match outcome {
            Some(VisitOutcome::Complete) => Ok(rows),
            _ => Err(malformed_reader_error(
                "A join lookup could not read the index it planned to use",
            )),
        }
    }
}

/// An earlier relation's value for an `ON` equality, or `None` when that relation has no row, as in
/// an unmatched `LEFT JOIN`.
fn earlier_value<'a>(
    bindings: &[Option<&'a Row>],
    source: usize,
    column: &str,
) -> Result<Option<&'a Value>> {
    bindings[source]
        .map(|row| {
            row.get(column)
                .ok_or_else(|| missing_join_column_error(column))
        })
        .transpose()
}

/// The values a lookup fixes, converted to the looked-up columns' types, or `None` when one is
/// `NULL` or cannot equal any value of its column, so that nothing matches.
fn lookup_key(columns: &[KeyColumn], bindings: &[Option<&Row>]) -> Result<Option<Row>> {
    let mut key = Row::new();
    for column in columns {
        let Some(value) = earlier_value(bindings, column.source, &column.source_column)? else {
            return Ok(None);
        };
        let Some(value) = lookup_value(column.data_type, value) else {
            return Ok(None);
        };
        key.insert(column.column.clone(), value);
    }
    Ok(Some(key))
}

/// A value as a lookup of a `data_type` column takes it, if some value of that type equals it.
fn lookup_value(data_type: ColumnType, value: &Value) -> Option<Value> {
    Some(match (data_type, value) {
        (ColumnType::Integer, Value::Number(number)) => match number.as_i64() {
            Some(value) => Value::from(value),
            None => {
                let number = number.as_f64()?;
                const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
                if number.fract() != 0.0 || number.abs() > MAX_SAFE_INTEGER {
                    return None;
                }
                Value::from(number as i64)
            }
        },
        (ColumnType::Float, Value::Number(number)) => Value::from(number.as_f64()?),
        (ColumnType::Text, Value::String(_)) | (ColumnType::Boolean, Value::Bool(_)) => {
            value.clone()
        }
        _ => return None,
    })
}

/// Reports whether a matching joined row is the first with its projected values, remembering it if
/// so. Without DISTINCT every row is first. Remembered keys are retained work for the whole join.
fn first_distinct_row(
    seen: &mut KeySet<String>,
    bindings: &[Option<&Row>],
    plan: &JoinPlan,
    relations: &[Relation],
    budget: &mut WorkBudget,
) -> Result<bool> {
    if !plan.distinct {
        return Ok(true);
    }
    let mut parts = Vec::with_capacity(plan.projections.len());
    let mut input_bytes = 32_usize;
    for projection in &plan.projections {
        let (_, _, definition) = resolve_column(&projection.source, relations)?;
        let value = joined_value(bindings, &projection.source, relations)?;
        // A text part is JSON-escaped twice while the key is encoded; see the aggregate executor.
        input_bytes = checked_add(input_bytes, checked_mul(owned_value_bytes(value)?, 14)?)?;
        budget.ensure_transient(input_bytes)?;
        parts.push(group_key_part(
            definition.data_type,
            value,
            &projection.source.column,
        )?);
    }
    let key = encode_group_key(&parts)?;
    if seen.contains(&key) {
        return Ok(false);
    }
    budget.retain(checked_add(64, checked_mul(key.len(), 2)?)?)?;
    seen.insert(key);
    Ok(true)
}

/// A join's WHERE clause, and where each relation's columns start in the numbering it uses.
struct JoinFilter<'a> {
    filter: Filter<'a>,
    offsets: &'a [usize],
}

impl JoinFilter<'_> {
    fn matches(&self, bindings: &[Option<&Row>], relations: &[Relation]) -> Result<bool> {
        self.filter.matches(&JoinedRow {
            bindings,
            relations,
            offsets: self.offsets,
        })
    }
}

/// One combination of joined rows, whose columns are numbered relation by relation. A relation
/// with no row, as in an unmatched `LEFT JOIN`, reads as `NULL`.
struct JoinedRow<'a> {
    bindings: &'a [Option<&'a Row>],
    relations: &'a [Relation],
    offsets: &'a [usize],
}

impl Columns for JoinedRow<'_> {
    fn column(&self, index: usize) -> Result<ValueRef<'_>> {
        let source = self.offsets.partition_point(|offset| *offset <= index) - 1;
        let definition = &self.relations[source].schema.columns[index - self.offsets[source]];
        Ok(self.bindings[source]
            .and_then(|row| row.get(&definition.name))
            .map_or(ValueRef::Null, |value| {
                ValueRef::from_value(value, definition.data_type)
            }))
    }
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
    let mut aliases = KeySet::default();
    for relation in relations {
        validate_relation_shape(relation)?;
        if !aliases.insert(relation.source.alias.as_str()) {
            return Err(EngineError::invalid_query(format!(
                "Table alias `{}` is used more than once",
                relation.source.alias
            )));
        }
    }
    let mut conditions = Vec::with_capacity(plan.joins.len());
    for (stage, join) in plan.joins.iter().enumerate() {
        let mut resolved = Vec::with_capacity(join.conditions.len());
        for condition in &join.conditions {
            resolved.push(resolved_condition(condition, relations, stage + 1)?);
        }
        conditions.push(resolved);
    }

    let mut outputs = KeySet::default();
    let mut fields = Vec::with_capacity(plan.projections.len());
    let mut projected = Vec::with_capacity(plan.projections.len());
    for projection in &plan.projections {
        if !outputs.insert(projection.output.as_str()) {
            return Err(EngineError::invalid_query(format!(
                "SELECT produces output column `{}` more than once; use distinct AS aliases",
                projection.output
            )));
        }
        let data_type = match &projection.expression {
            Some(_) if plan.distinct => {
                return Err(EngineError::unsupported_sql(
                    "SELECT DISTINCT cannot compare an output worked out from the row",
                ));
            }
            Some(_) => projection_type(projection, relations)?,
            None => {
                let (source, index, definition) = resolve_column(&projection.source, relations)?;
                if plan.distinct && definition.data_type == ColumnType::Json {
                    return Err(EngineError::type_mismatch(format!(
                        "JSON column `{}` cannot be compared by SELECT DISTINCT",
                        projection.source.column
                    )));
                }
                projected.push((source, index));
                definition.data_type
            }
        };
        fields.push(ResultField::new(
            crate::query::field_name(&projection.output, plan.positional),
            data_type,
        ));
    }
    if let Some(predicate) = &plan.predicate {
        validate_join_predicate(predicate, relations)?;
    }
    for order in &plan.order_by {
        match &order.source {
            OrderSource::Column(column) => {
                let (source, index, definition) = resolve_column(column, relations)?;
                if definition.data_type == ColumnType::Json {
                    return Err(EngineError::type_mismatch("JSON columns cannot be ordered"));
                }
                if plan.distinct && !projected.contains(&(source, index)) {
                    return Err(EngineError::invalid_query(format!(
                        "SELECT DISTINCT can only be ordered by projected columns, not `{}`",
                        column.column
                    )));
                }
            }
            OrderSource::Output(output) => {
                let projection = plan
                    .projections
                    .iter()
                    .find(|projection| projection.output == *output)
                    .ok_or_else(|| EngineError::column_not_found(output, "joined output"))?;
                if projection_type(projection, relations)? == ColumnType::Json {
                    return Err(EngineError::type_mismatch(format!(
                        "JSON column `{}` cannot be ordered",
                        projection.source.column
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
        Predicate::IsNull { column, .. } | Predicate::Subquery { column, .. } => {
            resolve_column(&parse_column_ref_text(column), relations)?;
            Ok(())
        }
        Predicate::Like {
            column,
            pattern,
            escape,
            case_insensitive,
        } => {
            let reference = parse_column_ref_text(column);
            let (_, _, definition) = resolve_column(&reference, relations)?;
            crate::query::validate_like(
                definition,
                pattern,
                escape.as_ref(),
                *case_insensitive,
                "joined tables",
            )
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
        Predicate::Expressions {
            left,
            operator,
            right,
        } => crate::expression::check_comparison(left, *operator, right, &|name| {
            let (_, _, definition) = resolve_column(&parse_column_ref_text(name), relations)?;
            Ok(definition.data_type)
        }),
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

/// The type of an output's values: its column's, or the type of the values its expression gives,
/// which is text where it gives only NULL, as PostgreSQL types an untyped NULL.
fn projection_type(projection: &Projection, relations: &[Relation]) -> Result<ColumnType> {
    let column_type = |name: &str| {
        let (_, _, definition) = resolve_column(&parse_column_ref_text(name), relations)?;
        Ok(definition.data_type)
    };
    match &projection.expression {
        Some(expression) => Ok(value_type(expression, &column_type)?.unwrap_or(ColumnType::Text)),
        None => Ok(resolve_column(&projection.source, relations)?.2.data_type),
    }
}

/// An output's value in a joined row: its column's, or the one its expression works out.
fn projected_value<'a>(
    bindings: &[Option<&'a Row>],
    projection: &Projection,
    relations: &[Relation],
) -> Result<Cow<'a, Value>> {
    match &projection.expression {
        Some(expression) => evaluate(expression, &mut |name, _| {
            joined_value(bindings, &parse_column_ref_text(name), relations).cloned()
        })
        .map(Cow::Owned),
        None => joined_value(bindings, &projection.source, relations).map(Cow::Borrowed),
    }
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
            projected_value(bindings, projection, relations)?.into_owned(),
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
    let mut keys = Vec::with_capacity(plan.order_by.len());
    for order in &plan.order_by {
        keys.push(match &order.source {
            OrderSource::Column(reference) => joined_value(bindings, reference, relations)?.clone(),
            OrderSource::Output(output) => {
                let projection = plan
                    .projections
                    .iter()
                    .find(|projection| projection.output == *output)
                    .expect("output alias was validated");
                projected_value(bindings, projection, relations)?.into_owned()
            }
        });
    }
    Ok(keys)
}

fn order_keys_bytes(
    bindings: &[Option<&Row>],
    plan: &JoinPlan,
    relations: &[Relation],
) -> Result<usize> {
    let mut bytes = 0;
    for order in &plan.order_by {
        let value = match &order.source {
            OrderSource::Column(reference) => {
                Cow::Borrowed(joined_value(bindings, reference, relations)?)
            }
            OrderSource::Output(output) => {
                let projection = plan
                    .projections
                    .iter()
                    .find(|projection| projection.output == *output)
                    .expect("output alias was validated");
                projected_value(bindings, projection, relations)?
            }
        };
        bytes = checked_add(bytes, 32)?;
        bytes = checked_add(bytes, owned_value_bytes(&value)?)?;
    }
    Ok(bytes)
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
                owned_value_bytes(projected_value(bindings, projection, relations)?.as_ref())?,
                2,
            )?,
        )?;
    }
    Ok(bytes)
}

fn ordered_row_bytes(projected_bytes: usize, key_bytes: usize) -> Result<usize> {
    checked_add(checked_add(64, projected_bytes)?, key_bytes)
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

fn count_scanned_rows(scanned: &mut usize, rows: usize) -> Result<()> {
    *scanned = scanned.saturating_add(rows);
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
        let distinct = is_distinct_keyword_at(&self.tokens, self.position);
        if distinct {
            self.position += 1;
            if self.consume_keyword("on") {
                return Err(EngineError::unsupported_sql(
                    "SELECT DISTINCT ON is not supported",
                ));
            }
        }
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
        let (limit, offset) = crate::query::parse_limit_offset(
            &self.tokens,
            &mut self.position,
            self.params,
            self.mode,
        )?;
        self.consume_semicolon();
        if self.position != self.tokens.len() {
            return Err(EngineError::unsupported_sql(
                "This join subset supports equijoin ON clauses, WHERE, ORDER BY, LIMIT, and OFFSET",
            ));
        }
        Ok(JoinPlan {
            distinct,
            projections,
            positional: false,
            array_rows: false,
            first,
            joins,
            predicate,
            order_by,
            limit,
            offset,
        })
    }

    fn parse_projections(&mut self) -> Result<Vec<Projection>> {
        let mut projections = Vec::new();
        loop {
            if projections.len() >= MAX_PROJECTIONS {
                return Err(EngineError::invalid_query(format!(
                    "A JOIN projection cannot contain more than {MAX_PROJECTIONS} columns"
                )));
            }
            let qualified_star = matches!(
                (
                    self.tokens.get(self.position),
                    self.tokens.get(self.position + 1),
                    self.tokens.get(self.position + 2),
                ),
                (
                    Some(Token::Identifier { .. }),
                    Some(Token::Dot),
                    Some(Token::Star)
                )
            );
            if qualified_star || self.consume_star() {
                let qualifier = if qualified_star {
                    let qualifier = self.parse_identifier()?;
                    self.position += 2;
                    Some(qualifier)
                } else {
                    None
                };
                projections.push(Projection {
                    source: ColumnRef {
                        qualifier,
                        column: String::new(),
                    },
                    output: String::new(),
                    expression: None,
                    star: true,
                });
                if !self.consume_comma() {
                    break;
                }
                continue;
            }
            let (source, expression) = match parse_expression_at(
                &self.tokens,
                &mut self.position,
                self.params,
                Names::Row,
            )? {
                Expression::Column(column) => (parse_column_ref_text(&column), None),
                expression => (parse_column_ref_text(""), Some(expression)),
            };
            let output = if self.consume_keyword("as") {
                self.parse_identifier()?
            } else if expression.is_some() {
                // As in PostgreSQL, an expression without an alias is unnamed.
                "?column?".to_owned()
            } else {
                source.column.clone()
            };
            projections.push(Projection {
                source,
                output,
                expression,
                star: false,
            });
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
            visitor: &mut dyn FnMut(&crate::row::RowRef<'_>) -> Result<VisitControl>,
        ) -> Result<VisitOutcome> {
            if matches!(self.fault, ReaderFault::StopRight) && table == "right_items" {
                return Ok(VisitOutcome::Stopped);
            }
            if matches!(self.fault, ReaderFault::OmitLeftJoinKey) && table == "left_items" {
                let schema = self.inner.table_schema(table)?;
                return self.inner.visit_table(table, &mut |row| {
                    let mut malformed = row.to_row()?;
                    malformed.remove("k1");
                    visitor(&crate::row::RowRef::map(&malformed, &schema))
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
            visitor: &mut dyn FnMut(&crate::row::RowRef<'_>) -> Result<VisitControl>,
        ) -> Result<Option<VisitOutcome>> {
            self.inner.visit_index(table, columns, key, visitor)
        }

        fn table_schema(&self, table: &str) -> Result<std::rc::Rc<TableDefinition>> {
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
                    table: std::rc::Rc::from(table),
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
    fn select_distinct_collapses_rows_the_join_multiplied() {
        let engine = database();
        let values = |sql: &str, params: &[Value]| {
            engine
                .query_sql(sql, params)
                .unwrap()
                .rows
                .into_iter()
                .map(|row| row.into_iter().next().unwrap().1)
                .collect::<Vec<_>>()
        };
        let from = "FROM left_items AS l JOIN right_items AS r ON l.k1 = r.k1";
        assert_eq!(
            values(&format!("SELECT DISTINCT l.k1 AS k {from} ORDER BY k"), &[]),
            [json!(10), json!(20), json!(30)]
        );
        assert_eq!(
            values(
                &format!("SELECT DISTINCT l.k1 AS k {from} ORDER BY l.k1 DESC"),
                &[]
            ),
            [json!(30), json!(20), json!(10)]
        );
        assert_eq!(
            values(
                &format!("SELECT DISTINCT r.k2 {from} WHERE r.score <= $1 ORDER BY k2 NULLS FIRST"),
                &[json!(6)],
            ),
            [Value::Null, json!("a"), json!("b")]
        );
        // Unordered pagination counts distinct rows in the join's left-major order.
        assert_eq!(
            values(
                &format!("SELECT DISTINCT l.label {from} LIMIT 2 OFFSET 1"),
                &[]
            ),
            [json!("L2"), json!("L3")]
        );
        // Every unmatched left row null-extends to the same projected row.
        assert_eq!(
            values(
                "SELECT DISTINCT r.label AS label FROM left_items AS l \
                 LEFT JOIN right_items AS r ON l.k1 = r.k1 AND l.k2 = r.k2 \
                 ORDER BY label NULLS FIRST",
                &[],
            ),
            [
                Value::Null,
                json!("R1"),
                json!("R2"),
                json!("R3"),
                json!("R4")
            ]
        );
        assert_eq!(
            engine
                .query_sql(
                    &format!("SELECT DISTINCT l.k1 AS k, r.k2 AS k2 {from} ORDER BY k, k2"),
                    &[]
                )
                .unwrap()
                .rows,
            vec![
                row(json!({"k": 10, "k2": "a"})),
                row(json!({"k": 10, "k2": "b"})),
                row(json!({"k": 20, "k2": "a"})),
                row(json!({"k": 30, "k2": null})),
            ]
        );

        for (sql, code) in [
            (
                format!("SELECT DISTINCT l.k1 AS k {from} ORDER BY r.score"),
                "INVALID_QUERY",
            ),
            (
                format!("SELECT DISTINCT ON (l.k1) l.k1 AS k {from}"),
                "UNSUPPORTED_SQL",
            ),
            // Both tables have an `id`, which object rows cannot hold twice.
            (format!("SELECT DISTINCT * {from}"), "INVALID_QUERY"),
        ] {
            assert_eq!(engine.query_sql(&sql, &[]).unwrap_err().code, code, "{sql}");
        }
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
            // Object rows cannot hold the `id` both tables have.
            (
                "SELECT * FROM left_items l JOIN right_items r ON l.k1 = r.k1",
                "INVALID_QUERY",
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
            // candidate-pair guard. ORDER BY must finish even with LIMIT 1. A
            // term reading both tables cannot be pushed into either one's reads.
            let plan = super::parse_sql(&format!("{query} WHERE l.id < 0 OR r.id < 0{order}"), &[])
                .unwrap();
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
             JOIN c ON b.join_key = c.join_key WHERE a.id < 0 OR c.id < 0",
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
