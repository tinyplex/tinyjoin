use std::borrow::Cow;
use std::collections::BTreeMap;
use std::rc::Rc;

use serde_json::{Map, Value};

#[cfg(test)]
use crate::StorageDriver;
use crate::hash::KeySet;
use crate::paged_codec::{
    EMPTY_RECORD, RecordLayout, StoredRecord, encode_primary_key, encode_primary_key_values,
    encode_row_values, encode_updated_record,
};
use crate::query::{
    Filter, ParseMode, Token, bind_parameter, exact_equalities, is_reserved_keyword,
    number_literal, parse_predicate_at, tokenize, validate_named_columns,
    validate_parameter_expansion, validate_predicate_columns, validate_predicate_types,
    validate_sql_input, visit_indexed_candidates,
};
use crate::row::{HeldRow, RowRef};
use crate::storage::{
    MAX_LOGICAL_ROW_BYTES, ensure_storage_key_bytes, estimated_checked_value_bytes,
    estimated_key_bytes, estimated_record_bytes, estimated_row_bytes, estimated_value_bytes,
    json_scalar_bound, normalize_row, primary_key_values_fit, row_json_overhead,
    schema_with_added_column, unplanned_record, validate_index_columns_for_schema,
    validate_index_definition_shape, validate_primary_storage_key_bound, validate_value,
};
use crate::{
    ChangedKeys, ColumnDefinition, ColumnType, EngineError, MAX_CHANGED_KEYS_PER_TABLE, Predicate,
    Result, ResultField, Row, RowChange, SelectPlan, StorageReader, TableDefinition, TableKeys,
    VisitControl, VisitOutcome,
};

const MAX_COLUMNS: usize = 256;
const MAX_VALUE_ROWS: usize = 4096;
const MAX_DML_SCAN_ROWS: usize = 1_000_000;
const MAX_DML_CHANGED_ROWS: usize = 100_000;
const MAX_DML_WORK_BYTES: usize = 16 * 1024 * 1024;
const MAX_DML_RESULT_BYTES: usize = 16 * 1024 * 1024;
const DML_CHANGE_RETAINED_BYTES: usize = 96;

#[derive(Clone, Debug)]
pub(crate) enum Statement {
    Select(SelectPlan),
    Aggregate(crate::aggregate::AggregatePlan),
    Join(crate::join::JoinPlan),
    Write(WriteStatement),
}

#[derive(Clone, Debug)]
pub(crate) enum WriteStatement {
    CreateTable {
        schema: TableDefinition,
        if_not_exists: bool,
    },
    CreateIndex {
        definition: crate::IndexDefinition,
        if_not_exists: bool,
    },
    DropTable {
        table: String,
        if_exists: bool,
    },
    DropIndex {
        name: String,
        if_exists: bool,
    },
    AddColumn {
        table: String,
        column: ColumnDefinition,
        if_not_exists: bool,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        values: Vec<Vec<SqlValue>>,
        on_conflict: Option<OnConflict>,
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

/// An `INSERT ... ON CONFLICT` clause.
#[derive(Clone, Debug)]
pub(crate) struct OnConflict {
    /// The conflict target's columns. `None` means every unique constraint is an arbiter, which
    /// only `DO NOTHING` permits.
    pub(crate) target: Option<Vec<String>>,
    pub(crate) action: ConflictAction,
}

#[derive(Clone, Debug)]
pub(crate) enum ConflictAction {
    Nothing,
    Update(Vec<(String, ConflictValue)>),
}

/// A `DO UPDATE SET` value: an ordinary literal, parameter, or `DEFAULT`, or a column of the row
/// proposed for insertion, spelled `EXCLUDED.column`.
#[derive(Clone, Debug)]
pub(crate) enum ConflictValue {
    Value(SqlValue),
    Excluded(String),
}

#[derive(Debug)]
pub(crate) struct WriteOutcome {
    pub command: &'static str,
    pub row_count: usize,
    pub rows: Vec<Row>,
    pub tables: Vec<String>,
    pub mutated: bool,
}

pub(crate) struct PlannedDml {
    pub outcome: WriteOutcome,
    pub changes: Vec<RowChange>,
    /// For each change, what its key held before the statement, where planning read it. Writers
    /// use it in place of looking the row up again.
    pub previous: Vec<PreviousRow>,
}

/// What a planned change's key held before its statement.
#[derive(Debug)]
pub(crate) enum PreviousRow {
    /// Planning did not read the key, or did not keep what it read.
    Unread,
    /// The key held this row, or no row.
    Read(Option<HeldRow>),
}

/// The most estimated bytes of rows one statement's planning keeps for its writer. Past it, the
/// writer looks rows up again, so planning never holds more than this beyond its own budget.
const MAX_KEPT_ROW_BYTES: usize = 8 * 1024 * 1024;

/// The rows planning keeps for its writer so far.
#[derive(Default)]
struct KeptRows {
    bytes: usize,
}

impl KeptRows {
    /// Whether a row of `bytes` can be kept, counting it if so.
    fn fits(&mut self, bytes: usize) -> bool {
        match self.bytes.checked_add(bytes) {
            Some(total) if total <= MAX_KEPT_ROW_BYTES => {
                self.bytes = total;
                true
            }
            _ => false,
        }
    }
}

pub(crate) fn parse(sql: &str, params: &[Value]) -> Result<Statement> {
    validate_sql_input(sql, params)?;
    parse_tokens(tokenize(sql)?, params, ParseMode::Bound)
}

/// The shared boundary for direct and prepared SQL. Callers check text/parameter bounds
/// before tokenization; all families share expansion accounting and consume these tokens once.
pub(crate) fn parse_tokens(
    tokens: Vec<Token>,
    params: &[Value],
    mode: ParseMode,
) -> Result<Statement> {
    validate_parameter_expansion(&tokens, params)?;
    if matches!(
        tokens.first(),
        Some(Token::Identifier {
            value,
            quoted: false,
        }) if value.eq_ignore_ascii_case("select")
    ) {
        if crate::join::is_join_select(&tokens) {
            return crate::join::parse_tokens(tokens, params, mode).map(Statement::Join);
        }
        // SELECT DISTINCT is grouping by every projected column without aggregates.
        if crate::aggregate::is_aggregate_select(&tokens)
            || crate::query::is_distinct_keyword_at(&tokens, 1)
        {
            return crate::aggregate::parse_tokens(tokens, params, mode).map(Statement::Aggregate);
        }
        return crate::query::parse_tokens(tokens, params, mode).map(Statement::Select);
    }
    MutationParser::new(tokens, params)
        .parse()
        .map(Statement::Write)
}

/// Applies one write statement, reporting both its outcome and the primary keys it changed.
#[cfg(test)]
pub(crate) fn execute<S: StorageDriver>(
    storage: &mut S,
    statement: &WriteStatement,
) -> Result<(WriteOutcome, ChangedKeys)> {
    match statement {
        WriteStatement::CreateTable {
            schema,
            if_not_exists,
        } => create_table(storage, schema, *if_not_exists).map(no_changed_keys),
        WriteStatement::CreateIndex {
            definition,
            if_not_exists,
        } => create_index(storage, definition, *if_not_exists).map(no_changed_keys),
        WriteStatement::DropTable { table, if_exists } => {
            drop_table(storage, table, *if_exists).map(no_changed_keys)
        }
        WriteStatement::DropIndex { name, if_exists } => {
            drop_index(storage, name, *if_exists).map(no_changed_keys)
        }
        WriteStatement::AddColumn {
            table,
            column,
            if_not_exists,
        } => add_column(storage, table, column, *if_not_exists).map(no_changed_keys),
        WriteStatement::Insert { .. }
        | WriteStatement::Update { .. }
        | WriteStatement::Delete { .. } => {
            let PlannedDml {
                outcome,
                changes,
                previous,
            } = plan_dml(storage, statement)?;
            let keys = changed_keys(storage, &changes, &previous)?;
            storage.apply_row_changes_unrevisioned(changes)?;
            Ok((outcome, keys))
        }
    }
}

/// A change's table and the encoding of the key it reports, by which a table's changed keys are
/// ordered and repeats found.
///
/// Changed keys are a set: the paged and in-memory engines reach the same set by different routes
/// (one applies a whole transaction write-set, the other accumulates statement by statement), so
/// the reported order must not depend on which engine produced it. A table's keys are reported in
/// the order of their encodings, which is its primary-key order, and two keys encode alike exactly
/// when they are equal.
struct EncodedKey<'a> {
    table: &'a String,
    key: Cow<'a, [u8]>,
    change: &'a RowChange,
}

/// The encoding of the key a change reports: planned as a record, it carries it. A planned map
/// always carries its table's key columns; one that somehow does not is ordered first.
fn encoded_changed_key<'a>(schema: &TableDefinition, change: &'a RowChange) -> Cow<'a, [u8]> {
    match change {
        RowChange::Put { key, .. } | RowChange::Remove { key, .. } => Cow::Borrowed(key),
        RowChange::Upsert { row, .. } | RowChange::Delete { key: row, .. } => {
            Cow::Owned(encode_primary_key(schema, row).unwrap_or_default())
        }
    }
}

#[cfg(test)]
/// Orders a table's changed keys by their encodings, as [`changed_keys`] reports them.
fn sort_changed_keys(schema: &TableDefinition, keys: &mut [Row]) {
    keys.sort_by_cached_key(|key| encode_primary_key(schema, key).unwrap_or_default());
}

#[cfg(test)]
/// Pairs a DDL outcome with an empty key set: DDL changes a table without naming rows.
fn no_changed_keys(outcome: WriteOutcome) -> (WriteOutcome, ChangedKeys) {
    (outcome, ChangedKeys::default())
}

/// Collects the primary keys a planned write-set touches, per table.
///
/// A table whose change count exceeds [`MAX_CHANGED_KEYS_PER_TABLE`] is dropped entirely rather
/// than reported partially, so a consumer can read the presence of a table as "this is every key
/// that changed". Keys are projected from the row the change carries, so a delete reports the row
/// that is going away and an upsert reports the row that replaces it. `previous` is what planning
/// read at each change's key.
pub(crate) fn changed_keys(
    storage: &dyn StorageReader,
    changes: &[RowChange],
    previous: &[PreviousRow],
) -> Result<ChangedKeys> {
    let mut keys = ChangedKeys::default();
    // One change reports one key, which needs neither the bound nor ordering.
    if let [change] = changes {
        let table = change_table(change);
        let schema = storage.table_schema(table)?;
        let mut values = Vec::with_capacity(schema.primary_key.len());
        push_changed_key(storage, &schema, change, &mut values)?;
        keys.insert(table_keys(table, &schema, values));
        return Ok(keys);
    }
    if exceeds_changed_keys(changes, previous) {
        return Ok(keys);
    }
    // A statement changes one table's rows, so the last schema read is almost always the one
    // needed.
    let mut schema: Option<Rc<TableDefinition>> = None;
    let mut schema_of = |table: &String| -> Result<Rc<TableDefinition>> {
        match &schema {
            Some(schema) if schema.name == *table => Ok(Rc::clone(schema)),
            _ => Ok(Rc::clone(schema.insert(storage.table_schema(table)?))),
        }
    };
    let mut encoded = Vec::with_capacity(changes.len());
    for change in changes {
        let table = change_table(change);
        let schema = schema_of(table)?;
        let key = encoded_changed_key(&schema, change);
        encoded.push(EncodedKey { table, key, change });
    }
    // Keys a scan or ascending values produced are already in order, which the sort checks first.
    encoded.sort_unstable_by(|left, right| (left.table, &left.key).cmp(&(right.table, &right.key)));
    encoded.dedup_by(|next, kept| next.table == kept.table && next.key == kept.key);
    for changed in encoded.chunk_by(|left, right| left.table == right.table) {
        if changed.len() > MAX_CHANGED_KEYS_PER_TABLE {
            continue;
        }
        let table = changed[0].table;
        let schema = schema_of(table)?;
        let mut values = Vec::with_capacity(changed.len() * schema.primary_key.len());
        for changed in changed {
            push_changed_key(storage, &schema, changed.change, &mut values)?;
        }
        keys.insert(table_keys(table, &schema, values));
    }
    Ok(keys)
}

/// A table's changed keys: its primary-key columns, in key order, and each key's values.
fn table_keys(table: &str, schema: &TableDefinition, values: Vec<Value>) -> TableKeys {
    TableKeys {
        table: table.to_owned(),
        columns: schema.primary_key.clone(),
        values,
    }
}

/// The table a change is to.
pub(crate) fn change_table(change: &RowChange) -> &String {
    match change {
        RowChange::Upsert { table, .. }
        | RowChange::Delete { table, .. }
        | RowChange::Put { table, .. }
        | RowChange::Remove { table, .. } => table,
    }
}

/// Whether changes to one table change more rows than it can report keys for, which is found
/// without projecting a key where each change's encoded key is at hand: planned as a record, or
/// the key of the stored row planning read there. Keys encoded differently project differently,
/// so more changes than the bound, whose encoded keys strictly ascend, report too many keys.
fn exceeds_changed_keys(changes: &[RowChange], previous: &[PreviousRow]) -> bool {
    let Some(first) = changes.get(MAX_CHANGED_KEYS_PER_TABLE) else {
        return false;
    };
    let table = change_table(first);
    let mut last: Option<&[u8]> = None;
    for (index, change) in changes.iter().enumerate() {
        let key = match (change, previous.get(index)) {
            (RowChange::Put { key, .. } | RowChange::Remove { key, .. }, _) => key.as_slice(),
            (_, Some(PreviousRow::Read(Some(HeldRow::Stored(entry))))) => entry.key(),
            _ => return false,
        };
        if change_table(change) != table || last.is_some_and(|last| last >= key) {
            return false;
        }
        last = Some(key);
    }
    true
}

/// Appends a changed row's primary-key values, in key order: projected from the row a change
/// carries, or decoded from the key of a row planned as its record. A planned map always carries
/// its table's key columns; one that somehow does not reports null for them rather than failing an
/// otherwise valid write.
fn push_changed_key(
    storage: &dyn StorageReader,
    schema: &TableDefinition,
    change: &RowChange,
    values: &mut Vec<Value>,
) -> Result<()> {
    match change {
        RowChange::Upsert { row, .. } | RowChange::Delete { key: row, .. } => {
            for name in &schema.primary_key {
                values.push(row.get(name).cloned().unwrap_or(Value::Null));
            }
            Ok(())
        }
        RowChange::Put { table, key, .. } | RowChange::Remove { table, key } => {
            let layout = storage.record_layout(table).ok_or_else(unplanned_record)?;
            StoredRecord::new(schema, &layout, key, EMPTY_RECORD)?.push_key_values(values)
        }
    }
}

#[cfg(test)]
/// Accumulates one statement's reported keys into a transaction's running set.
///
/// `tables` is the statement's authoritative changed-table list. A table that changed but reported
/// no keys overflowed, and poisons the running set for that table: once any statement in a
/// transaction cannot name its keys, the transaction as a whole cannot either.
pub(crate) fn merge_changed_keys(
    accumulated: &mut BTreeMap<String, Option<Vec<Row>>>,
    tables: &[String],
    incoming: &ChangedKeys,
) {
    for table in tables {
        let entry = accumulated
            .entry(table.clone())
            .or_insert_with(|| Some(vec![]));
        let Some(incoming_keys) = incoming.rows(table) else {
            *entry = None;
            continue;
        };
        let Some(keys) = entry else {
            continue;
        };
        for key in incoming_keys {
            if keys.len() >= MAX_CHANGED_KEYS_PER_TABLE {
                *entry = None;
                break;
            }
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
}

#[cfg(test)]
/// Drops the tables that could not report a complete key set, leaving only usable entries, each
/// in the order [`changed_keys`] reports them.
pub(crate) fn finish_changed_keys(
    storage: &dyn StorageReader,
    accumulated: BTreeMap<String, Option<Vec<Row>>>,
) -> Result<ChangedKeys> {
    let mut finished = ChangedKeys::default();
    for (table, keys) in accumulated {
        if let Some(mut keys) = keys {
            let schema = storage.table_schema(&table)?;
            sort_changed_keys(&schema, &mut keys);
            let mut values = Vec::new();
            for key in &keys {
                for name in &schema.primary_key {
                    values.push(key.get(name).cloned().unwrap_or(Value::Null));
                }
            }
            finished.insert(table_keys(&table, &schema, values));
        }
    }
    Ok(finished)
}

/// Plans one SQL row mutation without modifying storage.
///
/// Both the in-memory and paged engines use this path so validation, resource limits, affected-row
/// selection, and `RETURNING` semantics cannot drift between their publication mechanisms.
pub(crate) fn plan_dml(
    storage: &dyn StorageReader,
    statement: &WriteStatement,
) -> Result<PlannedDml> {
    match statement {
        WriteStatement::Insert {
            table,
            columns,
            values,
            on_conflict,
            returning,
        } => plan_insert(
            storage,
            table,
            columns.as_deref(),
            values,
            on_conflict.as_ref(),
            returning.as_deref(),
        ),
        WriteStatement::Update {
            table,
            assignments,
            predicate,
            returning,
        } => plan_update(
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
        } => plan_delete(storage, table, predicate.as_ref(), returning.as_deref()),
        WriteStatement::CreateTable { .. }
        | WriteStatement::CreateIndex { .. }
        | WriteStatement::DropTable { .. }
        | WriteStatement::DropIndex { .. }
        | WriteStatement::AddColumn { .. } => Err(EngineError::unsupported_sql(
            "Page-native SQL currently supports SELECT, CREATE TABLE, INSERT, UPDATE, and DELETE",
        )),
    }
}

pub(crate) fn write_result_fields(
    storage: &dyn StorageReader,
    statement: &WriteStatement,
) -> Result<Vec<ResultField>> {
    let (table, returning) = match statement {
        WriteStatement::Insert {
            table, returning, ..
        }
        | WriteStatement::Update {
            table, returning, ..
        }
        | WriteStatement::Delete {
            table, returning, ..
        } => (table, returning.as_deref()),
        _ => return Ok(vec![]),
    };
    let Some(returning) = returning else {
        return Ok(vec![]);
    };
    let schema = storage.table_schema(table)?;
    crate::query::projection_fields(&schema, (!returning.is_empty()).then_some(returning))
}

#[cfg(test)]
fn drop_table<S: StorageDriver>(
    storage: &mut S,
    table: &str,
    if_exists: bool,
) -> Result<WriteOutcome> {
    let outcome = plan_drop_table(storage, table, if_exists)?;
    if outcome.mutated {
        storage.drop_table(table)?;
    }
    Ok(outcome)
}

pub(crate) fn plan_drop_table(
    storage: &dyn StorageReader,
    table: &str,
    if_exists: bool,
) -> Result<WriteOutcome> {
    storage.ensure_readable()?;
    if let Err(error) = storage.table_schema(table) {
        if error.code != "TABLE_NOT_FOUND" || !if_exists {
            return Err(error);
        }
        return Ok(WriteOutcome {
            command: "DROP TABLE",
            row_count: 0,
            rows: vec![],
            tables: vec![],
            mutated: false,
        });
    }
    Ok(WriteOutcome {
        command: "DROP TABLE",
        row_count: 0,
        rows: vec![],
        tables: vec![table.to_owned()],
        mutated: true,
    })
}

#[cfg(test)]
fn drop_index<S: StorageDriver>(
    storage: &mut S,
    name: &str,
    if_exists: bool,
) -> Result<WriteOutcome> {
    let outcome = plan_drop_index(storage, name, if_exists)?;
    if outcome.mutated {
        storage.drop_index(name)?;
    }
    Ok(outcome)
}

pub(crate) fn plan_drop_index(
    storage: &dyn StorageReader,
    name: &str,
    if_exists: bool,
) -> Result<WriteOutcome> {
    storage.ensure_readable()?;
    let Some(definition) = storage.index_definition(name) else {
        if if_exists {
            return Ok(WriteOutcome {
                command: "DROP INDEX",
                row_count: 0,
                rows: vec![],
                tables: vec![],
                mutated: false,
            });
        }
        return Err(EngineError::new(
            "INDEX_NOT_FOUND",
            format!("Index `{name}` is not defined"),
        ));
    };
    Ok(WriteOutcome {
        command: "DROP INDEX",
        row_count: 0,
        rows: vec![],
        tables: vec![definition.table],
        mutated: true,
    })
}

#[cfg(test)]
fn add_column<S: StorageDriver>(
    storage: &mut S,
    table: &str,
    column: &ColumnDefinition,
    if_not_exists: bool,
) -> Result<WriteOutcome> {
    let outcome = plan_add_column(storage, table, column, if_not_exists)?;
    if outcome.mutated {
        storage.add_column(table, column.clone())?;
    }
    Ok(outcome)
}

pub(crate) fn plan_add_column(
    storage: &dyn StorageReader,
    table: &str,
    column: &ColumnDefinition,
    if_not_exists: bool,
) -> Result<WriteOutcome> {
    storage.ensure_readable()?;
    let schema = storage.table_schema(table)?;
    if schema
        .columns
        .iter()
        .any(|existing| existing.name == column.name)
    {
        if if_not_exists {
            return Ok(WriteOutcome {
                command: "ALTER TABLE",
                row_count: 0,
                rows: vec![],
                tables: vec![],
                mutated: false,
            });
        }
        return Err(EngineError::column_already_exists(&column.name, table));
    }
    schema_with_added_column(&schema, column)?;
    Ok(WriteOutcome {
        command: "ALTER TABLE",
        row_count: 0,
        rows: vec![],
        tables: vec![table.to_owned()],
        mutated: true,
    })
}

#[cfg(test)]
fn create_index<S: StorageDriver>(
    storage: &mut S,
    definition: &crate::IndexDefinition,
    if_not_exists: bool,
) -> Result<WriteOutcome> {
    let outcome = plan_create_index(storage, definition, if_not_exists)?;
    if outcome.mutated {
        storage.define_index(definition.clone())?;
    }
    Ok(outcome)
}

pub(crate) fn plan_create_index(
    storage: &dyn StorageReader,
    definition: &crate::IndexDefinition,
    if_not_exists: bool,
) -> Result<WriteOutcome> {
    if let Some(existing) = storage.index_definition(&definition.name) {
        // The metadata probe is infallible so page storage can report its last confirmed
        // catalog after an ambiguous commit. Force a fallible read before accepting the duplicate
        // name, while still checking that name before the requested table or columns.
        storage.table_schema(&existing.table)?;
        if if_not_exists {
            return Ok(WriteOutcome {
                command: "CREATE INDEX",
                row_count: 0,
                rows: vec![],
                tables: vec![],
                mutated: false,
            });
        }
        return Err(EngineError::index_already_exists(&definition.name));
    }
    validate_index_definition_shape(definition)?;
    let schema = storage.table_schema(&definition.table)?;
    validate_index_columns_for_schema(definition, &schema)?;
    Ok(WriteOutcome {
        command: "CREATE INDEX",
        row_count: 0,
        rows: vec![],
        tables: vec![definition.table.clone()],
        mutated: true,
    })
}

#[cfg(test)]
fn create_table<S: StorageDriver>(
    storage: &mut S,
    schema: &TableDefinition,
    if_not_exists: bool,
) -> Result<WriteOutcome> {
    let outcome = plan_create_table(storage, schema, if_not_exists)?;
    if outcome.mutated {
        storage.define_table(schema.clone())?;
    }
    Ok(outcome)
}

pub(crate) fn plan_create_table(
    storage: &dyn StorageReader,
    schema: &TableDefinition,
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

    Ok(WriteOutcome {
        command: "CREATE TABLE",
        row_count: 0,
        rows: vec![],
        tables: vec![schema.name.clone()],
        mutated: true,
    })
}

fn plan_insert(
    storage: &dyn StorageReader,
    table: &str,
    columns: Option<&[String]>,
    value_rows: &[Vec<SqlValue>],
    on_conflict: Option<&OnConflict>,
    returning: Option<&[String]>,
) -> Result<PlannedDml> {
    let schema = storage.table_schema(table)?;
    let default_values =
        columns.is_none() && value_rows.len() == 1 && value_rows.first().is_some_and(Vec::is_empty);
    let all_columns;
    let columns = match columns {
        Some(columns) => columns,
        None => {
            all_columns = schema
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect::<Vec<_>>();
            &all_columns
        }
    };
    // Each schema column's place among the named ones, found once for all of the rows.
    let positions = named_column_positions(&schema, columns)?;
    validate_projection(&schema, returning)?;
    let mut conflicts = on_conflict
        .map(|clause| ConflictPlan::new(storage, &schema, clause))
        .transpose()?;

    // Canonical primary keys of every row this statement writes, whether inserted or updated. A
    // lone row cannot meet another, so without a conflict clause its key is not needed, and with
    // one its canonical key is never looked for. Without a conflict clause, the encoded key is
    // canonical and serves.
    let lone = value_rows.len() == 1;
    let tracks_keys = conflicts.is_some() || !lone;
    let mut written = KeySet::with_capacity_and_hasher(
        if tracks_keys { value_rows.len() } else { 0 },
        Default::default(),
    );
    let mut written_keys = KeySet::with_capacity_and_hasher(
        if conflicts.is_none() && tracks_keys {
            value_rows.len()
        } else {
            0
        },
        Default::default(),
    );
    let mut changes = Vec::with_capacity(value_rows.len());
    let mut previous = Vec::with_capacity(value_rows.len());
    let mut kept = KeptRows::default();
    let mut returned = Vec::with_capacity(returning.map_or(0, |_| value_rows.len()));
    let mut work_bytes = 0usize;
    let mut result_bytes = 0usize;
    // A reader that stores records lets a plain INSERT plan each row straight into the entry it
    // writes, without a map.
    let records = if conflicts.is_none() && returning.is_none() {
        storage
            .record_layout(table)
            .map(|layout| RecordPlan::new(&schema, layout))
            .transpose()?
    } else {
        None
    };
    // A lone upsert without RETURNING rewrites a stored row it conflicts with as its record, as an
    // UPDATE that keeps each row's key does.
    if let Some(conflicts) = &mut conflicts {
        conflicts.keeps_stored =
            lone && returning.is_none() && storage.record_layout(table).is_some();
    }
    for values in value_rows {
        if !default_values && values.len() != columns.len() {
            return Err(EngineError::invalid_query(format!(
                "INSERT names {} columns but provides {} values",
                columns.len(),
                values.len()
            )));
        }
        let prospective_bytes =
            prospective_insert_row_bytes(&schema, &positions, values, default_values)?;
        let prospective_charge = checked_dml_add(
            checked_dml_mul(prospective_bytes, 2)?,
            checked_dml_add(table.len(), DML_CHANGE_RETAINED_BYTES + 160)?,
        )?;
        ensure_dml_work_bytes(checked_dml_add(work_bytes, prospective_charge)?)?;
        if let Some(plan) = &records
            && let Some(values) = plan.values(&schema, &positions, values, default_values)
        {
            // Checked as normalizing a map of them would check them, in schema order; a FLOAT is
            // respelled by encoding it.
            for (column, value) in schema.columns.iter().zip(&values) {
                validate_value(column, value, table)?;
            }
            // Only its size can fail a key of valid values, however it is measured.
            let key = encode_primary_key_values(&schema, &plan.layout, &values)?;
            ensure_storage_key_bytes(key.len())?;
            let key_charge = checked_dml_add(checked_dml_mul(key.len(), 2)?, 64)?;
            work_bytes = checked_dml_add(work_bytes, key_charge)?;
            ensure_dml_work_bytes(work_bytes)?;
            if (tracks_keys && !written_keys.insert(key.clone()))
                || storage.holds_encoded_key(table, &key)?
            {
                return Err(duplicate_primary_key("INSERT into", table));
            }
            let record = encode_row_values(&schema, &plan.layout, &values)?;
            // Charged as the map it replaces, which the record's estimate is.
            let record_bytes =
                estimated_record_bytes(&StoredRecord::new(&schema, &plan.layout, &key, &record)?)?;
            work_bytes = checked_dml_add(work_bytes, checked_dml_add(record_bytes, 96)?)?;
            ensure_dml_work_bytes(work_bytes)?;
            work_bytes = retain_dml_change(work_bytes, table)?;
            changes.push(RowChange::Put {
                table: table.to_owned(),
                key,
                record,
            });
            previous.push(PreviousRow::Read(None));
            continue;
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
        validate_primary_storage_key_bound(&schema, &row)?;
        let storage_key = encode_primary_key(&schema, &row)?;
        let key_charge = checked_dml_add(checked_dml_mul(storage_key.len(), 2)?, 64)?;
        work_bytes = checked_dml_add(work_bytes, key_charge)?;
        ensure_dml_work_bytes(work_bytes)?;
        let key = if conflicts.is_some() && !lone {
            primary_conflict_key(&schema, &row)?
        } else {
            String::new()
        };

        let conflict = match &mut conflicts {
            Some(conflicts) => {
                conflicts.find(storage, &schema, &row, &key, &written, &mut work_bytes)?
            }
            None => Conflict::None,
        };
        let updates = conflicts.as_ref().and_then(ConflictPlan::updates);
        let conflict = match (conflict, updates) {
            (Conflict::Stored(held, bytes), Some(updates)) => {
                match rewritten_record(storage, &schema, updates, &held, &row)? {
                    Some(RewrittenRecord {
                        key,
                        record,
                        bytes: record_bytes,
                    }) => {
                        // Charged as the map it replaces, which the record's estimate is.
                        work_bytes =
                            checked_dml_add(work_bytes, checked_dml_add(record_bytes, 96)?)?;
                        ensure_dml_work_bytes(work_bytes)?;
                        work_bytes = retain_dml_change(work_bytes, table)?;
                        changes.push(RowChange::Put {
                            table: table.to_owned(),
                            key,
                            record,
                        });
                        previous.push(if kept.fits(bytes) {
                            PreviousRow::Read(Some(held))
                        } else {
                            PreviousRow::Unread
                        });
                        continue;
                    }
                    None => {
                        Conflict::Existing(held_row(&schema, storage, &held)?, Some((held, bytes)))
                    }
                }
            }
            (Conflict::Stored(held, bytes), None) => {
                Conflict::Existing(held_row(&schema, storage, &held)?, Some((held, bytes)))
            }
            (conflict, _) => conflict,
        };
        let (row, key, held) = match conflict {
            Conflict::None => {
                let repeated = if conflicts.is_some() {
                    written.contains(&key)
                } else {
                    tracks_keys && !written_keys.insert(storage_key)
                };
                if repeated
                    || storage.visit_primary_key(table, &row, &mut |_| Ok(VisitControl::Stop))?
                        == VisitOutcome::Stopped
                {
                    return Err(duplicate_primary_key("INSERT into", table));
                }
                (row, key, PreviousRow::Read(None))
            }
            Conflict::Written | Conflict::Existing(..) if updates.is_none() => continue,
            Conflict::Stored(..) => unreachable!("a stored conflict was rewritten or read above"),
            Conflict::Written => {
                return Err(EngineError::constraint_violation(format!(
                    "INSERT ... ON CONFLICT DO UPDATE cannot affect a row in `{table}` a second time"
                )));
            }
            Conflict::Existing(existing, stored) => {
                // The updated row keeps the existing row's key, which held the existing row.
                let held = match stored {
                    Some((held, bytes)) => kept.fits(bytes).then_some(held),
                    None => kept
                        .fits(estimated_row_bytes(&existing)?)
                        .then(|| HeldRow::Map(existing.clone())),
                };
                let row = updated_conflict_row(
                    &schema,
                    updates.expect("DO NOTHING was handled above"),
                    existing,
                    &row,
                )?;
                let key = if lone {
                    String::new()
                } else {
                    primary_conflict_key(&schema, &row)?
                };
                (
                    row,
                    key,
                    held.map_or(PreviousRow::Unread, |held| PreviousRow::Read(Some(held))),
                )
            }
        };
        work_bytes = retain_dml_row(work_bytes, &row)?;
        work_bytes = retain_dml_change(work_bytes, table)?;
        if let Some(conflicts) = &mut conflicts
            && !lone
        {
            conflicts.record(&schema, &row, &mut work_bytes)?;
            written.insert(key);
        }
        if let Some(columns) = returning {
            result_bytes = retain_returned_row(result_bytes, &row, columns)?;
            returned.push(project_returning_row(&row, columns, table)?);
        }
        changes.push(RowChange::Upsert {
            table: table.to_owned(),
            row,
        });
        previous.push(held);
    }

    let row_count = changes.len();
    Ok(PlannedDml {
        outcome: WriteOutcome {
            command: "INSERT",
            row_count,
            rows: returned,
            tables: (row_count > 0)
                .then(|| table.to_owned())
                .into_iter()
                .collect(),
            mutated: row_count > 0,
        },
        changes,
        previous,
    })
}

fn primary_conflict_key(schema: &TableDefinition, row: &Row) -> Result<String> {
    conflict_key(schema, &schema.primary_key, row)?.ok_or_else(|| {
        EngineError::invalid_change(format!(
            "A primary-key column in `{}` cannot be null",
            schema.name
        ))
    })
}

/// Encodes the values of `columns` so that SQL-equal values encode identically, or `None` when
/// any of them is `NULL`, which can never conflict with anything.
fn conflict_key(schema: &TableDefinition, columns: &[String], row: &Row) -> Result<Option<String>> {
    let mut parts = Vec::with_capacity(columns.len());
    for column in columns {
        let definition = schema
            .columns
            .iter()
            .find(|definition| definition.name == *column)
            .ok_or_else(|| EngineError::column_not_found(column, &schema.name))?;
        let value = row
            .get(column)
            .ok_or_else(|| EngineError::column_not_found(column, &schema.name))?;
        if value == &Value::Null {
            return Ok(None);
        }
        parts.push(crate::aggregate::group_key_part(
            definition.data_type,
            value,
            column,
        )?);
    }
    crate::aggregate::encode_group_key(&parts).map(Some)
}

enum Conflict {
    None,
    /// The proposed row conflicts with a row this statement already inserted or updated.
    Written,
    /// The proposed row conflicts with an existing row. A row found by its primary key also comes
    /// as a writer keeps it, with the bytes that keeps.
    Existing(Row, Option<(HeldRow, usize)>),
    /// The proposed row conflicts with a stored row found by its primary key, kept as its entry
    /// with the bytes that keeps, for a lone row that rewrites the record without a map of it.
    Stored(HeldRow, usize),
}

/// One arbiter unique index, with the keys this statement has written into it.
struct ConflictIndex {
    definition: crate::IndexDefinition,
    written: KeySet<String>,
    /// Existing primary keys by index key, collected with one scan when the storage view cannot
    /// visit this index directly, as inside a transaction.
    scanned: Option<BTreeMap<String, Row>>,
}

/// A validated `ON CONFLICT` clause for one table.
///
/// PostgreSQL inserts proposed rows one at a time, so a row conflicts both with committed rows and
/// with rows earlier in the same statement. Only arbiters are consulted: a conflict on any other
/// unique constraint still fails the statement when the write-set is validated.
struct ConflictPlan {
    primary: bool,
    indexes: Vec<ConflictIndex>,
    updates: Option<Vec<(String, ResolvedConflictValue)>>,
    /// Whether a stored row found by its primary key comes as its entry, to be rewritten as its
    /// record, rather than as a map.
    keeps_stored: bool,
}

enum ResolvedConflictValue {
    Value(Value),
    Excluded(String),
}

impl ConflictPlan {
    fn new(
        storage: &dyn StorageReader,
        schema: &TableDefinition,
        clause: &OnConflict,
    ) -> Result<Self> {
        let mut primary = true;
        let mut indexes = Vec::new();
        for definition in storage.indexes_for_table(&schema.name)? {
            if definition.unique {
                indexes.push(ConflictIndex {
                    definition,
                    written: KeySet::default(),
                    scanned: None,
                });
            }
        }
        if let Some(target) = &clause.target {
            validate_named_columns(schema, target)?;
            let same_columns = |columns: &[String]| {
                columns.len() == target.len()
                    && columns.iter().all(|column| target.contains(column))
            };
            primary = same_columns(&schema.primary_key);
            // Index names are unique, so keeping the first match is deterministic.
            indexes.retain(|index| !primary && same_columns(&index.definition.columns));
            indexes.truncate(1);
            if !primary && indexes.is_empty() {
                return Err(EngineError::invalid_query(format!(
                    "The ON CONFLICT target does not match the primary key or a unique index of `{}`",
                    schema.name
                )));
            }
        }
        let updates = match &clause.action {
            ConflictAction::Nothing => None,
            ConflictAction::Update(assignments) => {
                let mut resolved = Vec::with_capacity(assignments.len());
                for (column, value) in assignments {
                    if resolved
                        .iter()
                        .any(|(assigned, _): &(String, _)| assigned == column)
                    {
                        return Err(EngineError::invalid_query(format!(
                            "Column `{column}` is named more than once"
                        )));
                    }
                    // `column_default` also rejects an unknown column.
                    let default = column_default(schema, column)?;
                    let value = match value {
                        ConflictValue::Value(SqlValue::Value(value)) => {
                            let definition = schema
                                .columns
                                .iter()
                                .find(|definition| definition.name == *column)
                                .expect("column_default found the column");
                            validate_value(definition, value, &schema.name)?;
                            ResolvedConflictValue::Value(value.clone())
                        }
                        ConflictValue::Value(SqlValue::Default) => {
                            ResolvedConflictValue::Value(default)
                        }
                        ConflictValue::Excluded(source) => {
                            column_default(schema, source)?;
                            ResolvedConflictValue::Excluded(source.clone())
                        }
                    };
                    resolved.push((column.clone(), value));
                }
                Some(resolved)
            }
        };
        Ok(Self {
            primary,
            indexes,
            updates,
            keeps_stored: false,
        })
    }

    fn updates(&self) -> Option<&[(String, ResolvedConflictValue)]> {
        self.updates.as_deref()
    }

    fn find(
        &mut self,
        storage: &dyn StorageReader,
        schema: &TableDefinition,
        row: &Row,
        key: &str,
        written: &KeySet<String>,
        work_bytes: &mut usize,
    ) -> Result<Conflict> {
        if self.primary && written.contains(key) {
            return Ok(Conflict::Written);
        }
        let mut index_keys = Vec::with_capacity(self.indexes.len());
        for index in &self.indexes {
            let index_key = conflict_key(schema, &index.definition.columns, row)?;
            if index_key
                .as_ref()
                .is_some_and(|index_key| index.written.contains(index_key))
            {
                return Ok(Conflict::Written);
            }
            index_keys.push(index_key);
        }
        if self.primary {
            let mut existing = None;
            storage.visit_primary_key(&schema.name, row, &mut |found| {
                let (held, bytes) = (found.hold()?, found.held_bytes()?);
                existing = Some(if self.keeps_stored && found.stored().is_some() {
                    Conflict::Stored(held, bytes)
                } else {
                    Conflict::Existing(found.to_row()?, Some((held, bytes)))
                });
                Ok(VisitControl::Stop)
            })?;
            if let Some(existing) = existing {
                return Ok(existing);
            }
        }
        for (index, index_key) in self.indexes.iter_mut().zip(index_keys) {
            let Some(index_key) = index_key else {
                continue;
            };
            for existing in index.existing(storage, schema, row, &index_key, work_bytes)? {
                // A row this statement already rewrote was checked above through its new values.
                if written.is_empty()
                    || !written.contains(&primary_conflict_key(schema, &existing)?)
                {
                    return Ok(Conflict::Existing(existing, None));
                }
            }
        }
        Ok(Conflict::None)
    }

    /// Remembers the arbiter index keys of a row this statement writes.
    fn record(
        &mut self,
        schema: &TableDefinition,
        row: &Row,
        work_bytes: &mut usize,
    ) -> Result<()> {
        for index in &mut self.indexes {
            if let Some(index_key) = conflict_key(schema, &index.definition.columns, row)? {
                *work_bytes = checked_dml_add(
                    *work_bytes,
                    checked_dml_add(checked_dml_mul(index_key.len(), 2)?, 64)?,
                )?;
                ensure_dml_work_bytes(*work_bytes)?;
                index.written.insert(index_key);
            }
        }
        Ok(())
    }
}

/// Applies `DO UPDATE SET` to the conflicting row, reading `EXCLUDED` from the proposed row.
fn updated_conflict_row(
    schema: &TableDefinition,
    updates: &[(String, ResolvedConflictValue)],
    existing: Row,
    proposed: &Row,
) -> Result<Row> {
    let existing_key = primary_conflict_key(schema, &existing)?;
    let mut row = existing;
    for (column, value) in updates {
        let value = match value {
            ResolvedConflictValue::Value(value) => value.clone(),
            ResolvedConflictValue::Excluded(source) => proposed
                .get(source)
                .cloned()
                .ok_or_else(|| EngineError::column_not_found(source, &schema.name))?,
        };
        row.insert(column.clone(), value);
    }
    let row = normalize_row(schema, row)?;
    if primary_conflict_key(schema, &row)? != existing_key {
        return Err(EngineError::unsupported_sql(format!(
            "INSERT ... ON CONFLICT DO UPDATE cannot change the primary key of a row in `{}`",
            schema.name
        )));
    }
    Ok(row)
}

impl ConflictIndex {
    fn existing(
        &mut self,
        storage: &dyn StorageReader,
        schema: &TableDefinition,
        row: &Row,
        index_key: &str,
        work_bytes: &mut usize,
    ) -> Result<Vec<Row>> {
        if self.scanned.is_none() {
            let mut lookup = Row::new();
            for column in &self.definition.columns {
                lookup.insert(column.clone(), row[column].clone());
            }
            let mut rows = Vec::new();
            if storage
                .visit_index(
                    &schema.name,
                    &self.definition.columns,
                    &lookup,
                    &mut |row| {
                        rows.push(row.to_row()?);
                        Ok(VisitControl::Continue)
                    },
                )?
                .is_some()
            {
                return Ok(rows);
            }
            self.scanned = Some(self.scan(storage, schema, work_bytes)?);
        }
        match self
            .scanned
            .as_ref()
            .expect("the scan was collected above")
            .get(index_key)
        {
            Some(primary_key) => Ok(storage
                .lookup_primary_key(&schema.name, primary_key)?
                .into_iter()
                .collect()),
            None => Ok(vec![]),
        }
    }

    fn scan(
        &self,
        storage: &dyn StorageReader,
        schema: &TableDefinition,
        work_bytes: &mut usize,
    ) -> Result<BTreeMap<String, Row>> {
        let mut scanned = 0usize;
        let mut keys = BTreeMap::new();
        let outcome = storage.visit_table(&schema.name, &mut |row| {
            scanned = scanned.saturating_add(1);
            if scanned > MAX_DML_SCAN_ROWS {
                return Err(dml_limit_error(format!(
                    "A data-modification statement cannot scan more than {MAX_DML_SCAN_ROWS} rows"
                )));
            }
            let row = row.to_row()?;
            if let Some(index_key) = conflict_key(schema, &self.definition.columns, &row)? {
                let primary_key = primary_key_row(schema, &row)?;
                *work_bytes = checked_dml_add(
                    *work_bytes,
                    checked_dml_add(
                        checked_dml_mul(index_key.len(), 2)?,
                        checked_dml_add(estimated_row_bytes(&primary_key)?, 64)?,
                    )?,
                )?;
                ensure_dml_work_bytes(*work_bytes)?;
                keys.insert(index_key, primary_key);
            }
            Ok(VisitControl::Continue)
        })?;
        require_complete_dml_scan(outcome, &schema.name)?;
        Ok(keys)
    }
}

fn plan_update(
    storage: &dyn StorageReader,
    table: &str,
    assignments: &[(String, SqlValue)],
    predicate: Option<&Predicate>,
    returning: Option<&[String]>,
) -> Result<PlannedDml> {
    let schema = storage.table_schema(table)?;
    validate_named_columns(
        &schema,
        &assignments
            .iter()
            .map(|(column, _)| column.clone())
            .collect::<Vec<_>>(),
    )?;
    if let Some(predicate) = predicate {
        validate_predicate_columns(predicate, &schema, table)?;
        validate_predicate_types(predicate, &schema, table)?;
    }
    validate_projection(&schema, returning)?;

    let assignment_bytes = assignments
        .iter()
        .try_fold(0usize, |bytes, (column, value)| {
            let definition = schema
                .columns
                .iter()
                .find(|definition| definition.name == *column)
                .expect("assignment columns were validated above");
            let value = match value {
                SqlValue::Value(value) => value,
                SqlValue::Default => definition.default.as_ref().unwrap_or(&Value::Null),
            };
            validate_value(definition, value, table)?;
            let value_bytes = estimated_value_bytes(value)?;
            checked_dml_add(
                bytes,
                checked_dml_add(
                    checked_dml_mul(column.len(), 2)?,
                    checked_dml_add(checked_dml_mul(value_bytes, 2)?, 64)?,
                )?,
            )
        })?;
    ensure_dml_work_bytes(assignment_bytes)?;
    let resolved_assignments = assignments
        .iter()
        .map(|(column, value)| {
            Ok((
                column.clone(),
                match value {
                    SqlValue::Value(value) => value.clone(),
                    SqlValue::Default => column_default(&schema, column)?,
                },
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    struct PlannedUpdate {
        old_key: Vec<u8>,
        /// The row being updated, when it is kept for the writer.
        old_row: Option<HeldRow>,
        next: UpdatedRow,
    }
    enum UpdatedRow {
        /// A row planned as a map: its old key's columns, its new key, and itself.
        Map {
            old_primary_key: Row,
            new_key: Vec<u8>,
            new_row: Row,
        },
        /// A row that keeps its key, planned as the record it is rewritten with.
        Record(Vec<u8>),
    }
    impl PlannedUpdate {
        fn new_key(&self) -> &[u8] {
            match &self.next {
                UpdatedRow::Map { new_key, .. } => new_key,
                UpdatedRow::Record(_) => &self.old_key,
            }
        }
    }
    let records = RecordUpdate::new(storage, &schema, &resolved_assignments, returning)?;

    let mut updates = Vec::new();
    let mut kept = KeptRows::default();
    let mut returned = Vec::new();
    let mut scanned = 0usize;
    let mut work_bytes = 0usize;
    let mut result_bytes = 0usize;
    let filter = Filter::new(predicate, &schema, table)?;
    let visit_outcome = visit_dml_candidates(storage, &schema, predicate, &mut |row| {
        scanned = scanned.saturating_add(1);
        if scanned > MAX_DML_SCAN_ROWS {
            return Err(dml_limit_error(format!(
                "A data-modification statement cannot scan more than {MAX_DML_SCAN_ROWS} rows"
            )));
        }
        if !filter.matches(row)? {
            return Ok(VisitControl::Continue);
        }
        if updates.len() == MAX_DML_CHANGED_ROWS {
            return Err(dml_limit_error(format!(
                "A data-modification statement cannot change more than {MAX_DML_CHANGED_ROWS} rows"
            )));
        }

        // A stored row keeping its key is rewritten as its record, charged as the map it replaces.
        let record = records
            .as_ref()
            .and_then(|plan| Some((plan, plan.record(row)?)));
        let map = match record {
            Some(_) => None,
            None => Some(row.to_row()?),
        };
        // Check the retained candidate budget before copying assignment values into the row.
        let old_row_bytes = match (&map, record) {
            (Some(map), _) => estimated_row_bytes(map)?,
            (None, Some((_, record))) => estimated_record_bytes(record)?,
            (None, None) => unreachable!("a row is read as a map or a record"),
        };
        let conservative_row_bytes = checked_dml_add(
            checked_dml_mul(old_row_bytes, 3)?,
            checked_dml_add(assignment_bytes, 256)?,
        )?;
        ensure_dml_work_bytes(checked_dml_add(work_bytes, conservative_row_bytes)?)?;

        let old_key = row.encoded_key()?.into_owned();
        let old_row = if kept.fits(row.held_bytes()?) {
            Some(row.hold()?)
        } else {
            None
        };
        let (next, next_bytes, key_bytes) = match (map, record) {
            (None, Some((plan, record))) => {
                let next = encode_updated_record(record, &plan.assigned)?;
                let next_bytes = estimated_record_bytes(&StoredRecord::new(
                    &schema,
                    &plan.layout,
                    &old_key,
                    &next,
                )?)?;
                (
                    UpdatedRow::Record(next),
                    next_bytes,
                    estimated_key_bytes(record)?,
                )
            }
            (map, _) => {
                let mut new_row = map.expect("a row not read as a record is a map");
                let old_primary_key = primary_key_row(&schema, &new_row)?;
                for (column, value) in &resolved_assignments {
                    new_row.insert(column.clone(), value.clone());
                }
                let new_row = normalize_row(&schema, new_row)?;
                validate_primary_storage_key_bound(&schema, &new_row)?;
                let new_key = encode_primary_key(&schema, &new_row)?;
                let (next_bytes, key_bytes) = (
                    estimated_row_bytes(&new_row)?,
                    estimated_row_bytes(&old_primary_key)?,
                );
                (
                    UpdatedRow::Map {
                        old_primary_key,
                        new_key,
                        new_row,
                    },
                    next_bytes,
                    key_bytes,
                )
            }
        };
        let update = PlannedUpdate {
            old_key,
            old_row,
            next,
        };
        let new_key = update.new_key();
        work_bytes = checked_dml_add(work_bytes, next_bytes)?;
        work_bytes = checked_dml_add(work_bytes, key_bytes)?;
        work_bytes = checked_dml_add(
            work_bytes,
            checked_dml_add(update.old_key.len(), checked_dml_add(new_key.len(), 192)?)?,
        )?;
        ensure_dml_work_bytes(work_bytes)?;
        work_bytes = retain_dml_change(work_bytes, table)?;
        if update.old_key != new_key {
            work_bytes = retain_dml_change(work_bytes, table)?;
        }

        if let (Some(columns), UpdatedRow::Map { new_row, .. }) = (returning, &update.next) {
            result_bytes = retain_returned_row(result_bytes, new_row, columns)?;
            returned.push(project_returning_row(new_row, columns, table)?);
        }
        updates.push(update);
        Ok(VisitControl::Continue)
    })?;
    require_complete_dml_scan(visit_outcome, table)?;

    let row_count = updates.len();
    // Encoded keys are canonical, so a spelling-only FLOAT update such as `0` to `-0.0` keeps its
    // key, and a row moving to a key that another row holds, and that this statement leaves in
    // place, is a collision. Rows keeping their keys, each visited once, collide with none.
    if updates
        .iter()
        .any(|update| update.new_key() != update.old_key)
    {
        let mut old_keys = KeySet::with_capacity_and_hasher(row_count, Default::default());
        for update in &updates {
            old_keys.insert(update.old_key.clone());
        }
        let mut destinations = KeySet::with_capacity_and_hasher(row_count, Default::default());
        for update in &updates {
            let new_key = update.new_key();
            if !destinations.insert(new_key.to_vec()) {
                return Err(duplicate_primary_key("UPDATE of", table));
            }
            // New keys are distinct, so adding one reports whether no row held it before.
            if let UpdatedRow::Map { new_row, .. } = &update.next
                && new_key != update.old_key
                && old_keys.insert(new_key.to_vec())
                && storage.visit_primary_key(table, new_row, &mut |_| Ok(VisitControl::Stop))?
                    == VisitOutcome::Stopped
            {
                return Err(duplicate_primary_key("UPDATE of", table));
            }
        }
    }

    // A row keeping its key replaces the row planning read there. A row moving to a new key is
    // deleted from its old one, and what its new key holds is left for the writer to read.
    let (changes, previous) = if row_count > 0 {
        let mut deletes = Vec::with_capacity(row_count);
        let mut deleted = Vec::with_capacity(row_count);
        let mut upserts = Vec::with_capacity(row_count);
        let mut replaced = Vec::with_capacity(row_count);
        for update in updates {
            let old_row = update
                .old_row
                .map_or(PreviousRow::Unread, |row| PreviousRow::Read(Some(row)));
            let (old_primary_key, new_key, new_row) = match update.next {
                UpdatedRow::Record(record) => {
                    replaced.push(old_row);
                    upserts.push(RowChange::Put {
                        table: table.to_owned(),
                        key: update.old_key,
                        record,
                    });
                    continue;
                }
                UpdatedRow::Map {
                    old_primary_key,
                    new_key,
                    new_row,
                } => (old_primary_key, new_key, new_row),
            };
            if update.old_key != new_key {
                deletes.push(RowChange::Delete {
                    table: table.to_owned(),
                    key: old_primary_key,
                });
                deleted.push(old_row);
                replaced.push(PreviousRow::Unread);
            } else {
                replaced.push(old_row);
            }
            upserts.push(RowChange::Upsert {
                table: table.to_owned(),
                row: new_row,
            });
        }
        deletes.extend(upserts);
        deleted.extend(replaced);
        (deletes, deleted)
    } else {
        (Vec::new(), Vec::new())
    };
    Ok(PlannedDml {
        outcome: WriteOutcome {
            command: "UPDATE",
            row_count,
            rows: returned,
            tables: (row_count > 0)
                .then(|| table.to_owned())
                .into_iter()
                .collect(),
            mutated: row_count > 0,
        },
        changes,
        previous,
    })
}

fn plan_delete(
    storage: &dyn StorageReader,
    table: &str,
    predicate: Option<&Predicate>,
    returning: Option<&[String]>,
) -> Result<PlannedDml> {
    let schema = storage.table_schema(table)?;
    if let Some(predicate) = predicate {
        validate_predicate_columns(predicate, &schema, table)?;
        validate_predicate_types(predicate, &schema, table)?;
    }
    validate_projection(&schema, returning)?;

    let mut changes = Vec::new();
    let mut previous = Vec::new();
    let mut kept = KeptRows::default();
    let mut returned = Vec::new();
    let mut scanned = 0usize;
    let mut work_bytes = 0usize;
    let mut result_bytes = 0usize;
    let filter = Filter::new(predicate, &schema, table)?;
    // A script's writer lets a delete plan each stored row by its key, without a map.
    let by_key = storage.plans_removals();
    let visit_outcome = visit_dml_candidates(storage, &schema, predicate, &mut |row| {
        scanned = scanned.saturating_add(1);
        if scanned > MAX_DML_SCAN_ROWS {
            return Err(dml_limit_error(format!(
                "A data-modification statement cannot scan more than {MAX_DML_SCAN_ROWS} rows"
            )));
        }
        if !filter.matches(row)? {
            return Ok(VisitControl::Continue);
        }
        if changes.len() == MAX_DML_CHANGED_ROWS {
            return Err(dml_limit_error(format!(
                "A data-modification statement cannot change more than {MAX_DML_CHANGED_ROWS} rows"
            )));
        }
        // A delete needs only the row's key; the writer reads the rest from the row it keeps.
        let (change, charge) = match row.stored().filter(|_| by_key) {
            // Charged as the map of its key columns, which is what its key's estimate is.
            Some(record) => (
                RowChange::Remove {
                    table: table.to_owned(),
                    key: record.key().to_vec(),
                },
                checked_dml_add(estimated_key_bytes(record)?, 96)?,
            ),
            None => {
                let key = row.primary_key()?;
                let charge = delete_charge(&schema, &key)?;
                (
                    RowChange::Delete {
                        table: table.to_owned(),
                        key,
                    },
                    charge,
                )
            }
        };
        ensure_dml_work_bytes(checked_dml_add(work_bytes, charge)?)?;
        work_bytes = retain_dml_change(work_bytes, table)?;
        if let Some(columns) = returning {
            let row = row.to_row()?;
            result_bytes = retain_returned_row(result_bytes, &row, columns)?;
            returned.push(project_returning_row(&row, columns, table)?);
        }
        work_bytes = checked_dml_add(work_bytes, charge)?;
        changes.push(change);
        previous.push(if kept.fits(row.held_bytes()?) {
            PreviousRow::Read(Some(row.hold()?))
        } else {
            PreviousRow::Unread
        });
        Ok(VisitControl::Continue)
    })?;
    require_complete_dml_scan(visit_outcome, table)?;
    let row_count = changes.len();
    Ok(PlannedDml {
        outcome: WriteOutcome {
            command: "DELETE",
            row_count,
            rows: returned,
            tables: (row_count > 0)
                .then(|| table.to_owned())
                .into_iter()
                .collect(),
            mutated: row_count > 0,
        },
        changes,
        previous,
    })
}

/// What planning a delete of the row whose key is `key` retains, in bytes.
fn delete_charge(schema: &TableDefinition, key: &Row) -> Result<usize> {
    let mut charge = 32usize;
    for column in &schema.primary_key {
        let value = key.get(column).ok_or_else(|| {
            EngineError::invalid_change(format!(
                "Row for `{}` is missing primary-key column `{column}`",
                schema.name
            ))
        })?;
        // The key was read from a stored row, within the limits on stored values.
        charge = checked_dml_add(charge, 64)?;
        charge = checked_dml_add(charge, checked_dml_mul(column.len(), 2)?)?;
        charge = checked_dml_add(
            charge,
            checked_dml_mul(estimated_checked_value_bytes(value)?, 2)?,
        )?;
    }
    checked_dml_add(charge, 96)
}

/// Predicate and assignment validation precedes this lookup, and the caller still checks the
/// complete predicate. Candidates are narrowed as a query's are, except that a primary key too large
/// to store is scanned for rather than looked up.
fn visit_dml_candidates(
    storage: &dyn StorageReader,
    schema: &TableDefinition,
    predicate: Option<&Predicate>,
    visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
) -> Result<VisitOutcome> {
    // A valid predicate can name a key too large to store; keep its scan behavior.
    if let Some(values) = exact_equalities(predicate, schema, &schema.primary_key)
        && primary_key_values_fit(&values)
    {
        return storage.visit_primary_key_values(&schema.name, schema, &values, visitor);
    }
    visit_indexed_candidates(
        storage,
        &schema.name,
        predicate,
        schema,
        crate::storage::KeyOrder::Ascending,
        visitor,
    )
}

fn validate_projection(schema: &TableDefinition, returning: Option<&[String]>) -> Result<()> {
    if let Some(columns) = returning {
        validate_named_columns(schema, columns)?;
    }
    Ok(())
}

fn column_default(schema: &TableDefinition, name: &str) -> Result<Value> {
    schema
        .columns
        .iter()
        .find(|column| column.name == name)
        .map(|column| column.default.clone().unwrap_or(Value::Null))
        .ok_or_else(|| EngineError::column_not_found(name, &schema.name))
}

pub(crate) fn primary_key_row(schema: &TableDefinition, row: &Row) -> Result<Row> {
    let mut key = Map::new();
    for column in &schema.primary_key {
        let value = row.get(column).ok_or_else(|| {
            EngineError::invalid_change(format!(
                "Row for `{}` is missing primary-key column `{column}`",
                schema.name
            ))
        })?;
        key.insert(column.clone(), value.clone());
    }
    Ok(key)
}

/// What planning an INSERT's rows straight into records needs, found once per statement.
struct RecordPlan {
    layout: Rc<RecordLayout>,
    /// The JSON text a row takes apart from its values.
    json_overhead: usize,
}

impl RecordPlan {
    fn new(schema: &TableDefinition, layout: Rc<RecordLayout>) -> Result<Self> {
        Ok(Self {
            layout,
            json_overhead: row_json_overhead(schema)?,
        })
    }

    /// A row's values in schema order, each named value or its column's default, when every one
    /// is a scalar and the row's JSON text cannot pass the row limit, so that no check on a map
    /// of them could fail for its size. Any other row is planned as a map.
    fn values<'a>(
        &self,
        schema: &'a TableDefinition,
        positions: &[Option<usize>],
        values: &'a [SqlValue],
        default_values: bool,
    ) -> Option<Vec<&'a Value>> {
        let mut row = Vec::with_capacity(schema.columns.len());
        let mut bound = self.json_overhead;
        for (column, position) in schema.columns.iter().zip(positions) {
            let explicit = (!default_values)
                .then(|| position.and_then(|index| values.get(index)))
                .flatten();
            let value = match explicit {
                Some(SqlValue::Value(value)) => value,
                Some(SqlValue::Default) | None => column.default.as_ref().unwrap_or(&Value::Null),
            };
            bound = bound.saturating_add(json_scalar_bound(value)?);
            row.push(value);
        }
        (bound <= MAX_LOGICAL_ROW_BYTES).then_some(row)
    }
}

/// What rewriting an UPDATE's rows straight into records needs, found once per statement: a
/// reader with record layouts, assignments that leave every row's key in place, and no RETURNING,
/// whose rows are maps. The assigned values were checked as normalizing a map of them checks them.
struct RecordUpdate<'a> {
    layout: Rc<RecordLayout>,
    /// Each column's assigned value, by schema position.
    assigned: Vec<Option<&'a Value>>,
    /// The most JSON text a row can take apart from its stored values: its punctuation and names,
    /// the assigned values, and the defaults of the columns its record may omit.
    bound: usize,
}

impl<'a> RecordUpdate<'a> {
    fn new(
        storage: &dyn StorageReader,
        schema: &TableDefinition,
        assignments: &'a [(String, Value)],
        returning: Option<&[String]>,
    ) -> Result<Option<Self>> {
        let Some(layout) = storage
            .record_layout(&schema.name)
            .filter(|_| returning.is_none())
        else {
            return Ok(None);
        };
        let mut assigned = vec![None; schema.columns.len()];
        for (name, value) in assignments {
            match schema
                .columns
                .iter()
                .position(|column| column.name == *name)
            {
                Some(position) if !schema.primary_key.contains(name) => {
                    assigned[position] = Some(value);
                }
                _ => return Ok(None),
            }
        }
        let mut bound = row_json_overhead(schema)?;
        for (column, value) in schema.columns.iter().zip(&assigned) {
            // Only encoding a JSON value a row keeps measures it, which a map would do.
            let value_bound = match value {
                Some(value) => json_scalar_bound(value),
                None if column.data_type == ColumnType::Json => None,
                None => json_scalar_bound(column.default.as_ref().unwrap_or(&Value::Null)),
            };
            let Some(value_bound) = value_bound else {
                return Ok(None);
            };
            bound = bound.saturating_add(value_bound);
        }
        Ok(Some(Self {
            layout,
            assigned,
            bound,
        }))
    }

    /// The stored record `row` is read from, when its JSON text, with the plan's values assigned,
    /// cannot pass the row limit, so that no check on a map of it could fail for its size. A stored
    /// value's JSON text takes at most six times its encoded bytes, as escaped text, and at most
    /// 25 as a number.
    fn record<'r, 's>(&self, row: &'r RowRef<'s>) -> Option<&'r StoredRecord<'s>> {
        row.stored().filter(|record| self.fits(record))
    }

    fn fits(&self, record: &StoredRecord<'_>) -> bool {
        let bound = record
            .entry_len()
            .saturating_mul(6)
            .saturating_add(self.assigned.len().saturating_mul(25))
            .saturating_add(self.bound);
        bound <= MAX_LOGICAL_ROW_BYTES
    }
}

/// A stored row's key, and the record an upsert rewrites it with, estimated at `bytes`, as the map
/// it replaces is.
struct RewrittenRecord {
    key: Vec<u8>,
    record: Vec<u8>,
    bytes: usize,
}

/// The record a lone upsert rewrites the stored row `held` with, keeping its key and the bytes of
/// every column `DO UPDATE SET` leaves. A row whose text could pass the row limit, a table keeping
/// a JSON column the statement does not assign, and an assignment to a key column are left to be
/// planned as maps.
fn rewritten_record(
    storage: &dyn StorageReader,
    schema: &TableDefinition,
    updates: &[(String, ResolvedConflictValue)],
    held: &HeldRow,
    proposed: &Row,
) -> Result<Option<RewrittenRecord>> {
    let HeldRow::Stored(entry) = held else {
        return Ok(None);
    };
    let mut assignments = Vec::with_capacity(updates.len());
    for (column, value) in updates {
        let value = match value {
            ResolvedConflictValue::Value(value) => value.clone(),
            ResolvedConflictValue::Excluded(source) => proposed
                .get(source)
                .cloned()
                .ok_or_else(|| EngineError::column_not_found(source, &schema.name))?,
        };
        assignments.push((column.clone(), value));
    }
    let Some(plan) = RecordUpdate::new(storage, schema, &assignments, None)? else {
        return Ok(None);
    };
    let record = StoredRecord::new(schema, &plan.layout, entry.key(), entry.value())?;
    if !plan.fits(&record) {
        return Ok(None);
    }
    // Checked in schema order, as normalizing a map of the row checks them: a proposed value was
    // checked for its own column, which need not be the one it is assigned to.
    for (definition, value) in schema.columns.iter().zip(&plan.assigned) {
        if let Some(value) = value {
            validate_value(definition, value, &schema.name)?;
        }
    }
    let next = encode_updated_record(&record, &plan.assigned)?;
    let bytes = estimated_record_bytes(&StoredRecord::new(
        schema,
        &plan.layout,
        entry.key(),
        &next,
    )?)?;
    Ok(Some(RewrittenRecord {
        key: entry.key().to_vec(),
        record: next,
        bytes,
    }))
}

/// The row `held` keeps, as a map.
fn held_row(schema: &TableDefinition, storage: &dyn StorageReader, held: &HeldRow) -> Result<Row> {
    match held {
        HeldRow::Map(row) => Ok(row.clone()),
        HeldRow::Stored(entry) => {
            let layout = storage
                .record_layout(&schema.name)
                .ok_or_else(unplanned_record)?;
            StoredRecord::new(schema, &layout, entry.key(), entry.value())?.to_row()
        }
    }
}

/// Where each of `schema`'s columns is among the named `columns`, which this validates as
/// [`validate_named_columns`] does.
fn named_column_positions(
    schema: &TableDefinition,
    columns: &[String],
) -> Result<Vec<Option<usize>>> {
    let mut positions = vec![None; schema.columns.len()];
    for (index, column) in columns.iter().enumerate() {
        let position = schema
            .columns
            .iter()
            .position(|definition| definition.name == *column)
            .ok_or_else(|| EngineError::column_not_found(column, &schema.name))?;
        // A repeated name is found here, since every earlier name was a column.
        if positions[position].replace(index).is_some() {
            return Err(EngineError::invalid_query(format!(
                "Column `{column}` is named more than once"
            )));
        }
    }
    Ok(positions)
}

/// The estimated bytes of the row an INSERT's `values` make, with each column's value at its
/// place in `positions`. Values are bound parameters or literals within the SQL text limit, which
/// the size checks on either already cover.
fn prospective_insert_row_bytes(
    schema: &TableDefinition,
    positions: &[Option<usize>],
    values: &[SqlValue],
    default_values: bool,
) -> Result<usize> {
    let mut bytes = 32usize;
    for (definition, position) in schema.columns.iter().zip(positions) {
        let explicit = (!default_values)
            .then(|| position.and_then(|index| values.get(index)))
            .flatten();
        let value = match explicit {
            Some(SqlValue::Value(value)) => value,
            Some(SqlValue::Default) | None => definition.default.as_ref().unwrap_or(&Value::Null),
        };
        bytes = checked_dml_add(bytes, 64)?;
        bytes = checked_dml_add(bytes, checked_dml_mul(definition.name.len(), 2)?)?;
        bytes = checked_dml_add(
            bytes,
            checked_dml_mul(estimated_checked_value_bytes(value)?, 2)?,
        )?;
    }
    Ok(bytes)
}

fn project_returning_row(row: &Row, columns: &[String], table: &str) -> Result<Row> {
    if columns.is_empty() {
        return Ok(row.clone());
    }
    let mut projected = Map::new();
    for column in columns {
        projected.insert(
            column.clone(),
            row.get(column)
                .ok_or_else(|| EngineError::column_not_found(column, table))?
                .clone(),
        );
    }
    Ok(projected)
}

fn retain_dml_row(current: usize, row: &Row) -> Result<usize> {
    let next = checked_dml_add(current, checked_dml_add(estimated_row_bytes(row)?, 96)?)?;
    ensure_dml_work_bytes(next)?;
    Ok(next)
}

fn retain_dml_change(current: usize, table: &str) -> Result<usize> {
    let charge = checked_dml_add(table.len(), DML_CHANGE_RETAINED_BYTES)?;
    let next = checked_dml_add(current, charge)?;
    ensure_dml_work_bytes(next)?;
    Ok(next)
}

fn retain_returned_row(current: usize, row: &Row, columns: &[String]) -> Result<usize> {
    let bytes = if columns.is_empty() {
        estimated_row_bytes(row)?
    } else {
        let mut bytes = 32usize;
        for column in columns {
            let value = row
                .get(column)
                .ok_or_else(|| EngineError::column_not_found(column, "RETURNING"))?;
            bytes = checked_dml_add(bytes, 64)?;
            bytes = checked_dml_add(bytes, checked_dml_mul(column.len(), 2)?)?;
            bytes = checked_dml_add(bytes, checked_dml_mul(estimated_value_bytes(value)?, 2)?)?;
        }
        bytes
    };
    let next = checked_dml_add(current, bytes)?;
    if next > MAX_DML_RESULT_BYTES {
        return Err(dml_limit_error(format!(
            "A data-modification statement cannot materialize more than {MAX_DML_RESULT_BYTES} bytes of RETURNING results"
        )));
    }
    Ok(next)
}

fn checked_dml_add(left: usize, right: usize) -> Result<usize> {
    left.checked_add(right).ok_or_else(dml_overflow_error)
}

fn checked_dml_mul(left: usize, right: usize) -> Result<usize> {
    left.checked_mul(right).ok_or_else(dml_overflow_error)
}

fn ensure_dml_work_bytes(bytes: usize) -> Result<()> {
    if bytes > MAX_DML_WORK_BYTES {
        Err(dml_limit_error(format!(
            "A data-modification statement cannot retain more than {MAX_DML_WORK_BYTES} bytes of working state"
        )))
    } else {
        Ok(())
    }
}

/// The failure of a statement, `INSERT into` or `UPDATE of`, that would leave two rows of `table`
/// with one primary key.
fn duplicate_primary_key(statement: &str, table: &str) -> EngineError {
    EngineError::constraint_violation(format!(
        "{statement} `{table}` would duplicate a primary key"
    ))
}

fn dml_overflow_error() -> EngineError {
    dml_limit_error("A data-modification statement size overflowed".to_owned())
}

fn dml_limit_error(message: String) -> EngineError {
    EngineError::new("RESOURCE_LIMIT", message)
}

fn require_complete_dml_scan(outcome: VisitOutcome, table: &str) -> Result<()> {
    match outcome {
        VisitOutcome::Complete => Ok(()),
        VisitOutcome::Stopped => Err(EngineError::new(
            "STORAGE_CORRUPT",
            format!("The storage visitor for `{table}` stopped without being asked"),
        )),
    }
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
            self.parse_create()?
        } else if self.consume_keyword("insert") {
            self.parse_insert()?
        } else if self.consume_keyword("update") {
            self.parse_update()?
        } else if self.consume_keyword("delete") {
            self.parse_delete()?
        } else if self.consume_keyword("drop") {
            self.parse_drop()?
        } else if self.consume_keyword("alter") {
            self.parse_alter_table()?
        } else {
            return Err(unsupported_statement());
        };
        self.consume(TokenMatcher::Semicolon);
        if !self.is_done() {
            return Err(unsupported_statement());
        }
        Ok(statement)
    }

    fn parse_create(&mut self) -> Result<WriteStatement> {
        if self.consume_keyword("table") {
            return self.parse_create_table();
        }
        let unique = self.consume_keyword("unique");
        self.expect_keyword("index")?;
        self.parse_create_index(unique)
    }

    fn parse_create_table(&mut self) -> Result<WriteStatement> {
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
            schema: TableDefinition {
                name,
                primary_key,
                columns,
            },
            if_not_exists,
        })
    }

    fn parse_create_index(&mut self, unique: bool) -> Result<WriteStatement> {
        let if_not_exists = if self.consume_keyword("if") {
            self.expect_keyword("not")?;
            self.expect_keyword("exists")?;
            true
        } else {
            false
        };
        let name = self.parse_identifier()?;
        self.expect_keyword("on")?;
        let table = self.parse_table_name()?;
        self.expect(TokenMatcher::LParen, "Expected `(` after indexed table")?;
        let columns = self.parse_identifier_list(TokenMatcher::RParen)?;
        self.expect(TokenMatcher::RParen, "Expected `)` after indexed columns")?;
        Ok(WriteStatement::CreateIndex {
            definition: crate::IndexDefinition {
                name,
                table,
                columns,
                unique,
            },
            if_not_exists,
        })
    }

    fn parse_drop(&mut self) -> Result<WriteStatement> {
        let table = if self.consume_keyword("table") {
            true
        } else {
            self.expect_keyword("index")?;
            false
        };
        let if_exists = if self.consume_keyword("if") {
            self.expect_keyword("exists")?;
            true
        } else {
            false
        };
        if table {
            Ok(WriteStatement::DropTable {
                table: self.parse_table_name()?,
                if_exists,
            })
        } else {
            Ok(WriteStatement::DropIndex {
                name: self.parse_identifier()?,
                if_exists,
            })
        }
    }

    fn parse_alter_table(&mut self) -> Result<WriteStatement> {
        self.expect_keyword("table")?;
        let table = self.parse_table_name()?;
        self.expect_keyword("add")?;
        self.consume_keyword("column");
        let if_not_exists = if self.consume_keyword("if") {
            self.expect_keyword("not")?;
            self.expect_keyword("exists")?;
            true
        } else {
            false
        };
        let (column, primary_key) = self.parse_column_definition()?;
        if primary_key {
            return Err(EngineError::unsupported_sql(
                "ALTER TABLE ADD COLUMN cannot add a primary key",
            ));
        }
        Ok(WriteStatement::AddColumn {
            table,
            column,
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
        let on_conflict = if self.consume_keyword("on") {
            Some(self.parse_on_conflict()?)
        } else {
            None
        };
        let returning = self.parse_returning()?;
        Ok(WriteStatement::Insert {
            table,
            columns,
            values,
            on_conflict,
            returning,
        })
    }

    fn parse_on_conflict(&mut self) -> Result<OnConflict> {
        self.expect_keyword("conflict")?;
        if self.consume_keyword("on") {
            return Err(EngineError::unsupported_sql(
                "ON CONFLICT ON CONSTRAINT is not supported; name the conflict target columns",
            ));
        }
        let target = if self.consume(TokenMatcher::LParen) {
            let columns = self.parse_identifier_list(TokenMatcher::RParen)?;
            self.expect(
                TokenMatcher::RParen,
                "Expected `)` after ON CONFLICT columns",
            )?;
            Some(columns)
        } else {
            None
        };
        if self.consume_keyword("where") {
            return Err(EngineError::unsupported_sql(
                "ON CONFLICT does not support a partial-index WHERE clause",
            ));
        }
        self.expect_keyword("do")?;
        if self.consume_keyword("nothing") {
            return Ok(OnConflict {
                target,
                action: ConflictAction::Nothing,
            });
        }
        self.expect_keyword("update")?;
        if target.is_none() {
            return Err(EngineError::invalid_query(
                "ON CONFLICT DO UPDATE requires a conflict target such as `ON CONFLICT (id)`",
            ));
        }
        self.expect_keyword("set")?;
        let mut assignments = Vec::new();
        loop {
            if assignments.len() >= MAX_COLUMNS {
                return Err(EngineError::invalid_query(format!(
                    "ON CONFLICT DO UPDATE cannot assign more than {MAX_COLUMNS} columns"
                )));
            }
            let column = self.parse_identifier()?;
            self.expect(
                TokenMatcher::Eq,
                "Expected `=` in ON CONFLICT DO UPDATE assignment",
            )?;
            let value = if self.consume_excluded_qualifier() {
                ConflictValue::Excluded(self.parse_identifier()?)
            } else {
                ConflictValue::Value(self.parse_sql_value(true)?)
            };
            assignments.push((column, value));
            if !self.consume(TokenMatcher::Comma) {
                break;
            }
        }
        if self.consume_keyword("where") {
            return Err(EngineError::unsupported_sql(
                "ON CONFLICT DO UPDATE does not support a WHERE clause",
            ));
        }
        Ok(OnConflict {
            target,
            action: ConflictAction::Update(assignments),
        })
    }

    /// Consumes `EXCLUDED.`, the qualifier naming the row proposed for insertion.
    fn consume_excluded_qualifier(&mut self) -> bool {
        let excluded = matches!(
            self.tokens.get(self.position),
            Some(Token::Identifier { value, quoted })
                if (*quoted && value == "excluded")
                    || (!*quoted && value.eq_ignore_ascii_case("excluded"))
        ) && matches!(self.tokens.get(self.position + 1), Some(Token::Dot));
        if excluded {
            self.position += 2;
        }
        excluded
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
        if matches!(self.tokens.get(self.position), Some(Token::Placeholder(_))) {
            return Err(EngineError::unsupported_sql(
                "Column DEFAULT values must be literal constants, not parameters",
            ));
        }
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
            Token::Number(value) => number_literal(&value)
                .map(Value::Number)
                .map(SqlValue::Value)
                .ok_or_else(|| EngineError::invalid_query(format!("Invalid number `{value}`"))),
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
        "Supported statements are SELECT, CREATE TABLE, CREATE INDEX, ALTER TABLE ADD COLUMN, DROP TABLE, DROP INDEX, INSERT, UPDATE, and DELETE",
    )
}

fn unsupported_expression() -> EngineError {
    EngineError::unsupported_sql(
        "This SQL subset supports literals, parameters, NULL, booleans, and DEFAULT values",
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{InMemoryStorage, IndexDefinition, StorageReader};

    struct UnexpectedStopStorage {
        inner: InMemoryStorage,
        mutation_calls: usize,
    }

    impl StorageReader for UnexpectedStopStorage {
        fn visit_table(
            &self,
            _table: &str,
            _visitor: &mut dyn FnMut(&crate::row::RowRef<'_>) -> Result<VisitControl>,
        ) -> Result<VisitOutcome> {
            Ok(VisitOutcome::Stopped)
        }

        fn table_row_count(&self, table: &str) -> Result<usize> {
            self.inner.table_row_count(table)
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

    impl StorageDriver for UnexpectedStopStorage {
        fn define_table(&mut self, schema: TableDefinition) -> Result<()> {
            self.mutation_calls += 1;
            self.inner.define_table(schema)
        }

        fn drop_table(&mut self, table: &str) -> Result<()> {
            self.mutation_calls += 1;
            self.inner.drop_table(table)
        }

        fn add_column(&mut self, table: &str, column: ColumnDefinition) -> Result<()> {
            self.mutation_calls += 1;
            self.inner.add_column(table, column)
        }

        fn define_index(&mut self, definition: IndexDefinition) -> Result<()> {
            self.mutation_calls += 1;
            self.inner.define_index(definition)
        }

        fn drop_index(&mut self, name: &str) -> Result<()> {
            self.mutation_calls += 1;
            self.inner.drop_index(name)
        }

        fn apply_row_changes_unrevisioned(&mut self, changes: Vec<RowChange>) -> Result<()> {
            self.mutation_calls += 1;
            self.inner.apply_row_changes_unrevisioned(changes)
        }

        fn advance_revision(&mut self) -> Result<u64> {
            self.mutation_calls += 1;
            self.inner.advance_revision()
        }
    }

    fn row(value: Value) -> Row {
        value
            .as_object()
            .expect("test row must be an object")
            .clone()
    }

    fn seed_rows(storage: &mut InMemoryStorage, table: &str, rows: Vec<Row>) {
        for row in rows {
            storage
                .apply_row_changes_unrevisioned(vec![RowChange::Upsert {
                    table: table.to_owned(),
                    row,
                }])
                .unwrap();
        }
        storage.advance_revision().unwrap();
    }

    fn nested_json(depth: usize) -> Value {
        (0..depth).fold(Value::Null, |value, _| Value::Array(vec![value]))
    }

    fn storage() -> InMemoryStorage {
        let mut storage = InMemoryStorage::default();
        storage
            .define_table(TableDefinition {
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
                        name: "value".to_owned(),
                        data_type: ColumnType::Text,
                        nullable: false,
                        default: None,
                    },
                ],
            })
            .unwrap();
        storage
    }

    fn execute_sql(storage: &mut InMemoryStorage, sql: &str, params: &[Value]) -> WriteOutcome {
        let Statement::Write(statement) = parse(sql, params).unwrap() else {
            panic!("test SQL must be a write statement");
        };
        execute(storage, &statement).unwrap().0
    }

    #[test]
    fn dml_uses_exact_primary_key_lookups_for_insert_update_and_delete() {
        let mut storage = storage();

        execute_sql(
            &mut storage,
            "INSERT INTO items (id, value) VALUES (1, 'one'), (2, 'two')",
            &[],
        );
        assert_eq!(storage.access_counts(), (0, 2));
        assert_eq!(storage.visitor_counts(), (0, 0));

        let before_access = storage.access_counts();
        let before_visitors = storage.visitor_counts();
        let update = execute_sql(
            &mut storage,
            "UPDATE items SET value = 'changed' WHERE id = 2 RETURNING value",
            &[],
        );
        assert_eq!(update.row_count, 1);
        assert_eq!(update.rows, vec![row(json!({"value": "changed"}))]);
        assert_eq!(
            storage.access_counts(),
            (before_access.0, before_access.1 + 1)
        );
        assert_eq!(storage.visitor_counts(), before_visitors);

        let before_access = storage.access_counts();
        let before_visitors = storage.visitor_counts();
        let delete = execute_sql(
            &mut storage,
            "DELETE FROM items WHERE id = 1 RETURNING value",
            &[],
        );
        assert_eq!(delete.row_count, 1);
        assert_eq!(delete.rows, vec![row(json!({"value": "one"}))]);
        assert_eq!(
            storage.access_counts(),
            (before_access.0, before_access.1 + 1)
        );
        assert_eq!(storage.visitor_counts(), before_visitors);
    }

    #[test]
    fn keyed_dml_preserves_residual_filters_and_scan_fallbacks() {
        for command in ["UPDATE items SET value = 'changed'", "DELETE FROM items"] {
            for (predicate, expected_rows, expected_access) in [
                ("id = 1 AND value = 'one'", 1, (0, 1)),
                ("id = 1 AND value = 'two'", 0, (0, 1)),
                ("id = 999", 0, (0, 1)),
                ("(id = 1 AND value = 'one') AND id = 2", 0, (0, 1)),
                ("id = 1 OR id = 2", 2, (1, 0)),
                ("id = 1.0", 1, (1, 0)),
                // A comparison with NULL matches no row, so nothing is read.
                ("id = NULL", 0, (0, 0)),
                ("value = 'one'", 1, (1, 0)),
            ] {
                let mut storage = storage();
                seed_rows(
                    &mut storage,
                    "items",
                    vec![
                        row(json!({"id": 1, "value": "one"})),
                        row(json!({"id": 2, "value": "two"})),
                    ],
                );
                storage.reset_counts();
                let sql = format!("{command} WHERE {predicate} RETURNING id");
                let outcome = execute_sql(&mut storage, &sql, &[]);
                assert_eq!(outcome.row_count, expected_rows, "{sql}");
                assert_eq!(outcome.rows.len(), expected_rows, "{sql}");
                assert_eq!(storage.access_counts(), expected_access, "{sql}");
                assert_eq!(
                    storage.visitor_counts(),
                    (expected_access.0 * 2, 0),
                    "{sql}"
                );
            }
        }
    }

    #[test]
    fn keyed_dml_validates_the_entire_statement_before_a_missing_key_lookup() {
        for (sql, code) in [
            (
                "UPDATE items SET value = 'changed' WHERE value > 1 AND id = 999",
                "TYPE_MISMATCH",
            ),
            (
                "DELETE FROM items WHERE value > 1 AND id = 999",
                "TYPE_MISMATCH",
            ),
            (
                "UPDATE items SET value = 'changed' WHERE id = '1' AND id = 999",
                "TYPE_MISMATCH",
            ),
            (
                "DELETE FROM items WHERE id = '1' AND id = 999",
                "TYPE_MISMATCH",
            ),
            (
                "UPDATE items SET id = 'bad' WHERE id = 999",
                "TYPE_MISMATCH",
            ),
            (
                "UPDATE items SET value = 'changed' WHERE id = 999 RETURNING missing",
                "COLUMN_NOT_FOUND",
            ),
            (
                "DELETE FROM items WHERE id = 999 RETURNING missing",
                "COLUMN_NOT_FOUND",
            ),
        ] {
            let mut storage = storage();
            let Statement::Write(statement) = parse(sql, &[]).unwrap() else {
                unreachable!()
            };
            let error = execute(&mut storage, &statement).unwrap_err();
            assert_eq!(error.code, code, "{sql}");
            assert_eq!(storage.access_counts(), (0, 0), "{sql}");
        }
    }

    #[test]
    fn keyed_dml_requires_every_composite_key_column_with_exact_values() {
        for command in ["UPDATE items SET value = 'changed'", "DELETE FROM items"] {
            for (predicate, expected_rows, expected_access) in [
                ("id = 1 AND active = true AND tenant = 'a'", 1, (0, 1)),
                ("id = 1 AND active = false AND tenant = 'a'", 0, (0, 1)),
                ("id = 1 AND tenant = 'a'", 1, (1, 0)),
                ("id = 1.0 AND active = true AND tenant = 'a'", 1, (1, 0)),
            ] {
                let mut storage = InMemoryStorage::default();
                execute_sql(
                    &mut storage,
                    "CREATE TABLE items (id INTEGER, active BOOLEAN, tenant TEXT, value TEXT, PRIMARY KEY (id, active, tenant))",
                    &[],
                );
                execute_sql(
                    &mut storage,
                    "INSERT INTO items VALUES (1, true, 'a', 'one'), (1, true, 'b', 'two')",
                    &[],
                );
                storage.reset_counts();
                let sql = format!("{command} WHERE {predicate} RETURNING tenant");
                let outcome = execute_sql(&mut storage, &sql, &[]);
                assert_eq!(outcome.row_count, expected_rows, "{sql}");
                assert_eq!(storage.access_counts(), expected_access, "{sql}");
                assert_eq!(
                    outcome.rows,
                    if expected_rows == 1 {
                        vec![row(json!({"tenant": "a"}))]
                    } else {
                        vec![]
                    },
                    "{sql}"
                );
            }
        }
    }

    #[test]
    fn keyed_dml_scans_when_a_valid_text_predicate_exceeds_the_storage_key_bound() {
        for command in ["UPDATE items SET value = 'changed'", "DELETE FROM items"] {
            for (schema, insert, predicate, parameter_bytes) in [
                (
                    "CREATE TABLE items (id TEXT PRIMARY KEY, value TEXT)",
                    "INSERT INTO items VALUES ('one', 'one')",
                    "id = $1",
                    crate::storage::MAX_STORAGE_KEY_BYTES,
                ),
                (
                    "CREATE TABLE items (tenant TEXT, id TEXT, value TEXT, PRIMARY KEY (tenant, id))",
                    "INSERT INTO items VALUES ('a', 'one', 'one')",
                    "tenant = $1 AND id = 'one'",
                    // The tenant alone fits; the complete encoded composite key does not.
                    crate::storage::MAX_STORAGE_KEY_BYTES - 3,
                ),
            ] {
                let mut storage = InMemoryStorage::default();
                execute_sql(&mut storage, schema, &[]);
                execute_sql(&mut storage, insert, &[]);
                storage.reset_counts();
                let sql = format!("{command} WHERE {predicate} RETURNING id");
                let outcome =
                    execute_sql(&mut storage, &sql, &[json!("x".repeat(parameter_bytes))]);
                assert_eq!(outcome.row_count, 0, "{sql}");
                assert!(outcome.rows.is_empty(), "{sql}");
                assert_eq!(storage.access_counts(), (1, 0), "{sql}");
            }
        }
    }

    #[test]
    fn keyed_dml_reads_staged_inserts_moves_and_deletes_and_reopens() {
        use crate::{MemoryPageDevice, PagedEngine};

        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine.exec_sql("CREATE TABLE items (id INTEGER PRIMARY KEY, value TEXT); CREATE UNIQUE INDEX items_value ON items (value); INSERT INTO items VALUES (1, 'one'), (2, 'two')").unwrap();
        engine.begin_transaction().unwrap();
        engine
            .execute_sql("INSERT INTO items VALUES (3, 'three')", &[])
            .unwrap();
        assert_eq!(
            engine
                .execute_sql(
                    "UPDATE items SET value = $1 WHERE id = $2 RETURNING value",
                    &[json!("staged"), json!(3)]
                )
                .unwrap()
                .rows,
            vec![row(json!({"value": "staged"}))]
        );
        engine
            .execute_sql("UPDATE items SET id = 4 WHERE id = 3", &[])
            .unwrap();
        assert_eq!(
            engine
                .execute_sql("DELETE FROM items WHERE id = 3", &[])
                .unwrap()
                .row_count,
            0
        );
        assert_eq!(
            engine
                .execute_sql("UPDATE items SET value = 'two' WHERE id = 4", &[])
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        assert_eq!(
            engine
                .execute_sql("UPDATE items SET id = 2 WHERE id = 4", &[])
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        assert_eq!(
            engine
                .execute_sql("DELETE FROM items WHERE id = 4 RETURNING value", &[])
                .unwrap()
                .rows,
            vec![row(json!({"value": "staged"}))]
        );
        engine
            .execute_sql(
                "UPDATE items SET value = 'committed update' WHERE id = 1",
                &[],
            )
            .unwrap();
        engine
            .execute_sql("DELETE FROM items WHERE id = 2", &[])
            .unwrap();
        assert_eq!(
            engine
                .execute_sql("UPDATE items SET value = 'deleted' WHERE id = 2", &[])
                .unwrap()
                .row_count,
            0
        );
        engine.commit_transaction().unwrap();

        let mut reopened = PagedEngine::open(engine.into_device()).unwrap();
        reopened.check().unwrap();
        let expected = vec![row(json!({"id": 1, "value": "committed update"}))];
        assert_eq!(
            reopened.query_sql("SELECT * FROM items", &[]).unwrap().rows,
            expected
        );
        reopened.begin_transaction().unwrap();
        reopened
            .execute_sql("DELETE FROM items WHERE id = 1", &[])
            .unwrap();
        reopened.rollback_transaction().unwrap();
        assert_eq!(
            reopened.query_sql("SELECT * FROM items", &[]).unwrap().rows,
            expected
        );
    }

    #[test]
    fn dml_work_and_returning_budgets_fail_before_cloning_large_visited_values() {
        let mut storage = storage();
        let huge = "x".repeat(crate::storage::MAX_LOGICAL_ROW_BYTES - 256);
        seed_rows(
            &mut storage,
            "items",
            (0..19)
                .map(|id| row(json!({"id": id, "value": huge})))
                .collect(),
        );
        let revision = storage.revision();

        let Statement::Write(update) = parse("UPDATE items SET id = 100", &[]).unwrap() else {
            unreachable!()
        };
        let error = match execute(&mut storage, &update) {
            Ok(_) => panic!("oversized UPDATE should fail"),
            Err(error) => error,
        };
        assert_eq!(error.code, "RESOURCE_LIMIT");
        assert_eq!(storage.revision(), revision);

        let Statement::Write(delete_returning) =
            parse("DELETE FROM items RETURNING value", &[]).unwrap()
        else {
            unreachable!()
        };
        let error = match execute(&mut storage, &delete_returning) {
            Ok(_) => panic!("oversized RETURNING should fail"),
            Err(error) => error,
        };
        assert_eq!(error.code, "RESOURCE_LIMIT");
        assert_eq!(storage.table_row_count("items").unwrap(), 19);
        assert_eq!(storage.revision(), revision);

        let deleted = execute_sql(&mut storage, "DELETE FROM items", &[]);
        assert_eq!(deleted.row_count, 19);
        assert_eq!(storage.table_row_count("items").unwrap(), 0);
        // Statement execution is deliberately unrevisioned; the Engine publishes after success.
        assert_eq!(storage.revision(), revision);
    }

    #[test]
    fn update_and_delete_fail_closed_when_a_storage_scan_stops_unexpectedly() {
        for sql in [
            "UPDATE items SET value = 'changed' WHERE id >= 1",
            "DELETE FROM items WHERE id >= 1",
        ] {
            let mut inner = storage();
            seed_rows(
                &mut inner,
                "items",
                vec![row(json!({"id": 1, "value": "unchanged"}))],
            );
            let revision = inner.revision();
            let mut storage = UnexpectedStopStorage {
                inner,
                mutation_calls: 0,
            };
            let Statement::Write(statement) = parse(sql, &[]).unwrap() else {
                unreachable!()
            };

            let error = execute(&mut storage, &statement).unwrap_err();

            assert_eq!(error.code, "STORAGE_CORRUPT");
            assert!(error.message.contains("stopped without being asked"));
            assert_eq!(storage.mutation_calls, 0);
            assert_eq!(storage.inner.revision(), revision);
            assert_eq!(
                storage
                    .inner
                    .lookup_primary_key("items", &row(json!({"id": 1})))
                    .unwrap(),
                Some(row(json!({"id": 1, "value": "unchanged"})))
            );
        }
    }

    #[test]
    fn deeply_nested_json_dml_fails_before_mutation() {
        let mut storage = InMemoryStorage::default();
        storage
            .define_table(TableDefinition {
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
                        nullable: false,
                        default: None,
                    },
                ],
            })
            .unwrap();
        let revision = storage.revision();
        let error = match parse(
            "INSERT INTO documents (id, payload) VALUES (1, $1)",
            &[nested_json(crate::storage::MAX_JSON_DEPTH + 1)],
        ) {
            Ok(_) => panic!("deeply nested SQL parameter should fail before binding"),
            Err(error) => error,
        };

        assert_eq!(error.code, "BIND_ERROR");
        assert!(error.message.contains("cannot nest more than 64 levels"));
        assert_eq!(storage.table_row_count("documents").unwrap(), 0);
        assert_eq!(storage.revision(), revision);
    }

    #[test]
    fn column_defaults_are_literal_constants_not_bound_parameters() {
        for sql in [
            "CREATE TABLE defaults (id INTEGER PRIMARY KEY, value TEXT DEFAULT $1)",
            "ALTER TABLE defaults ADD COLUMN value TEXT DEFAULT $1",
        ] {
            let error = match parse(sql, &[json!("dynamic")]) {
                Ok(_) => panic!("column DEFAULT parameter should be rejected: {sql}"),
                Err(error) => error,
            };

            assert_eq!(error.code, "UNSUPPORTED_SQL");
            assert_eq!(
                error.message,
                "Column DEFAULT values must be literal constants, not parameters"
            );
        }

        parse(
            "CREATE TABLE defaults (id INTEGER PRIMARY KEY, value TEXT DEFAULT 'constant')",
            &[],
        )
        .unwrap();
    }
}

#[cfg(test)]
#[path = "on_conflict_tests.rs"]
mod on_conflict_tests;
