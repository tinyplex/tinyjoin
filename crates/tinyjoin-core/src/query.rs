use std::cmp::Ordering;
use std::collections::HashSet;
use std::str::FromStr;

use serde_json::{Map, Number, Value};

use crate::paged_codec::{encode_key_bound, encode_text_prefix_bounds};
use crate::row::{Columns, RowRef, ValueRef};
use crate::storage::{
    KeyOrder, KeyRange, StorageReader, estimated_row_bytes, estimated_value_bytes,
    validate_json_value,
};
use crate::{
    ColumnDefinition, ColumnType, ComparisonOperator, EngineError, NullOrder, OrderBy,
    OrderDirection, Predicate, QueryResult, Result, ResultField, Row, SelectColumn, SelectPlan,
    TableDefinition, VisitControl, VisitOutcome,
};

const MAX_SQL_BYTES: usize = 64 * 1024;
const MAX_SQL_TOKENS: usize = 4 * 1024;
const MAX_BOUND_PARAMETER_BYTES: usize = 16 * 1024 * 1024;
const MAX_PROJECTION_COLUMNS: usize = 256;
const MAX_PREDICATE_NODES: usize = 256;
const MAX_PREDICATE_DEPTH: usize = 32;
const MAX_IN_VALUES: usize = 1024;
const MAX_ORDER_COLUMNS: usize = 32;
pub(crate) const MAX_SQL_PARAMETERS: usize = 1024;
const MAX_COMMENT_DEPTH: usize = 32;
const MAX_RESULT_ROWS: usize = 100_000;
const MAX_QUERY_RESULT_BYTES: usize = 16 * 1024 * 1024;
const MAX_SCAN_ROWS: usize = 1_000_000;
const MAX_ORDERED_ROWS: usize = 100_000;

pub(crate) fn execute(storage: &dyn StorageReader, plan: &SelectPlan) -> Result<QueryResult> {
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
    let fields = select_fields(&schema, plan.columns.as_deref())?;
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

    if plan.limit == Some(0) {
        return Ok(QueryResult {
            revision: storage.revision(),
            fields,
            rows: Vec::new(),
        });
    }

    let rows = match row_order(plan, &schema) {
        RowOrder::Any => execute_unordered(storage, plan, &schema, KeyOrder::Ascending)?,
        // Rows ordered by a prefix of the primary key arrive in that order, so they stream and
        // stop at the limit, unless a descending query would read them through an index.
        RowOrder::Key(order)
            if storage.visits_in_key_order(&plan.table)
                && (order == KeyOrder::Ascending
                    || !secondary_index_applies(
                        storage,
                        &plan.table,
                        plan.predicate.as_ref(),
                        &schema,
                    )?) =>
        {
            execute_unordered(storage, plan, &schema, order)?
        }
        RowOrder::Key(_) | RowOrder::Sorted => execute_ordered(storage, plan, &schema)?,
    };

    Ok(QueryResult {
        revision: storage.revision(),
        fields,
        rows,
    })
}

pub(crate) fn projection_fields(
    schema: &TableDefinition,
    columns: Option<&[String]>,
) -> Result<Vec<ResultField>> {
    match columns {
        Some(columns) => {
            validate_named_columns(schema, columns)?;
            columns
                .iter()
                .map(|name| {
                    column_definition(schema, name, &schema.name)
                        .map(|definition| ResultField::new(name, definition.data_type))
                })
                .collect()
        }
        None => Ok(schema
            .columns
            .iter()
            .map(|definition| ResultField::new(&definition.name, definition.data_type))
            .collect()),
    }
}

/// Result fields for a single-table projection. Output names must be distinct because a result row
/// is a JSON object, but one source column may be returned under several names.
fn select_fields(
    schema: &TableDefinition,
    columns: Option<&[SelectColumn]>,
) -> Result<Vec<ResultField>> {
    let Some(columns) = columns else {
        return projection_fields(schema, None);
    };
    let mut outputs = HashSet::with_capacity(columns.len());
    columns
        .iter()
        .map(|item| {
            if !outputs.insert(item.output.as_str()) {
                return Err(EngineError::invalid_query(format!(
                    "SELECT produces output column `{}` more than once; use distinct AS aliases",
                    item.output
                )));
            }
            column_definition(schema, &item.column, &schema.name)
                .map(|definition| ResultField::new(&item.output, definition.data_type))
        })
        .collect()
}

/// An explicit projection resolved once per query rather than once per row: each item's output
/// name and the schema position of its source column.
struct Projection<'a> {
    items: Vec<(&'a str, usize)>,
}

impl<'a> Projection<'a> {
    fn new(
        columns: Option<&'a [SelectColumn]>,
        schema: &TableDefinition,
        table: &str,
    ) -> Result<Option<Self>> {
        let Some(columns) = columns else {
            return Ok(None);
        };
        let items = columns
            .iter()
            .map(|item| {
                schema
                    .columns
                    .iter()
                    .position(|definition| definition.name == item.column)
                    .map(|position| (item.output.as_str(), position))
                    .ok_or_else(|| EngineError::column_not_found(&item.column, table))
            })
            .collect::<Result<_>>()?;
        Ok(Some(Self { items }))
    }

    /// The estimated bytes of the projected row, computed before building it.
    fn estimated_bytes(&self, row: &RowRef<'_>) -> Result<usize> {
        let mut bytes = 32_usize;
        for (output, column) in &self.items {
            let value_bytes = row.get(*column)?.owned_bytes(owned_value_bytes)?;
            bytes = checked_result_add(bytes, 64)?;
            bytes = checked_result_add(bytes, checked_result_mul(output.len(), 2)?)?;
            bytes = checked_result_add(bytes, checked_result_mul(value_bytes, 2)?)?;
        }
        Ok(bytes)
    }

    fn project(&self, row: &RowRef<'_>) -> Result<Row> {
        let mut projected = Map::new();
        for (output, column) in &self.items {
            projected.insert((*output).to_owned(), row.get(*column)?.into_value());
        }
        Ok(projected)
    }

    fn project_owned(&self, row: &Row, schema: &TableDefinition) -> Result<Row> {
        self.project(&RowRef::map(row, schema))
    }
}

pub(crate) fn validate_named_columns(schema: &TableDefinition, columns: &[String]) -> Result<()> {
    let mut names = HashSet::with_capacity(columns.len());
    for column in columns {
        if !names.insert(column) {
            return Err(EngineError::invalid_query(format!(
                "Column `{column}` is named more than once"
            )));
        }
        column_definition(schema, column, &schema.name)?;
    }
    Ok(())
}

/// The order a query's rows must come in.
enum RowOrder {
    /// There is no `ORDER BY`.
    Any,
    /// `ORDER BY` names a prefix of the primary key's columns in ascending order, or all of them in
    /// descending order. Key columns are never `NULL`, so null placement cannot matter. Rows equal
    /// on a prefix then come in ascending key order, as a stable sort of rows read in key order
    /// would leave them.
    Key(KeyOrder),
    /// Rows must be collected and sorted.
    Sorted,
}

fn row_order(plan: &SelectPlan, schema: &TableDefinition) -> RowOrder {
    let Some(first) = plan.order_by.first() else {
        return RowOrder::Any;
    };
    let complete = match first.direction {
        OrderDirection::Asc => plan.order_by.len() <= schema.primary_key.len(),
        OrderDirection::Desc => plan.order_by.len() == schema.primary_key.len(),
    };
    if complete
        && plan
            .order_by
            .iter()
            .zip(&schema.primary_key)
            .all(|(order, column)| order.column == *column && order.direction == first.direction)
    {
        RowOrder::Key(match first.direction {
            OrderDirection::Asc => KeyOrder::Ascending,
            OrderDirection::Desc => KeyOrder::Descending,
        })
    } else {
        RowOrder::Sorted
    }
}

/// Reads the rows a query returns in the order candidates arrive, stopping at `LIMIT`. Without
/// `ORDER BY` any order will do, and with it [`row_order`] has confirmed that `order` is the one
/// asked for.
fn execute_unordered(
    storage: &dyn StorageReader,
    plan: &SelectPlan,
    schema: &crate::TableDefinition,
    order: KeyOrder,
) -> Result<Vec<Row>> {
    let mut scanned = 0_usize;
    let mut skipped_matches = 0_usize;
    let mut result_bytes = 0_usize;
    let mut rows = Vec::new();
    let filter = Filter::new(plan.predicate.as_ref(), schema, &plan.table)?;
    let projection = Projection::new(plan.columns.as_deref(), schema, &plan.table)?;
    visit_candidate_rows(storage, plan, schema, order, &mut |row| {
        count_scanned_row(&mut scanned)?;
        if !filter.matches(row)? {
            return Ok(VisitControl::Continue);
        }
        if skipped_matches < plan.offset {
            skipped_matches += 1;
            return Ok(VisitControl::Continue);
        }
        if rows.len() == MAX_RESULT_ROWS {
            return Err(result_limit_exceeded());
        }
        // An explicit projection is charged before it is built, from the columns it reads.
        let projected = match &projection {
            Some(projection) => {
                result_bytes = checked_result_add(result_bytes, projection.estimated_bytes(row)?)?;
                ensure_result_budget(result_bytes)?;
                projection.project(row)?
            }
            None => {
                let row = row.to_row()?;
                result_bytes = checked_result_add(result_bytes, owned_row_bytes(&row)?)?;
                ensure_result_budget(result_bytes)?;
                row
            }
        };
        rows.push(projected);
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
    plan: &SelectPlan,
    schema: &crate::TableDefinition,
) -> Result<Vec<Row>> {
    let mut scanned = 0_usize;
    let mut ordered_bytes = 0_usize;
    let mut rows = Vec::new();
    let filter = Filter::new(plan.predicate.as_ref(), schema, &plan.table)?;
    visit_candidate_rows(storage, plan, schema, KeyOrder::Ascending, &mut |row| {
        count_scanned_row(&mut scanned)?;
        if filter.matches(row)? {
            if rows.len() == MAX_ORDERED_ROWS {
                return Err(EngineError::new(
                    "QUERY_WORK_LIMIT_EXCEEDED",
                    format!(
                        "An ordered query cannot collect more than {MAX_ORDERED_ROWS} matching rows"
                    ),
                ));
            }
            let row = row.to_row()?;
            let next_ordered_bytes = checked_result_add(ordered_bytes, owned_row_bytes(&row)?)?;
            ensure_result_budget(next_ordered_bytes)?;
            rows.push(row);
            ordered_bytes = next_ordered_bytes;
        }
        Ok(VisitControl::Continue)
    })?;
    sort_rows(&mut rows, &plan.order_by, &plan.table)?;
    let take = plan.limit.unwrap_or(MAX_RESULT_ROWS + 1);
    let mut remaining_ordered_bytes = ordered_bytes;
    let mut result_bytes = 0_usize;
    let mut projected_rows = Vec::new();
    let projection = Projection::new(plan.columns.as_deref(), schema, &plan.table)?;
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
        let projected_bytes = match &projection {
            Some(projection) => projection.estimated_bytes(&RowRef::map(&row, schema))?,
            None => row_bytes,
        };
        let next_result_bytes = checked_result_add(result_bytes, projected_bytes)?;
        ensure_result_budget(next_result_bytes)?;
        ensure_result_budget(checked_result_add(
            remaining_ordered_bytes,
            next_result_bytes,
        )?)?;
        projected_rows.push(match &projection {
            Some(projection) => projection.project_owned(&row, schema)?,
            None => row,
        });
        result_bytes = next_result_bytes;
    }
    Ok(projected_rows)
}

fn visit_candidate_rows(
    storage: &dyn StorageReader,
    plan: &SelectPlan,
    schema: &crate::TableDefinition,
    order: KeyOrder,
    visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
) -> Result<VisitOutcome> {
    visit_predicate_candidates(
        storage,
        &plan.table,
        plan.predicate.as_ref(),
        schema,
        order,
        visitor,
    )
}

/// Narrows a scan to the rows a predicate can possibly match, shared by every statement family
/// that filters one table. Narrowing is only ever a candidate-selection step: each caller still
/// evaluates the full predicate per visited row, so an index that covers part of a predicate
/// cannot change which rows the caller accepts.
pub(crate) fn visit_predicate_candidates(
    storage: &dyn StorageReader,
    table: &str,
    predicate: Option<&Predicate>,
    schema: &crate::TableDefinition,
    order: KeyOrder,
    visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
) -> Result<VisitOutcome> {
    if let Some(key) = primary_key_lookup(predicate, schema) {
        return match storage.lookup_primary_key(table, &key)? {
            Some(row) if visitor(&RowRef::map(&row, schema))? == VisitControl::Stop => {
                Ok(VisitOutcome::Stopped)
            }
            _ => Ok(VisitOutcome::Complete),
        };
    }
    visit_indexed_candidates(storage, table, predicate, schema, order, visitor)
}

/// [`visit_predicate_candidates`] once no complete primary key applies. In ascending order it reads,
/// by preference: an index whose every column the predicate fixes, a range of an index's leading
/// column, a range of the leading primary-key column, and otherwise the whole table. Every one of
/// these returns rows in primary-key order, the equality index because its rows' indexed values
/// are equal. In descending order it reads only by primary key.
pub(crate) fn visit_indexed_candidates(
    storage: &dyn StorageReader,
    table: &str,
    predicate: Option<&Predicate>,
    schema: &crate::TableDefinition,
    order: KeyOrder,
    visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
) -> Result<VisitOutcome> {
    if order == KeyOrder::Ascending
        && let Some(outcome) = visit_secondary_index(storage, table, predicate, schema, visitor)?
    {
        return Ok(outcome);
    }
    let range = match predicate {
        Some(predicate) => column_range(
            predicate,
            column_definition(schema, &schema.primary_key[0], table)?,
        )?,
        None => ColumnRange::Unbounded,
    };
    match (range, order) {
        (ColumnRange::Unbounded, KeyOrder::Ascending) => storage.visit_table(table, visitor),
        (ColumnRange::Unbounded, KeyOrder::Descending) => {
            storage.visit_table_range(table, &KeyRange::default(), order, visitor)
        }
        (ColumnRange::Empty, _) => Ok(VisitOutcome::Complete),
        (ColumnRange::Bounded(range), _) => {
            storage.visit_table_range(table, &range, order, visitor)
        }
    }
}

/// Whether a predicate's candidates would be read through a secondary index, which returns them
/// only in ascending primary-key order.
pub(crate) fn secondary_index_applies(
    storage: &dyn StorageReader,
    table: &str,
    predicate: Option<&Predicate>,
    schema: &crate::TableDefinition,
) -> Result<bool> {
    if secondary_index_key(storage, table, predicate, schema)?.is_some() {
        return Ok(true);
    }
    let Some(predicate) = predicate else {
        return Ok(false);
    };
    for definition in ranged_indexes(storage, table, schema)? {
        let column = column_definition(schema, &definition.columns[0], table)?;
        if !matches!(column_range(predicate, column)?, ColumnRange::Unbounded) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Reads candidates through an index whose every column the predicate fixes, or through a range
/// of an index's leading column. `None` means no index serves the predicate.
fn visit_secondary_index(
    storage: &dyn StorageReader,
    table: &str,
    predicate: Option<&Predicate>,
    schema: &crate::TableDefinition,
    visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
) -> Result<Option<VisitOutcome>> {
    if let Some((columns, key)) = secondary_index_key(storage, table, predicate, schema)?
        && let Some(outcome) = storage.visit_index(table, &columns, &key, visitor)?
    {
        return Ok(Some(outcome));
    }
    let Some(predicate) = predicate else {
        return Ok(None);
    };
    // Each row an index range finds costs a lookup, so a range is worth reading only while it
    // covers a small part of the table. Beyond that, reading in key order is cheaper.
    let limit = storage.table_row_count(table)? / 4;
    for definition in ranged_indexes(storage, table, schema)? {
        match column_range(
            predicate,
            column_definition(schema, &definition.columns[0], table)?,
        )? {
            ColumnRange::Unbounded => {}
            ColumnRange::Empty => return Ok(Some(VisitOutcome::Complete)),
            ColumnRange::Bounded(range) => {
                if let Some(outcome) =
                    storage.visit_index_range(table, &definition.columns, &range, limit, visitor)?
                {
                    return Ok(Some(outcome));
                }
            }
        }
    }
    Ok(None)
}

/// The indexes whose leading column can be read as a range. A row is indexed only when every
/// indexed column is non-null, so a range of the leading column finds every row in it only if no
/// other indexed column can be null.
fn ranged_indexes(
    storage: &dyn StorageReader,
    table: &str,
    schema: &crate::TableDefinition,
) -> Result<Vec<crate::IndexDefinition>> {
    let mut indexes = storage.indexes_for_table(table)?;
    indexes.retain(|definition| {
        definition.columns[1..].iter().all(|name| {
            schema
                .columns
                .iter()
                .any(|column| column.name == *name && !column.nullable)
        })
    });
    Ok(indexes)
}

/// The key range of one column that a predicate's AND-ed terms allow.
enum ColumnRange {
    /// No term bounds the column.
    Unbounded,
    /// No row can satisfy the terms, such as an INTEGER column equal to 2.5.
    Empty,
    Bounded(KeyRange),
}

/// Bounds a column from the comparisons and prefix `LIKE` patterns among a predicate's top-level
/// AND-ed terms. Every bound is inclusive and so may admit a row that its term rejects, which the
/// caller's filter then does; exclusive integer bounds become the next integer.
fn column_range(predicate: &Predicate, column: &ColumnDefinition) -> Result<ColumnRange> {
    let mut range = KeyRange::default();
    if !narrow_range(predicate, column, &mut range)? {
        return Ok(ColumnRange::Empty);
    }
    Ok(match (&range.lower, &range.upper) {
        (None, None) => ColumnRange::Unbounded,
        (Some(lower), Some(upper)) if lower > upper => ColumnRange::Empty,
        _ => ColumnRange::Bounded(range),
    })
}

/// Tightens `range` with each AND-ed term that bounds `column`, returning `false` once a term can
/// match no row.
fn narrow_range(
    predicate: &Predicate,
    column: &ColumnDefinition,
    range: &mut KeyRange,
) -> Result<bool> {
    let (lower, upper) = match predicate {
        Predicate::And { predicates } => {
            for predicate in predicates {
                if !narrow_range(predicate, column, range)? {
                    return Ok(false);
                }
            }
            return Ok(true);
        }
        Predicate::Comparison {
            column: name,
            operator,
            value,
        } if *name == column.name => {
            if value.is_null() {
                // A comparison with NULL is never true, so neither is the conjunction.
                return Ok(false);
            }
            match comparison_bounds(column.data_type, *operator, value)? {
                Some(Some(bounds)) => bounds,
                Some(None) => return Ok(false),
                None => return Ok(true),
            }
        }
        Predicate::Like {
            column: name,
            pattern: Value::String(pattern),
            escape,
            case_insensitive: false,
        } if *name == column.name && column.data_type == ColumnType::Text => {
            let escape = match escape {
                None => Some('\\'),
                Some(Value::String(escape)) => escape.chars().next(),
                Some(_) => return Ok(true),
            };
            let prefix = LikePattern::new(pattern, escape, false).literal_prefix();
            if prefix.is_empty() {
                return Ok(true);
            }
            let (lower, upper) = encode_text_prefix_bounds(&prefix)?;
            (Some(lower), Some(upper))
        }
        _ => return Ok(true),
    };
    // A cursor cannot start at a key longer than any stored key, and neither bound is needed.
    if let Some(lower) = lower.filter(|lower| lower.len() <= crate::MAX_BTREE_KEY_BYTES)
        && range.lower.as_ref().is_none_or(|current| lower > *current)
    {
        range.lower = Some(lower);
    }
    if let Some(upper) = upper
        && range.upper.as_ref().is_none_or(|current| upper < *current)
    {
        range.upper = Some(upper);
    }
    Ok(true)
}

type KeyBounds = (Option<Vec<u8>>, Option<Vec<u8>>);

/// The encoded bounds one comparison places on a column: `None` if it places none, and
/// `Some(None)` if it can match no value of the column's type.
fn comparison_bounds(
    data_type: ColumnType,
    operator: ComparisonOperator,
    value: &Value,
) -> Result<Option<Option<KeyBounds>>> {
    if operator == ComparisonOperator::Neq {
        return Ok(None);
    }
    let bound = |value: Value| encode_key_bound(data_type, &value);
    let bounds = match data_type {
        ColumnType::Integer => {
            // Integers compare with numbers as f64 values do, and every stored integer is exact
            // in f64, so each bound is the nearest integer on its side of the value.
            let Some(value) = value.as_f64() else {
                return Ok(None);
            };
            let (low, high) = match operator {
                ComparisonOperator::Eq => (value.ceil(), value.floor()),
                ComparisonOperator::Gt => (value.floor() + 1.0, f64::INFINITY),
                ComparisonOperator::Gte => (value.ceil(), f64::INFINITY),
                ComparisonOperator::Lt => (f64::NEG_INFINITY, value.ceil() - 1.0),
                ComparisonOperator::Lte => (f64::NEG_INFINITY, value.floor()),
                ComparisonOperator::Neq => unreachable!("handled above"),
            };
            const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
            if low > high || low > MAX_SAFE_INTEGER || high < -MAX_SAFE_INTEGER {
                return Ok(Some(None));
            }
            (
                (low > -MAX_SAFE_INTEGER)
                    .then(|| bound(Value::from(low as i64)))
                    .transpose()?,
                (high < MAX_SAFE_INTEGER)
                    .then(|| bound(Value::from(high as i64)))
                    .transpose()?,
            )
        }
        ColumnType::Float | ColumnType::Text | ColumnType::Boolean => {
            let comparable = match data_type {
                ColumnType::Float => value.as_f64().map(Value::from),
                ColumnType::Text => value.is_string().then(|| value.clone()),
                _ => value.is_boolean().then(|| value.clone()),
            };
            let Some(value) = comparable else {
                return Ok(None);
            };
            let value = bound(value)?;
            match operator {
                ComparisonOperator::Eq => (Some(value.clone()), Some(value)),
                ComparisonOperator::Gt | ComparisonOperator::Gte => (Some(value), None),
                ComparisonOperator::Lt | ComparisonOperator::Lte => (None, Some(value)),
                ComparisonOperator::Neq => unreachable!("handled above"),
            }
        }
        ColumnType::Json => return Ok(None),
    };
    Ok(Some(Some(bounds)))
}

fn secondary_index_key(
    storage: &dyn StorageReader,
    table: &str,
    predicate: Option<&Predicate>,
    schema: &crate::TableDefinition,
) -> Result<Option<(Vec<String>, Row)>> {
    let mut equalities = Map::new();
    collect_guaranteed_equalities(predicate, &mut equalities);
    for definition in storage.indexes_for_table(table)? {
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

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ParseMode {
    Bound,
    Template,
}

#[cfg(test)]
pub(crate) fn parse_sql(sql: &str, params: &[Value]) -> Result<SelectPlan> {
    crate::query::validate_sql_input(sql, params)?;
    let tokens = crate::query::tokenize(sql)?;
    crate::query::validate_parameter_expansion(&tokens, params)?;
    parse_tokens(tokens, params, ParseMode::Bound)
}

pub(crate) fn parse_tokens(
    tokens: Vec<Token>,
    params: &[Value],
    mode: ParseMode,
) -> Result<SelectPlan> {
    SqlParser::new(tokens, params, mode).parse()
}

pub(crate) fn validate_sql_input(sql: &str, params: &[Value]) -> Result<()> {
    if sql.len() > MAX_SQL_BYTES {
        return Err(EngineError::invalid_query(format!(
            "SQL text exceeds the {MAX_SQL_BYTES}-byte limit"
        )));
    }
    validate_sql_parameters(params)
}

pub(crate) fn validate_sql_parameters(params: &[Value]) -> Result<()> {
    if params.len() > MAX_SQL_PARAMETERS {
        return Err(EngineError::invalid_query(format!(
            "A SQL query cannot receive more than {MAX_SQL_PARAMETERS} parameters"
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

/// Bounds retained parameter copies before parsing can clone any bound value into its AST.
/// The lexer has already excluded quoted strings, identifiers, and comments from placeholders.
pub(crate) fn validate_parameter_expansion(tokens: &[Token], params: &[Value]) -> Result<()> {
    let mut occurrences = vec![0usize; params.len()];
    for token in tokens {
        if let Token::Placeholder(raw_index) = token {
            let index = parameter_index(raw_index)?;
            let count = occurrences.get_mut(index - 1).ok_or_else(|| {
                EngineError::bind_error(format!("No value was provided for `${raw_index}`"))
            })?;
            *count = count.checked_add(1).ok_or_else(binding_limit_exceeded)?;
        }
    }
    validate_bound_parameter_bytes(&occurrences, params)
}

/// The occurrence vector is retained by prepared statements and recomputed for ordinary queries.
/// Each supplied value is measured once, even when its parameter appears many times in the SQL.
pub(crate) fn validate_bound_parameter_bytes(
    occurrences: &[usize],
    params: &[Value],
) -> Result<()> {
    debug_assert_eq!(occurrences.len(), params.len());
    let mut bytes = 0usize;
    for (count, value) in occurrences.iter().zip(params) {
        if *count == 0 {
            continue;
        }
        let retained = estimated_value_bytes(value)
            .map_err(|error| EngineError::bind_error(error.message))?
            .checked_add(std::mem::size_of::<Value>())
            .ok_or_else(binding_limit_exceeded)?;
        bytes = retained
            .checked_mul(*count)
            .and_then(|retained| bytes.checked_add(retained))
            .ok_or_else(binding_limit_exceeded)?;
        if bytes > MAX_BOUND_PARAMETER_BYTES {
            return Err(binding_limit_exceeded());
        }
    }
    Ok(())
}

fn binding_limit_exceeded() -> EngineError {
    EngineError::new(
        "RESOURCE_LIMIT",
        format!(
            "Expanded SQL parameters cannot retain more than {MAX_BOUND_PARAMETER_BYTES} bytes"
        ),
    )
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
    mode: ParseMode,
}

impl<'a> SqlParser<'a> {
    fn new(tokens: Vec<Token>, params: &'a [Value], mode: ParseMode) -> Self {
        Self {
            tokens,
            position: 0,
            params,
            mode,
        }
    }

    fn parse(mut self) -> Result<SelectPlan> {
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
        let mut order_by = if self.consume_keyword("order") {
            self.expect_keyword("by")?;
            self.parse_order_by()?
        } else {
            Vec::new()
        };
        // As in PostgreSQL, an ORDER BY name refers to an output column before a source column,
        // so an alias can be ordered by and can shadow the column it renames.
        for order in &mut order_by {
            if let Some(item) = columns
                .iter()
                .flatten()
                .find(|item| item.output == order.column)
            {
                order.column.clone_from(&item.column);
            }
        }
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

        Ok(SelectPlan {
            table,
            columns,
            predicate,
            order_by,
            limit,
            offset,
        })
    }

    fn parse_projection(&mut self) -> Result<Option<Vec<SelectColumn>>> {
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
            let column = self.parse_identifier()?;
            let output = if self.consume_keyword("as") {
                self.parse_identifier()?
            } else {
                column.clone()
            };
            columns.push(SelectColumn { column, output });
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
        pagination_value(&value, self.mode)
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

        let negated = self.consume_keyword("not");
        if self.consume_keyword("in") {
            let predicate = self.parse_in(column)?;
            return if negated {
                self.node(Predicate::Not {
                    predicate: Box::new(predicate),
                })
            } else {
                Ok(predicate)
            };
        }
        if self.consume_keyword("between") {
            return self.parse_between(column, negated);
        }
        let case_insensitive = if self.consume_keyword("like") {
            Some(false)
        } else if self.consume_keyword("ilike") {
            Some(true)
        } else {
            None
        };
        if let Some(case_insensitive) = case_insensitive {
            let pattern = self.parse_value()?;
            let escape = if self.consume_keyword("escape") {
                Some(self.parse_value()?)
            } else {
                None
            };
            let predicate = self.node(Predicate::Like {
                column,
                pattern,
                escape,
                case_insensitive,
            })?;
            return if negated {
                self.node(Predicate::Not {
                    predicate: Box::new(predicate),
                })
            } else {
                Ok(predicate)
            };
        }
        if negated {
            return Err(unsupported_shape());
        }

        let operator = match self.next() {
            Some(Token::Eq) => ComparisonOperator::Eq,
            Some(Token::Neq) => ComparisonOperator::Neq,
            Some(Token::Lt) => ComparisonOperator::Lt,
            Some(Token::Lte) => ComparisonOperator::Lte,
            Some(Token::Gt) => ComparisonOperator::Gt,
            Some(Token::Gte) => ComparisonOperator::Gte,
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

    /// `BETWEEN` is exactly its two inclusive comparisons, so it expands into them rather than
    /// adding a predicate the evaluator, validators, binders, and index planner would all need to
    /// learn. `NOT BETWEEN` is the De Morgan form, which preserves SQL unknown propagation.
    fn parse_between(&mut self, column: String, negated: bool) -> Result<Predicate> {
        if self.consume_keyword("symmetric") || self.consume_keyword("asymmetric") {
            return Err(EngineError::unsupported_sql(
                "BETWEEN SYMMETRIC and BETWEEN ASYMMETRIC are not supported",
            ));
        }
        let low = self.parse_value()?;
        if !self.consume_keyword("and") {
            return Err(EngineError::parse_error(
                "Expected AND between the BETWEEN bounds",
            ));
        }
        let high = self.parse_value()?;
        let (lower, upper) = if negated {
            (ComparisonOperator::Lt, ComparisonOperator::Gt)
        } else {
            (ComparisonOperator::Gte, ComparisonOperator::Lte)
        };
        let predicates = vec![
            self.node(Predicate::Comparison {
                column: column.clone(),
                operator: lower,
                value: low,
            })?,
            self.node(Predicate::Comparison {
                column,
                operator: upper,
                value: high,
            })?,
        ];
        self.node(if negated {
            Predicate::Or { predicates }
        } else {
            Predicate::And { predicates }
        })
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
        if self.nodes > MAX_PREDICATE_NODES {
            return Err(EngineError::invalid_query(format!(
                "A query cannot contain more than {MAX_PREDICATE_NODES} predicate nodes"
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

/// Whether the token at `position`, directly after `SELECT`, is the `DISTINCT` keyword.
///
/// `DISTINCT` is not reserved, so a column may be named `distinct`. The word is a keyword only when
/// something other than the end of a projection item or aggregate argument follows it.
pub(crate) fn is_distinct_keyword_at(tokens: &[Token], position: usize) -> bool {
    let is_keyword = |token: Option<&Token>, keyword: &str| {
        matches!(
            token,
            Some(Token::Identifier {
                value,
                quoted: false,
            }) if value.eq_ignore_ascii_case(keyword)
        )
    };
    is_keyword(tokens.get(position), "distinct")
        && tokens.get(position + 1).is_some_and(|next| {
            !matches!(next, Token::Comma | Token::RParen)
                && !is_keyword(Some(next), "from")
                && !is_keyword(Some(next), "as")
        })
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
    let index = parameter_index(index)?;
    params.get(index - 1).cloned().ok_or_else(|| {
        EngineError::bind_error(format!("No value was provided for `{placeholder}`"))
    })
}

pub(crate) fn parameter_index(index: &str) -> Result<usize> {
    let placeholder = format!("${index}");
    let index = index.parse::<usize>().map_err(|_| {
        EngineError::bind_error(format!("Invalid parameter placeholder `{placeholder}`"))
    })?;
    if index == 0 {
        return Err(EngineError::bind_error(
            "PostgreSQL parameter indexes start at $1",
        ));
    }
    Ok(index)
}

const PREPARED_PARAMETER_KEY: &str = "\0tinyjoin:parameter";

pub(crate) fn prepared_parameter_marker(index: usize) -> Value {
    Value::Object(Map::from_iter([(
        PREPARED_PARAMETER_KEY.to_owned(),
        Value::Number((index as u64).into()),
    )]))
}

pub(crate) fn prepared_parameter_index(value: &Value) -> Option<usize> {
    let Value::Object(object) = value else {
        return None;
    };
    if object.len() != 1 {
        return None;
    }
    object
        .get(PREPARED_PARAMETER_KEY)?
        .as_u64()
        .and_then(|index| usize::try_from(index).ok())
        .filter(|index| *index != 0)
}

pub(crate) fn bind_select_plan_parameters(
    plan: &SelectPlan,
    params: &[Value],
    limit_parameter: Option<usize>,
    offset_parameter: Option<usize>,
) -> Result<SelectPlan> {
    let mut plan = plan.clone();
    bind_predicate_parameters(plan.predicate.as_mut(), params)?;
    if let Some(index) = limit_parameter {
        plan.limit = Some(bind_nonnegative_integer_parameter(index, params)?);
    }
    if let Some(index) = offset_parameter {
        plan.offset = bind_nonnegative_integer_parameter(index, params)?;
    }
    Ok(plan)
}

pub(crate) fn bind_predicate_parameters(
    predicate: Option<&mut Predicate>,
    params: &[Value],
) -> Result<()> {
    let Some(predicate) = predicate else {
        return Ok(());
    };
    match predicate {
        Predicate::Comparison { value, .. } => bind_prepared_value(value, params),
        Predicate::In { values, .. } => {
            for value in values {
                bind_prepared_value(value, params)?;
            }
            Ok(())
        }
        Predicate::And { predicates } | Predicate::Or { predicates } => {
            for predicate in predicates {
                bind_predicate_parameters(Some(predicate), params)?;
            }
            Ok(())
        }
        Predicate::Like {
            pattern, escape, ..
        } => {
            bind_prepared_value(pattern, params)?;
            escape
                .as_mut()
                .map_or(Ok(()), |escape| bind_prepared_value(escape, params))
        }
        Predicate::Not { predicate } => bind_predicate_parameters(Some(predicate), params),
        Predicate::IsNull { .. } => Ok(()),
    }
}

pub(crate) fn bind_prepared_value(value: &mut Value, params: &[Value]) -> Result<()> {
    let Some(index) = prepared_parameter_index(value) else {
        return Ok(());
    };
    *value = params.get(index - 1).cloned().ok_or_else(|| {
        EngineError::new(
            "INTERNAL_ERROR",
            "Prepared statement parameter metadata is inconsistent",
        )
    })?;
    Ok(())
}

pub(crate) fn bind_nonnegative_integer_parameter(index: usize, params: &[Value]) -> Result<usize> {
    let value = params.get(index - 1).ok_or_else(|| {
        EngineError::invalid_query("LIMIT and OFFSET must be non-negative integers")
    })?;
    pagination_value(value, ParseMode::Bound)
}

pub(crate) fn pagination_value(value: &Value, mode: ParseMode) -> Result<usize> {
    // Only the internal prepare path supplies template markers. A caller's JSON
    // object with the same shape is still data and cannot stand in for an integer.
    if mode == ParseMode::Template && prepared_parameter_index(value).is_some() {
        return Ok(0);
    }
    value
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| EngineError::invalid_query("LIMIT and OFFSET must be non-negative integers"))
}

/// Evaluates a predicate over a row held as a map, directly from its SQL form. Executors evaluate
/// a [`Filter`] instead, and this reference evaluator is kept to check that they agree.
#[cfg(test)]
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

#[cfg(test)]
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
                match evaluate_comparison(actual, value, ComparisonOperator::Eq, table, column)? {
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
        Predicate::Like {
            column,
            pattern,
            escape,
            case_insensitive,
        } => {
            let text = row
                .get(column)
                .ok_or_else(|| EngineError::column_not_found(column, table))?;
            let escape = match escape {
                None => Some(Some('\\')),
                Some(Value::String(escape)) => Some(escape.chars().next()),
                Some(_) => None,
            };
            Ok(match (text, pattern, escape) {
                (Value::String(text), Value::String(pattern), Some(escape)) => {
                    if like_matches(text, pattern, escape, *case_insensitive) {
                        Truth::True
                    } else {
                        Truth::False
                    }
                }
                // Validation admits only text and NULL operands, so anything else is NULL.
                _ => Truth::Unknown,
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
    operator: ComparisonOperator,
    table: &str,
    column: &str,
) -> Result<Truth> {
    if left == &Value::Null || right == &Value::Null {
        return Ok(Truth::Unknown);
    }
    if matches!(operator, ComparisonOperator::Eq | ComparisonOperator::Neq) {
        let equal = values_equal(left, right);
        let matched = if operator == ComparisonOperator::Eq {
            equal
        } else {
            !equal
        };
        return Ok(if matched { Truth::True } else { Truth::False });
    }
    let ordering = compare_values(left, right, table, column)?;
    let matched = match operator {
        ComparisonOperator::Eq | ComparisonOperator::Neq => unreachable!("handled above"),
        ComparisonOperator::Lt => ordering == Ordering::Less,
        ComparisonOperator::Lte => ordering != Ordering::Greater,
        ComparisonOperator::Gt => ordering == Ordering::Greater,
        ComparisonOperator::Gte => ordering != Ordering::Less,
    };
    Ok(if matched { Truth::True } else { Truth::False })
}

fn values_equal(left: &Value, right: &Value) -> bool {
    // Every executor validates predicates against its catalog before evaluating rows. Different
    // non-null runtime kinds can therefore meet here only in a JSON comparison, where they are
    // unequal rather than incompatible. Numeric scalar comparisons retain integer/float equality.
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
        _ => left == right,
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

/// A `WHERE` predicate resolved to column positions, for evaluating against rows as storage
/// presents them.
///
/// Column names are resolved and `LIKE` patterns compiled once per statement, and each row decodes
/// only the columns the predicate reads, without copying text. Tests check that it agrees with the
/// reference evaluator, `matches_predicate`, on every row.
pub(crate) struct Filter<'a> {
    node: Option<FilterNode<'a>>,
    table: &'a str,
}

enum FilterNode<'a> {
    Comparison {
        column: usize,
        name: &'a str,
        operator: ComparisonOperator,
        value: &'a Value,
    },
    IsNull {
        column: usize,
        negated: bool,
    },
    In {
        column: usize,
        name: &'a str,
        values: &'a [Value],
    },
    /// `pattern` is `None` when the pattern or its escape is `NULL`, which leaves every row's
    /// result unknown.
    Like {
        column: usize,
        pattern: Option<LikePattern>,
    },
    And(Vec<FilterNode<'a>>),
    Or(Vec<FilterNode<'a>>),
    Not(Box<FilterNode<'a>>),
}

impl<'a> Filter<'a> {
    /// Resolves a predicate that has already been validated against `schema`.
    pub(crate) fn new(
        predicate: Option<&'a Predicate>,
        schema: &TableDefinition,
        table: &'a str,
    ) -> Result<Self> {
        Self::resolved(predicate, table, &|column| {
            schema
                .columns
                .iter()
                .position(|definition| definition.name == column)
                .ok_or_else(|| EngineError::column_not_found(column, table))
        })
    }

    /// Resolves a validated predicate whose columns `position` places. `table` names the rows in
    /// errors.
    pub(crate) fn resolved(
        predicate: Option<&'a Predicate>,
        table: &'a str,
        position: &dyn Fn(&str) -> Result<usize>,
    ) -> Result<Self> {
        Ok(Self {
            node: predicate
                .map(|predicate| FilterNode::new(predicate, position))
                .transpose()?,
            table,
        })
    }

    pub(crate) fn matches(&self, row: &dyn Columns) -> Result<bool> {
        match &self.node {
            None => Ok(true),
            Some(node) => Ok(node.evaluate(row, self.table)? == Truth::True),
        }
    }
}

impl<'a> FilterNode<'a> {
    fn new(predicate: &'a Predicate, position: &dyn Fn(&str) -> Result<usize>) -> Result<Self> {
        let children = |predicates: &'a [Predicate]| {
            predicates
                .iter()
                .map(|predicate| Self::new(predicate, position))
                .collect::<Result<Vec<_>>>()
        };
        Ok(match predicate {
            Predicate::Comparison {
                column,
                operator,
                value,
            } => Self::Comparison {
                column: position(column)?,
                name: column,
                operator: *operator,
                value,
            },
            Predicate::IsNull { column, negated } => Self::IsNull {
                column: position(column)?,
                negated: *negated,
            },
            Predicate::In { column, values } => Self::In {
                column: position(column)?,
                name: column,
                values,
            },
            Predicate::Like {
                column,
                pattern,
                escape,
                case_insensitive,
            } => {
                let escape = match escape {
                    None => Some(Some('\\')),
                    Some(Value::String(escape)) => Some(escape.chars().next()),
                    Some(_) => None,
                };
                Self::Like {
                    column: position(column)?,
                    pattern: match (pattern, escape) {
                        (Value::String(pattern), Some(escape)) => {
                            Some(LikePattern::new(pattern, escape, *case_insensitive))
                        }
                        _ => None,
                    },
                }
            }
            Predicate::And { predicates } => Self::And(children(predicates)?),
            Predicate::Or { predicates } => Self::Or(children(predicates)?),
            Predicate::Not { predicate } => Self::Not(Box::new(Self::new(predicate, position)?)),
        })
    }

    fn evaluate(&self, row: &dyn Columns, table: &str) -> Result<Truth> {
        match self {
            Self::Comparison {
                column,
                name,
                operator,
                value,
            } => compare_to_value(&row.column(*column)?, value, *operator, table, name),
            Self::IsNull { column, negated } => Ok(if row.column(*column)?.is_null() ^ negated {
                Truth::True
            } else {
                Truth::False
            }),
            Self::In {
                column,
                name,
                values,
            } => {
                let actual = row.column(*column)?;
                let mut unknown = false;
                for value in *values {
                    match compare_to_value(&actual, value, ComparisonOperator::Eq, table, name)? {
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
            Self::Like { column, pattern } => {
                let Some(pattern) = pattern else {
                    return Ok(Truth::Unknown);
                };
                let matched = match row.column(*column)? {
                    ValueRef::Text(text) => pattern.matches(&text),
                    ValueRef::Json(value) => match value.as_ref() {
                        Value::String(text) => pattern.matches(text),
                        // Validation admits only text and NULL operands, so anything else is NULL.
                        _ => return Ok(Truth::Unknown),
                    },
                    _ => return Ok(Truth::Unknown),
                };
                Ok(if matched { Truth::True } else { Truth::False })
            }
            Self::And(nodes) => {
                let mut unknown = false;
                for node in nodes {
                    match node.evaluate(row, table)? {
                        Truth::False => return Ok(Truth::False),
                        Truth::Unknown => unknown = true,
                        Truth::True => {}
                    }
                }
                Ok(if unknown { Truth::Unknown } else { Truth::True })
            }
            Self::Or(nodes) => {
                let mut unknown = false;
                for node in nodes {
                    match node.evaluate(row, table)? {
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
            Self::Not(node) => Ok(match node.evaluate(row, table)? {
                Truth::True => Truth::False,
                Truth::False => Truth::True,
                Truth::Unknown => Truth::Unknown,
            }),
        }
    }
}

/// [`evaluate_comparison`] for a column value read from a row. Numbers compare as `f64`, as they
/// do there, and values of different kinds are unequal but cannot be ordered.
fn compare_to_value(
    left: &ValueRef<'_>,
    right: &Value,
    operator: ComparisonOperator,
    table: &str,
    column: &str,
) -> Result<Truth> {
    let ordering = match (left, right) {
        (ValueRef::Json(left), right) => {
            return evaluate_comparison(left, right, operator, table, column);
        }
        (ValueRef::Null, _) | (_, Value::Null) => return Ok(Truth::Unknown),
        (ValueRef::Integer(left), Value::Number(right)) => right
            .as_f64()
            .and_then(|right| (*left as f64).partial_cmp(&right)),
        (ValueRef::Float(left), Value::Number(right)) => {
            right.as_f64().and_then(|right| left.partial_cmp(&right))
        }
        (ValueRef::Text(left), Value::String(right)) => Some(left.as_ref().cmp(right.as_str())),
        (ValueRef::Boolean(left), Value::Bool(right)) => Some(left.cmp(right)),
        _ => None,
    };
    let matched = match operator {
        ComparisonOperator::Eq => ordering == Some(Ordering::Equal),
        ComparisonOperator::Neq => ordering != Some(Ordering::Equal),
        operator => {
            let ordering = ordering.ok_or_else(|| comparison_error(table, column))?;
            match operator {
                ComparisonOperator::Lt => ordering == Ordering::Less,
                ComparisonOperator::Lte => ordering != Ordering::Greater,
                ComparisonOperator::Gt => ordering == Ordering::Greater,
                ComparisonOperator::Gte => ordering != Ordering::Less,
                ComparisonOperator::Eq | ComparisonOperator::Neq => unreachable!("handled above"),
            }
        }
    };
    Ok(if matched { Truth::True } else { Truth::False })
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
        if *nodes > MAX_PREDICATE_NODES {
            return Err(EngineError::invalid_query(format!(
                "A query cannot contain more than {MAX_PREDICATE_NODES} predicate nodes"
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
    schema: &crate::TableDefinition,
    table: &str,
) -> Result<()> {
    match predicate {
        Predicate::Comparison { column, .. }
        | Predicate::IsNull { column, .. }
        | Predicate::In { column, .. }
        | Predicate::Like { column, .. } => {
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
    schema: &'a crate::TableDefinition,
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
    schema: &crate::TableDefinition,
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
                validate_comparison_value(definition, ComparisonOperator::Eq, value, table)?;
            }
            Ok(())
        }
        Predicate::Like {
            column,
            pattern,
            escape,
            case_insensitive,
        } => validate_like(
            column_definition(schema, column, table)?,
            pattern,
            escape.as_ref(),
            *case_insensitive,
            table,
        ),
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

/// Checks a `LIKE` or `ILIKE` before any row is read, so a malformed pattern fails even when no
/// row would reach it.
pub(crate) fn validate_like(
    definition: &ColumnDefinition,
    pattern: &Value,
    escape: Option<&Value>,
    case_insensitive: bool,
    table: &str,
) -> Result<()> {
    let operator = if case_insensitive { "ILIKE" } else { "LIKE" };
    if definition.data_type != ColumnType::Text {
        return Err(EngineError::type_mismatch(format!(
            "{operator} requires a text column, but `{}` in `{table}` is not text",
            definition.name
        )));
    }
    if !pattern.is_string() && !pattern.is_null() {
        return Err(EngineError::type_mismatch(format!(
            "{operator} requires a text pattern"
        )));
    }
    let escape = match escape {
        None => Some('\\'),
        Some(Value::Null) => return Ok(()),
        Some(Value::String(escape)) => {
            let mut characters = escape.chars();
            let escape = characters.next();
            if characters.next().is_some() {
                return Err(EngineError::invalid_query(format!(
                    "{operator} ESCAPE must be empty or a single character"
                )));
            }
            escape
        }
        Some(_) => {
            return Err(EngineError::type_mismatch(format!(
                "{operator} ESCAPE requires a text value"
            )));
        }
    };
    if let (Value::String(pattern), Some(escape)) = (pattern, escape) {
        let mut characters = pattern.chars();
        while let Some(character) = characters.next() {
            if character == escape && characters.next().is_none() {
                return Err(EngineError::invalid_query(format!(
                    "{operator} pattern must not end with its escape character"
                )));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum LikeElement {
    AnyCharacter,
    Character(char),
}

/// Matches a validated `LIKE` pattern against all of `text`.
#[cfg(test)]
pub(crate) fn like_matches(
    text: &str,
    pattern: &str,
    escape: Option<char>,
    case_insensitive: bool,
) -> bool {
    LikePattern::new(pattern, escape, case_insensitive).matches(text)
}

/// A validated `LIKE` pattern, split at its unescaped `%` wildcards once so that it can match any
/// number of texts.
///
/// `ILIKE` folds only ASCII letters, as PostgreSQL does under the C locale. That matches the
/// code-point collation TinyJoin uses everywhere else and avoids shipping Unicode case tables.
///
/// Between unescaped `%` wildcards, each segment matches a fixed number of characters, so taking
/// the leftmost match of every middle segment always leaves the most room for the rest. That keeps
/// the work proportional to the text length times the longest segment, with no backtracking.
struct LikePattern {
    segments: Vec<Vec<LikeElement>>,
    /// Each segment's text when it has no `_`, which is matched as bytes. Folding ASCII letters byte
    /// by byte is exactly `ILIKE`'s folding, and UTF-8 bytes equal to a whole string of characters
    /// can only begin at a character boundary.
    literals: Vec<Option<String>>,
    case_insensitive: bool,
}

impl LikePattern {
    fn new(pattern: &str, escape: Option<char>, case_insensitive: bool) -> Self {
        let mut segments = vec![Vec::new()];
        let mut characters = pattern.chars();
        while let Some(character) = characters.next() {
            let segment = segments.last_mut().expect("there is always a segment");
            if Some(character) == escape {
                if let Some(escaped) = characters.next() {
                    segment.push(LikeElement::Character(escaped));
                }
            } else if character == '%' {
                segments.push(Vec::new());
            } else if character == '_' {
                segment.push(LikeElement::AnyCharacter);
            } else {
                segment.push(LikeElement::Character(character));
            }
        }
        let literals = segments
            .iter()
            .map(|segment| {
                segment
                    .iter()
                    .map(|element| match element {
                        LikeElement::Character(character) => Some(*character),
                        LikeElement::AnyCharacter => None,
                    })
                    .collect()
            })
            .collect();
        Self {
            segments,
            literals,
            case_insensitive,
        }
    }

    /// The characters every match starts with: those before the first wildcard.
    fn literal_prefix(&self) -> String {
        self.segments[0]
            .iter()
            .map_while(|element| match element {
                LikeElement::Character(character) => Some(*character),
                LikeElement::AnyCharacter => None,
            })
            .collect()
    }

    fn matches(&self, text: &str) -> bool {
        let case_insensitive = self.case_insensitive;
        let same = |bytes: &[u8], literal: &str| {
            if case_insensitive {
                bytes.eq_ignore_ascii_case(literal.as_bytes())
            } else {
                bytes == literal.as_bytes()
            }
        };
        // Where segment `index`, starting at byte `start`, ends.
        let matches_at = |start: usize, index: usize| -> Option<usize> {
            if let Some(literal) = &self.literals[index] {
                let end = start + literal.len();
                return text
                    .as_bytes()
                    .get(start..end)
                    .is_some_and(|bytes| same(bytes, literal))
                    .then_some(end);
            }
            let mut end = start;
            let mut remaining = text[start..].chars();
            for element in &self.segments[index] {
                let character = remaining.next()?;
                if let LikeElement::Character(expected) = element
                    && *expected != character
                    && !(case_insensitive && expected.eq_ignore_ascii_case(&character))
                {
                    return None;
                }
                end += character.len_utf8();
            }
            Some(end)
        };

        let last = self.segments.len() - 1;
        if last == 0 {
            return matches_at(0, 0) == Some(text.len());
        }
        let Some(mut cursor) = matches_at(0, 0) else {
            return false;
        };
        for index in (1..last).filter(|index| !self.segments[*index].is_empty()) {
            let found = match &self.literals[index] {
                Some(literal) if !case_insensitive => text[cursor..]
                    .find(literal.as_str())
                    .map(|offset| cursor + offset + literal.len()),
                Some(literal) => text.as_bytes()[cursor..]
                    .windows(literal.len())
                    .position(|bytes| same(bytes, literal))
                    .map(|offset| cursor + offset + literal.len()),
                None => text[cursor..]
                    .char_indices()
                    .map(|(offset, _)| cursor + offset)
                    .find_map(|start| matches_at(start, index)),
            };
            let Some(end) = found else {
                return false;
            };
            cursor = end;
        }
        // The last segment is anchored to the end of the text, a fixed number of characters back.
        let start = match &self.literals[last] {
            Some(literal) => match text.len().checked_sub(literal.len()) {
                Some(start) => start,
                None => return false,
            },
            None => match text.char_indices().rev().nth(self.segments[last].len() - 1) {
                Some((start, _)) => start,
                None => return false,
            },
        };
        start >= cursor && matches_at(start, last) == Some(text.len())
    }
}

fn validate_comparison_value(
    definition: &ColumnDefinition,
    operator: ComparisonOperator,
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
        ColumnType::Json => matches!(operator, ComparisonOperator::Eq | ComparisonOperator::Neq),
    };
    if compatible {
        Ok(())
    } else {
        Err(comparison_error(table, &definition.name))
    }
}

pub(crate) fn primary_key_lookup(
    predicate: Option<&Predicate>,
    schema: &crate::TableDefinition,
) -> Option<Row> {
    let mut equalities = Map::new();
    collect_guaranteed_equalities(predicate, &mut equalities);
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
/// and evaluating the predicate. Floats have multiple JSON encodings that
/// compare numerically equal (`1`/`1.0`), so they cannot use this shortcut.
fn exact_primary_key_value(schema: &crate::TableDefinition, column: &str, value: &Value) -> bool {
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
            operator: ComparisonOperator::Eq,
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
    use crate::{ColumnDefinition, Engine, InMemoryStorage, IndexDefinition, TableDefinition};

    fn row(value: Value) -> Row {
        value
            .as_object()
            .expect("test row must be an object")
            .clone()
    }

    fn select_column(column: &str) -> SelectColumn {
        SelectColumn {
            column: column.to_owned(),
            output: column.to_owned(),
        }
    }

    fn engine() -> Engine<InMemoryStorage> {
        let mut engine = Engine::default();
        engine
            .execute_sql(
                "CREATE TABLE posts (\
                    id INTEGER PRIMARY KEY, \
                    title TEXT NOT NULL, \
                    user_id INTEGER, \
                    deleted BOOLEAN\
                )",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO posts (id, title, user_id, deleted) VALUES \
                    (1, 'one', 7, NULL), \
                    (2, 'two', 8, NULL), \
                    (3, 'three', 7, true)",
                &[],
            )
            .unwrap();
        let storage = engine.into_storage();
        storage.reset_counts();
        Engine::new(storage)
    }

    struct VisitorOnlyStorage {
        schema: TableDefinition,
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
                schema: TableDefinition {
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
            visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
        ) -> Result<VisitOutcome> {
            if table != self.schema.name {
                return Err(EngineError::table_not_found(table));
            }
            for row in std::iter::repeat_n(&self.repeated_row, self.repetitions)
                .chain(self.table_rows.iter())
            {
                self.table_visits.set(self.table_visits.get() + 1);
                if visitor(&RowRef::map(row, &self.schema))? == VisitControl::Stop {
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
            visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
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
                if visitor(&RowRef::map(row, &self.schema))? == VisitControl::Stop {
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

        fn table_schema(&self, table: &str) -> Result<std::rc::Rc<TableDefinition>> {
            if table == self.schema.name {
                Ok(std::rc::Rc::new(self.schema.clone()))
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

        assert_eq!(result.revision, 2);
        assert_eq!(
            result.fields,
            vec![
                ResultField::new("id", ColumnType::Integer),
                ResultField::new("title", ColumnType::Text),
            ]
        );
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

        assert_eq!(result.revision, 2);
        assert_eq!(
            result.fields,
            vec![
                ResultField::new("id", ColumnType::Integer),
                ResultField::new("title", ColumnType::Text),
                ResultField::new("user_id", ColumnType::Integer),
                ResultField::new("deleted", ColumnType::Boolean),
            ]
        );
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
    fn like_matches_whole_text_with_wildcards_escapes_and_folding() {
        for (text, pattern, escape, case_insensitive, expected) in [
            ("", "", Some('\\'), false, true),
            ("", "%", Some('\\'), false, true),
            ("", "_", Some('\\'), false, false),
            ("abc", "abc", Some('\\'), false, true),
            ("abc", "ab", Some('\\'), false, false),
            ("abc", "a%", Some('\\'), false, true),
            ("abc", "%c", Some('\\'), false, true),
            ("abc", "%b%", Some('\\'), false, true),
            ("abc", "a_c", Some('\\'), false, true),
            ("abc", "a__c", Some('\\'), false, false),
            ("abc", "_%_%_", Some('\\'), false, true),
            ("ab", "_%_%_", Some('\\'), false, false),
            ("aXbXc", "%X%X%", Some('\\'), false, true),
            ("aXb", "%X%X%", Some('\\'), false, false),
            ("seeded", "%ded", Some('\\'), false, true),
            ("abab", "%ab%ab", Some('\\'), false, true),
            ("aab", "%ab%ab", Some('\\'), false, false),
            ("bob", "b%b", Some('\\'), false, true),
            ("b", "b%b", Some('\\'), false, false),
            ("mississippi", "m%iss%ppi", Some('\\'), false, true),
            ("mississippi", "%sip%", Some('\\'), false, true),
            ("mississippi", "m%ss%ss%ss%", Some('\\'), false, false),
            ("100%", "100\\%", Some('\\'), false, true),
            ("1000", "100\\%", Some('\\'), false, false),
            ("a_b", "a\\_b", Some('\\'), false, true),
            ("axb", "a\\_b", Some('\\'), false, false),
            ("a\\b", "a\\\\b", Some('\\'), false, true),
            ("a%b", "a#%b", Some('#'), false, true),
            ("a\\b", "a\\b", None, false, true),
            ("a%", "a%%", Some('%'), false, true),
            ("ab", "a%%", Some('%'), false, false),
            ("é🦀", "_🦀", Some('\\'), false, true),
            ("é🦀", "__", Some('\\'), false, true),
            ("é🦀", "___", Some('\\'), false, false),
            ("Hello", "hELLO", Some('\\'), true, true),
            ("Hello", "hELLO", Some('\\'), false, false),
            ("Éte", "É_E", Some('\\'), true, true),
            // Only ASCII letters fold, as under PostgreSQL's C locale.
            ("ÉTE", "éte", Some('\\'), true, false),
            ("ß", "SS", Some('\\'), true, false),
        ] {
            assert_eq!(
                like_matches(text, pattern, escape, case_insensitive),
                expected,
                "{text:?} LIKE {pattern:?}, escape={escape:?}, case_insensitive={case_insensitive}"
            );
        }
    }

    #[test]
    fn like_predicates_validate_operands_and_propagate_null() {
        let database = engine();
        let ids = |sql: &str, params: &[Value]| {
            database
                .query_sql(sql, params)
                .unwrap()
                .rows
                .into_iter()
                .map(|row| row["id"].as_i64().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(
                "SELECT id FROM posts WHERE title LIKE 't%' ORDER BY id",
                &[]
            ),
            [2, 3]
        );
        assert_eq!(
            ids(
                "SELECT id FROM posts WHERE title NOT ILIKE $1 ORDER BY id",
                &[json!("T%")]
            ),
            [1]
        );
        assert_eq!(
            ids(
                "SELECT id FROM posts WHERE title LIKE $1 ESCAPE $2 ORDER BY id",
                &[json!("_!%%"), json!("!")],
            ),
            Vec::<i64>::new()
        );
        // A NULL pattern or escape makes the predicate unknown, and so does its negation.
        for sql in [
            "SELECT id FROM posts WHERE title LIKE NULL",
            "SELECT id FROM posts WHERE title NOT LIKE NULL",
            "SELECT id FROM posts WHERE title NOT LIKE '%' ESCAPE NULL",
        ] {
            assert!(ids(sql, &[]).is_empty(), "{sql}");
        }
        for (sql, params, code) in [
            (
                "SELECT id FROM posts WHERE user_id LIKE '7'",
                vec![],
                "TYPE_MISMATCH",
            ),
            (
                "SELECT id FROM posts WHERE title LIKE $1",
                vec![json!(7)],
                "TYPE_MISMATCH",
            ),
            (
                "SELECT id FROM posts WHERE title LIKE 'x' ESCAPE $1",
                vec![json!(true)],
                "TYPE_MISMATCH",
            ),
            (
                "SELECT id FROM posts WHERE title LIKE 'x' ESCAPE '!!'",
                vec![],
                "INVALID_QUERY",
            ),
            (
                "SELECT id FROM posts WHERE title LIKE 'x\\' LIMIT 0",
                vec![],
                "INVALID_QUERY",
            ),
            (
                "SELECT id FROM posts WHERE title LIKE 'x!' ESCAPE '!'",
                vec![],
                "INVALID_QUERY",
            ),
            (
                "SELECT id FROM posts WHERE title SIMILAR TO 'x'",
                vec![],
                "UNSUPPORTED_SQL",
            ),
        ] {
            assert_eq!(
                database.query_sql(sql, &params).unwrap_err().code,
                code,
                "{sql}"
            );
        }
        // Without ESCAPE, backslash escapes; an empty ESCAPE makes it an ordinary character.
        assert!(
            database
                .query_sql("SELECT id FROM posts WHERE title LIKE 'x\\' ESCAPE ''", &[])
                .is_ok()
        );
    }

    #[test]
    fn projection_aliases_rename_outputs_and_can_be_ordered_by() {
        let database = engine();
        let result = database
            .query_sql(
                "SELECT id AS post_id, title, id AS \"Copy\" FROM posts WHERE id = 1",
                &[],
            )
            .unwrap();
        assert_eq!(
            result.fields,
            vec![
                ResultField::new("post_id", ColumnType::Integer),
                ResultField::new("title", ColumnType::Text),
                ResultField::new("Copy", ColumnType::Integer),
            ]
        );
        assert_eq!(
            result.rows,
            vec![row(json!({"post_id": 1, "title": "one", "Copy": 1}))]
        );

        // An output name wins over a source column of the same name, so swapped aliases order by
        // the renamed column; a source column that is not projected can still be ordered by.
        assert_eq!(
            database
                .query_sql(
                    "SELECT title AS id, id AS title FROM posts ORDER BY id DESC",
                    &[],
                )
                .unwrap()
                .rows,
            vec![
                row(json!({"id": "two", "title": 2})),
                row(json!({"id": "three", "title": 3})),
                row(json!({"id": "one", "title": 1})),
            ]
        );
        assert_eq!(
            database
                .query_sql(
                    "SELECT title AS name FROM posts ORDER BY user_id DESC, name LIMIT 2",
                    &[],
                )
                .unwrap()
                .rows,
            vec![row(json!({"name": "two"})), row(json!({"name": "one"}))]
        );

        let empty = database
            .query_sql("SELECT deleted AS gone FROM posts LIMIT 0", &[])
            .unwrap();
        assert_eq!(
            empty.fields,
            vec![ResultField::new("gone", ColumnType::Boolean)]
        );
        assert!(empty.rows.is_empty());

        for (sql, code) in [
            ("SELECT id AS x, title AS x FROM posts", "INVALID_QUERY"),
            ("SELECT id, title AS id FROM posts LIMIT 0", "INVALID_QUERY"),
            ("SELECT missing AS x FROM posts", "COLUMN_NOT_FOUND"),
            ("SELECT id AS FROM posts", "SQL_PARSE_ERROR"),
            ("SELECT id AS select FROM posts", "SQL_PARSE_ERROR"),
            ("SELECT id post_id FROM posts", "UNSUPPORTED_SQL"),
            ("SELECT id AS x FROM posts ORDER BY y", "COLUMN_NOT_FOUND"),
        ] {
            assert_eq!(
                database.query_sql(sql, &[]).unwrap_err().code,
                code,
                "{sql}"
            );
        }
    }

    #[test]
    fn between_is_inclusive_and_propagates_unknown_like_its_comparisons() {
        let database = engine();
        let ids = |sql: &str, params: &[Value]| {
            database
                .query_sql(sql, params)
                .unwrap()
                .rows
                .into_iter()
                .map(|row| row["id"].as_i64().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(
                "SELECT id FROM posts WHERE id BETWEEN 1 AND 2 ORDER BY id",
                &[]
            ),
            [1, 2]
        );
        assert_eq!(
            ids(
                "SELECT id FROM posts WHERE id NOT BETWEEN $1 AND $2 ORDER BY id",
                &[json!(2), json!(2.5)],
            ),
            [1, 3]
        );
        // Bounds are not reordered: a reversed range is empty rather than symmetric.
        assert!(ids("SELECT id FROM posts WHERE id BETWEEN 2 AND 1", &[]).is_empty());
        assert_eq!(
            ids(
                "SELECT id FROM posts WHERE title BETWEEN 'one' AND 'three' ORDER BY id",
                &[],
            ),
            [1, 3]
        );
        // The trailing AND belongs to the enclosing conjunction, not to the range.
        assert_eq!(
            ids(
                "SELECT id FROM posts WHERE user_id BETWEEN 7 AND 8 AND id <> 1 ORDER BY id",
                &[],
            ),
            [2, 3]
        );
        // A NULL bound makes one comparison unknown. The range can then only be false or unknown,
        // and negating an unknown range must not match.
        assert_eq!(
            ids(
                "SELECT id FROM posts WHERE id NOT BETWEEN NULL AND 1 ORDER BY id",
                &[],
            ),
            [2, 3]
        );
        assert!(ids("SELECT id FROM posts WHERE id BETWEEN NULL AND 3", &[]).is_empty());
        assert!(
            ids(
                "SELECT id FROM posts WHERE NOT (id BETWEEN NULL AND 3)",
                &[]
            )
            .is_empty()
        );
        assert!(
            ids(
                "SELECT id FROM posts WHERE deleted NOT BETWEEN false AND true",
                &[],
            )
            .is_empty()
        );

        for (sql, code) in [
            (
                "SELECT id FROM posts WHERE id BETWEEN SYMMETRIC 2 AND 1",
                "UNSUPPORTED_SQL",
            ),
            (
                "SELECT id FROM posts WHERE id BETWEEN 1 OR 2",
                "SQL_PARSE_ERROR",
            ),
            ("SELECT id FROM posts WHERE id BETWEEN 1", "SQL_PARSE_ERROR"),
            (
                "SELECT id FROM posts WHERE id BETWEEN 'a' AND 2",
                "TYPE_MISMATCH",
            ),
        ] {
            assert_eq!(
                database.query_sql(sql, &[]).unwrap_err().code,
                code,
                "{sql}"
            );
        }
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
    fn filters_agree_with_predicate_evaluation_on_maps_and_stored_records() {
        use crate::paged_codec::{RecordLayout, StoredRecord, encode_primary_key, encode_row};

        struct Random(u64);
        impl Random {
            fn below(&mut self, bound: usize) -> usize {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                (self.0 % bound as u64) as usize
            }
            fn pick<T: Clone>(&mut self, values: &[T]) -> T {
                values[self.below(values.len())].clone()
            }
        }

        let column = |name: &str, data_type, default| ColumnDefinition {
            name: name.to_owned(),
            data_type,
            nullable: name != "id",
            default,
        };
        let schema = TableDefinition {
            name: "t".to_owned(),
            primary_key: vec!["id".to_owned()],
            columns: vec![
                column("id", ColumnType::Integer, None),
                column("b", ColumnType::Boolean, None),
                column("i", ColumnType::Integer, None),
                column("f", ColumnType::Float, None),
                column("s", ColumnType::Text, None),
                column("j", ColumnType::Json, None),
                column("d", ColumnType::Integer, Some(json!(7))),
            ],
        };
        let values = [
            vec![json!(1), json!(-4), json!(9_007_199_254_740_991_i64)],
            vec![Value::Null, json!(false), json!(true)],
            vec![
                Value::Null,
                json!(-3),
                json!(0),
                json!(5),
                json!(4_294_967_296_i64),
            ],
            vec![
                Value::Null,
                json!(-1.5),
                json!(0.0),
                json!(-0.0),
                json!(2.0),
                json!(1e300),
            ],
            vec![
                Value::Null,
                json!(""),
                json!("a"),
                json!("abc"),
                json!("a%b"),
                json!("é🦀"),
                json!("x\u{0}y"),
            ],
            vec![
                Value::Null,
                json!(1),
                json!(1.0),
                json!("a"),
                json!([]),
                json!({"k": [1, null]}),
                json!(true),
            ],
            vec![Value::Null, json!(7), json!(3)],
        ];
        let parameters = [
            Value::Null,
            json!(false),
            json!(true),
            json!(-3),
            json!(0),
            json!(-0.0),
            json!(2.0),
            json!(1.5),
            json!(5),
            json!(""),
            json!("a"),
            json!("abc"),
            json!("b"),
            json!("é🦀"),
            json!([]),
            json!({"k": [1, null]}),
        ];
        let patterns = [
            json!("%"),
            json!("a%"),
            json!("%b%"),
            json!("_"),
            json!("a\\%b"),
            json!("a#%b"),
            json!("%🦀"),
            json!("x_y"),
            Value::Null,
        ];
        let escapes = [None, Some(json!("")), Some(json!("#")), Some(Value::Null)];
        let operators = [
            ComparisonOperator::Eq,
            ComparisonOperator::Neq,
            ComparisonOperator::Lt,
            ComparisonOperator::Lte,
            ComparisonOperator::Gt,
            ComparisonOperator::Gte,
        ];

        fn predicate(
            random: &mut Random,
            depth: usize,
            schema: &TableDefinition,
            parameters: &[Value],
            patterns: &[Value],
            escapes: &[Option<Value>],
            operators: &[ComparisonOperator],
        ) -> Predicate {
            let column = schema.columns[random.below(schema.columns.len())]
                .name
                .clone();
            match random.below(if depth == 0 { 4 } else { 7 }) {
                0 => Predicate::Comparison {
                    column,
                    operator: random.pick(operators),
                    value: random.pick(parameters),
                },
                1 => Predicate::IsNull {
                    column,
                    negated: random.below(2) == 1,
                },
                2 => Predicate::In {
                    column,
                    values: (0..random.below(4))
                        .map(|_| random.pick(parameters))
                        .collect(),
                },
                3 => Predicate::Like {
                    column: "s".to_owned(),
                    pattern: random.pick(patterns),
                    escape: random.pick(escapes),
                    case_insensitive: random.below(2) == 1,
                },
                kind => {
                    let mut child = || {
                        predicate(
                            random,
                            depth - 1,
                            schema,
                            parameters,
                            patterns,
                            escapes,
                            operators,
                        )
                    };
                    match kind {
                        4 => Predicate::And {
                            predicates: vec![child(), child()],
                        },
                        5 => Predicate::Or {
                            predicates: vec![child(), child()],
                        },
                        _ => Predicate::Not {
                            predicate: Box::new(child()),
                        },
                    }
                }
            }
        }

        let outcome = |result: Result<bool>| result.map_err(|error| error.code);
        let layout = RecordLayout::new(&schema).unwrap();
        let mut random = Random(0x2545_f491_4f6c_dd1d);
        let mut checked = 0;
        for _ in 0..4_000 {
            let stored = schema
                .columns
                .iter()
                .zip(&values)
                .map(|(column, values)| (column.name.clone(), random.pick(values)))
                .collect::<Row>();
            let stored = crate::storage::normalize_row(&schema, stored).unwrap();
            let key = encode_primary_key(&schema, &stored).unwrap();
            let record = encode_row(&schema, &stored).unwrap();
            let record =
                RowRef::record(StoredRecord::new(&schema, &layout, &key, &record).unwrap());
            assert_eq!(record.to_row().unwrap(), stored);
            for (index, column) in schema.columns.iter().enumerate() {
                assert_eq!(
                    record.get(index).unwrap().into_value(),
                    stored[&column.name]
                );
            }

            let predicate = predicate(
                &mut random,
                3,
                &schema,
                &parameters,
                &patterns,
                &escapes,
                &operators,
            );
            if validate_predicate_types(&predicate, &schema, "t").is_err() {
                continue;
            }
            checked += 1;
            let expected = outcome(matches_predicate(&stored, Some(&predicate), "t"));
            let filter = Filter::new(Some(&predicate), &schema, "t").unwrap();
            let map = RowRef::map(&stored, &schema);
            assert_eq!(
                outcome(filter.matches(&map)),
                expected,
                "{predicate:?} on {stored:?}"
            );
            assert_eq!(
                outcome(filter.matches(&record)),
                expected,
                "{predicate:?} on {stored:?}"
            );
        }
        assert!(
            checked > 1_000,
            "only {checked} generated predicates were valid"
        );
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
        let mut accepted = VisitorOnlyStorage::new(repeated_row.clone(), 0);
        accepted.add_payload_column();
        // Only the projected column is charged, since the rest of the row is never copied.
        let projected_bytes = Projection::new(plan.columns.as_deref(), &accepted.schema, "items")
            .unwrap()
            .unwrap()
            .estimated_bytes(&RowRef::map(&repeated_row, &accepted.schema))
            .unwrap();
        assert_eq!(projected_bytes, 32 + 64 + 2 * 7 + 2 * (24 + 128 * 1024));
        let accepted_rows = MAX_QUERY_RESULT_BYTES / projected_bytes;
        assert!(accepted_rows * projected_bytes <= MAX_QUERY_RESULT_BYTES);
        assert!((accepted_rows + 1) * projected_bytes > MAX_QUERY_RESULT_BYTES);
        accepted.repetitions = accepted_rows;
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
            .execute_sql(
                "CREATE TABLE documents (id INTEGER PRIMARY KEY, payload JSONB)",
                &[],
            )
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
            SelectPlan {
                table: "posts".to_owned(),
                columns: Some(vec![select_column("id")]),
                predicate: None,
                order_by: vec![],
                limit: None,
                offset: 0,
            }
        );
        assert_eq!(
            parse_sql("SELECT \"ID\" FROM \"Posts\"", &[]).unwrap(),
            SelectPlan {
                table: "Posts".to_owned(),
                columns: Some(vec![select_column("ID")]),
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
            SelectPlan {
                table: "public.posts".to_owned(),
                columns: Some(vec![select_column("display\"name")]),
                predicate: Some(Predicate::And {
                    predicates: vec![
                        Predicate::Comparison {
                            column: "title".to_owned(),
                            operator: ComparisonOperator::Eq,
                            value: json!("James's post"),
                        },
                        Predicate::Comparison {
                            column: "published".to_owned(),
                            operator: ComparisonOperator::Eq,
                            value: json!(true),
                        },
                        Predicate::Comparison {
                            column: "rating".to_owned(),
                            operator: ComparisonOperator::Eq,
                            value: json!(-4.5),
                        },
                        Predicate::Comparison {
                            column: "removed".to_owned(),
                            operator: ComparisonOperator::Eq,
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
            Some(vec![select_column("from"), select_column("select")])
        );
    }

    #[test]
    fn bounds_sql_input_and_parser_complexity() {
        let too_long = "x".repeat(MAX_SQL_BYTES + 1);
        assert_eq!(parse_sql(&too_long, &[]).unwrap_err().code, "INVALID_QUERY");

        let too_many_params = vec![Value::Null; MAX_SQL_PARAMETERS + 1];
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

        let too_many_predicates = format!(
            "SELECT * FROM posts WHERE {}",
            vec!["id = 1"; MAX_PREDICATE_NODES + 1].join(" AND ")
        );
        assert_eq!(
            parse_sql(&too_many_predicates, &[]).unwrap_err().code,
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
    fn expanded_parameter_accounting_checks_its_boundary_and_overflow() {
        let value = json!("x".repeat(1024));
        let retained = estimated_value_bytes(&value).unwrap() + std::mem::size_of::<Value>();
        let maximum_copies = MAX_BOUND_PARAMETER_BYTES / retained;
        validate_bound_parameter_bytes(&[maximum_copies], std::slice::from_ref(&value)).unwrap();
        for count in [maximum_copies + 1, usize::MAX] {
            assert_eq!(
                validate_bound_parameter_bytes(&[count], std::slice::from_ref(&value))
                    .unwrap_err()
                    .code,
                "RESOURCE_LIMIT"
            );
        }
        assert_eq!(
            validate_bound_parameter_bytes(&[maximum_copies, 1], &[value.clone(), value])
                .unwrap_err()
                .code,
            "RESOURCE_LIMIT"
        );
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
