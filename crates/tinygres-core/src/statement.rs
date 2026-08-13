use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use serde_json::{Map, Number, Value};

use crate::query::{
    Token, bind_parameter, is_reserved_keyword, matches_predicate, parse_predicate_at, tokenize,
    validate_predicate_columns, validate_sql_input,
};
use crate::storage::{estimated_row_bytes, estimated_value_bytes, normalize_row, row_key};
use crate::{
    Change, ColumnDefinition, ColumnType, EngineError, Predicate, QueryPlan, Result, Row,
    StorageDriver, StorageReader, TableSchema, VisitControl, VisitOutcome,
};

const MAX_COLUMNS: usize = 256;
const MAX_VALUE_ROWS: usize = 4096;
const MAX_DML_SCAN_ROWS: usize = 1_000_000;
const MAX_DML_CHANGED_ROWS: usize = 100_000;
const MAX_DML_WORK_BYTES: usize = 16 * 1024 * 1024;
const MAX_DML_RESULT_BYTES: usize = 16 * 1024 * 1024;
const DML_CHANGE_RETAINED_BYTES: usize = 96;

pub(crate) enum Statement {
    Select(QueryPlan),
    Aggregate(crate::aggregate::AggregatePlan),
    Join(crate::join::JoinPlan),
    Write(WriteStatement),
}

pub(crate) enum WriteStatement {
    CreateTable {
        schema: TableSchema,
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
    pub changes: Vec<Change>,
}

pub(crate) fn parse(sql: &str, params: &[Value]) -> Result<Statement> {
    validate_sql_input(sql, params)?;
    let tokens = tokenize(sql)?;
    if matches!(
        tokens.first(),
        Some(Token::Identifier {
            value,
            quoted: false,
        }) if value.eq_ignore_ascii_case("select")
    ) {
        if crate::join::is_join_select(&tokens) {
            return crate::join::parse_sql(sql, params).map(Statement::Join);
        }
        if crate::aggregate::is_aggregate_select(&tokens) {
            return crate::aggregate::parse_sql(sql, params).map(Statement::Aggregate);
        }
        return crate::query::parse_sql(sql, params).map(Statement::Select);
    }
    MutationParser::new(tokens, params)
        .parse()
        .map(Statement::Write)
}

pub(crate) fn execute<S: StorageDriver>(
    storage: &mut S,
    statement: &WriteStatement,
) -> Result<WriteOutcome> {
    match statement {
        WriteStatement::CreateTable {
            schema,
            if_not_exists,
        } => create_table(storage, schema, *if_not_exists),
        WriteStatement::CreateIndex {
            definition,
            if_not_exists,
        } => create_index(storage, definition, *if_not_exists),
        WriteStatement::DropTable { table, if_exists } => drop_table(storage, table, *if_exists),
        WriteStatement::DropIndex { name, if_exists } => drop_index(storage, name, *if_exists),
        WriteStatement::AddColumn {
            table,
            column,
            if_not_exists,
        } => add_column(storage, table, column, *if_not_exists),
        WriteStatement::Insert { .. }
        | WriteStatement::Update { .. }
        | WriteStatement::Delete { .. } => {
            let PlannedDml { outcome, changes } = plan_dml(storage, statement)?;
            storage.apply_row_changes_unrevisioned(changes)?;
            Ok(outcome)
        }
    }
}

/// Plans one SQL row mutation without modifying storage.
///
/// Both the in-memory and paged engines use this path so validation, resource limits, affected-row
/// selection, and `RETURNING` semantics cannot drift between their publication mechanisms.
pub(crate) fn plan_dml<S: StorageReader>(
    storage: &S,
    statement: &WriteStatement,
) -> Result<PlannedDml> {
    match statement {
        WriteStatement::Insert {
            table,
            columns,
            values,
            returning,
        } => plan_insert(
            storage,
            table,
            columns.as_deref(),
            values,
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

fn drop_table<S: StorageDriver>(
    storage: &mut S,
    table: &str,
    if_exists: bool,
) -> Result<WriteOutcome> {
    if storage.table_schema(table).is_err() {
        if if_exists {
            return Ok(WriteOutcome {
                command: "DROP TABLE",
                row_count: 0,
                rows: vec![],
                tables: vec![],
                mutated: false,
            });
        }
        return Err(EngineError::table_not_found(table));
    }
    storage.drop_table(table)?;
    Ok(WriteOutcome {
        command: "DROP TABLE",
        row_count: 0,
        rows: vec![],
        tables: vec![table.to_owned()],
        mutated: true,
    })
}

fn drop_index<S: StorageDriver>(
    storage: &mut S,
    name: &str,
    if_exists: bool,
) -> Result<WriteOutcome> {
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
    storage.drop_index(name)?;
    Ok(WriteOutcome {
        command: "DROP INDEX",
        row_count: 0,
        rows: vec![],
        tables: vec![definition.table],
        mutated: true,
    })
}

fn add_column<S: StorageDriver>(
    storage: &mut S,
    table: &str,
    column: &ColumnDefinition,
    if_not_exists: bool,
) -> Result<WriteOutcome> {
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
    storage.add_column(table, column.clone())?;
    Ok(WriteOutcome {
        command: "ALTER TABLE",
        row_count: 0,
        rows: vec![],
        tables: vec![table.to_owned()],
        mutated: true,
    })
}

fn create_index<S: StorageDriver>(
    storage: &mut S,
    definition: &crate::IndexDefinition,
    if_not_exists: bool,
) -> Result<WriteOutcome> {
    if storage.index_definition(&definition.name).is_some() {
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
    storage.define_index(definition.clone())?;
    Ok(WriteOutcome {
        command: "CREATE INDEX",
        row_count: 0,
        rows: vec![],
        tables: vec![definition.table.clone()],
        mutated: true,
    })
}

fn create_table<S: StorageDriver>(
    storage: &mut S,
    schema: &TableSchema,
    if_not_exists: bool,
) -> Result<WriteOutcome> {
    let outcome = plan_create_table(storage, schema, if_not_exists)?;
    if outcome.mutated {
        storage.define_table(schema.clone())?;
    }
    Ok(outcome)
}

pub(crate) fn plan_create_table<S: StorageReader>(
    storage: &S,
    schema: &TableSchema,
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

fn plan_insert<S: StorageReader>(
    storage: &S,
    table: &str,
    columns: Option<&[String]>,
    value_rows: &[Vec<SqlValue>],
    returning: Option<&[String]>,
) -> Result<PlannedDml> {
    let schema = storage.table_schema(table)?;
    let default_values =
        columns.is_none() && value_rows.len() == 1 && value_rows.first().is_some_and(Vec::is_empty);
    let columns = match columns {
        Some(columns) => columns.to_vec(),
        None if !schema.columns.is_empty() => schema
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect(),
        None => {
            return Err(EngineError::invalid_query(format!(
                "INSERT into untyped table `{table}` must include a column list"
            )));
        }
    };
    validate_named_columns(&schema, &columns)?;
    validate_projection(&schema, returning)?;

    let mut keys = HashSet::with_capacity(value_rows.len());
    let mut changes = Vec::with_capacity(value_rows.len());
    let mut returned = Vec::with_capacity(returning.map_or(0, |_| value_rows.len()));
    let mut work_bytes = 0usize;
    let mut result_bytes = 0usize;
    for values in value_rows {
        if !default_values && values.len() != columns.len() {
            return Err(EngineError::invalid_query(format!(
                "INSERT names {} columns but provides {} values",
                columns.len(),
                values.len()
            )));
        }
        let prospective_bytes =
            prospective_insert_row_bytes(&schema, &columns, values, default_values)?;
        let prospective_charge = checked_dml_add(
            checked_dml_mul(prospective_bytes, 2)?,
            checked_dml_add(table.len(), DML_CHANGE_RETAINED_BYTES + 160)?,
        )?;
        ensure_dml_work_bytes(checked_dml_add(work_bytes, prospective_charge)?)?;
        let mut row = Map::new();
        if !default_values {
            for (column, value) in columns.iter().zip(values) {
                if let SqlValue::Value(value) = value {
                    row.insert(column.clone(), value.clone());
                }
            }
        }
        let row = normalize_row(&schema, row)?;
        let key = row_key(&schema, &row)?;
        let key_charge = checked_dml_add(checked_dml_mul(key.len(), 2)?, 64)?;
        work_bytes = checked_dml_add(work_bytes, key_charge)?;
        ensure_dml_work_bytes(work_bytes)?;
        if !keys.insert(key) {
            return Err(EngineError::constraint_violation(format!(
                "INSERT into `{table}` would duplicate a primary key"
            )));
        }
        if storage.lookup_primary_key(table, &row)?.is_some() {
            return Err(EngineError::constraint_violation(format!(
                "INSERT into `{table}` would duplicate a primary key"
            )));
        }
        work_bytes = retain_dml_row(work_bytes, &row)?;
        work_bytes = retain_dml_change(work_bytes, table)?;
        if let Some(columns) = returning {
            result_bytes = retain_returned_row(result_bytes, &row, columns)?;
            returned.push(project_returning_row(&row, columns, table)?);
        }
        changes.push(Change::Upsert {
            table: table.to_owned(),
            row,
        });
    }

    let row_count = changes.len();
    Ok(PlannedDml {
        outcome: WriteOutcome {
            command: "INSERT",
            row_count,
            rows: returned,
            tables: vec![table.to_owned()],
            mutated: true,
        },
        changes,
    })
}

fn plan_update<S: StorageReader>(
    storage: &S,
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
    if let Some(predicate) = predicate.filter(|_| !schema.columns.is_empty()) {
        validate_predicate_columns(predicate, &schema, table)?;
    }
    validate_projection(&schema, returning)?;

    let assignment_bytes = assignments
        .iter()
        .try_fold(0usize, |bytes, (column, value)| {
            let value = match value {
                SqlValue::Value(value) => value,
                SqlValue::Default => schema
                    .columns
                    .iter()
                    .find(|definition| definition.name == *column)
                    .and_then(|definition| definition.default.as_ref())
                    .unwrap_or(&Value::Null),
            };
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
        old_key: String,
        old_primary_key: Row,
        new_key: String,
        new_row: Row,
    }

    let mut updates = Vec::new();
    let mut returned = Vec::new();
    let mut scanned = 0usize;
    let mut work_bytes = 0usize;
    let mut result_bytes = 0usize;
    let visit_outcome = storage.visit_table(table, &mut |row| {
        scanned = scanned.saturating_add(1);
        if scanned > MAX_DML_SCAN_ROWS {
            return Err(dml_limit_error(format!(
                "A data-modification statement cannot scan more than {MAX_DML_SCAN_ROWS} rows"
            )));
        }
        if !matches_predicate(row, predicate, table)? {
            return Ok(VisitControl::Continue);
        }
        if updates.len() == MAX_DML_CHANGED_ROWS {
            return Err(dml_limit_error(format!(
                "A data-modification statement cannot change more than {MAX_DML_CHANGED_ROWS} rows"
            )));
        }

        // Check the retained candidate budget before cloning the visited row or assignment values.
        let conservative_row_bytes = checked_dml_add(
            checked_dml_mul(estimated_row_bytes(row)?, 3)?,
            checked_dml_add(assignment_bytes, 256)?,
        )?;
        ensure_dml_work_bytes(checked_dml_add(work_bytes, conservative_row_bytes)?)?;

        let old_key = row_key(&schema, row)?;
        let old_primary_key = primary_key_row(&schema, row)?;
        let mut new_row = row.clone();
        for (column, value) in &resolved_assignments {
            new_row.insert(column.clone(), value.clone());
        }
        let new_row = normalize_row(&schema, new_row)?;
        let new_key = row_key(&schema, &new_row)?;
        work_bytes = checked_dml_add(work_bytes, estimated_row_bytes(&new_row)?)?;
        work_bytes = checked_dml_add(work_bytes, estimated_row_bytes(&old_primary_key)?)?;
        work_bytes = checked_dml_add(
            work_bytes,
            checked_dml_add(old_key.len(), checked_dml_add(new_key.len(), 192)?)?,
        )?;
        ensure_dml_work_bytes(work_bytes)?;
        work_bytes = retain_dml_change(work_bytes, table)?;
        if old_key != new_key {
            work_bytes = retain_dml_change(work_bytes, table)?;
        }

        if let Some(columns) = returning {
            result_bytes = retain_returned_row(result_bytes, &new_row, columns)?;
            returned.push(project_returning_row(&new_row, columns, table)?);
        }
        updates.push(PlannedUpdate {
            old_key,
            old_primary_key,
            new_key,
            new_row,
        });
        Ok(VisitControl::Continue)
    })?;
    require_complete_dml_scan(visit_outcome, table)?;

    let row_count = updates.len();
    let old_keys = updates
        .iter()
        .map(|update| update.old_key.as_str())
        .collect::<HashSet<_>>();
    let mut destinations = HashMap::with_capacity(row_count);
    for update in &updates {
        if destinations
            .insert(update.new_key.as_str(), update.old_key.as_str())
            .is_some()
        {
            return Err(EngineError::constraint_violation(format!(
                "UPDATE of `{table}` would duplicate a primary key"
            )));
        }
        if update.new_key != update.old_key
            && !old_keys.contains(update.new_key.as_str())
            && storage
                .lookup_primary_key(table, &update.new_row)?
                .is_some()
        {
            return Err(EngineError::constraint_violation(format!(
                "UPDATE of `{table}` would duplicate a primary key"
            )));
        }
    }

    let changes = if row_count > 0 {
        let mut deletes = Vec::with_capacity(row_count);
        let mut upserts = Vec::with_capacity(row_count);
        for update in updates {
            if update.old_key != update.new_key {
                deletes.push(Change::Delete {
                    table: table.to_owned(),
                    key: update.old_primary_key,
                });
            }
            upserts.push(Change::Upsert {
                table: table.to_owned(),
                row: update.new_row,
            });
        }
        deletes.extend(upserts);
        deletes
    } else {
        Vec::new()
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
    })
}

fn plan_delete<S: StorageReader>(
    storage: &S,
    table: &str,
    predicate: Option<&Predicate>,
    returning: Option<&[String]>,
) -> Result<PlannedDml> {
    let schema = storage.table_schema(table)?;
    if let Some(predicate) = predicate.filter(|_| !schema.columns.is_empty()) {
        validate_predicate_columns(predicate, &schema, table)?;
    }
    validate_projection(&schema, returning)?;

    let mut changes = Vec::new();
    let mut returned = Vec::new();
    let mut scanned = 0usize;
    let mut work_bytes = 0usize;
    let mut result_bytes = 0usize;
    let visit_outcome = storage.visit_table(table, &mut |row| {
        scanned = scanned.saturating_add(1);
        if scanned > MAX_DML_SCAN_ROWS {
            return Err(dml_limit_error(format!(
                "A data-modification statement cannot scan more than {MAX_DML_SCAN_ROWS} rows"
            )));
        }
        if !matches_predicate(row, predicate, table)? {
            return Ok(VisitControl::Continue);
        }
        if changes.len() == MAX_DML_CHANGED_ROWS {
            return Err(dml_limit_error(format!(
                "A data-modification statement cannot change more than {MAX_DML_CHANGED_ROWS} rows"
            )));
        }

        let mut charge = 32usize;
        for column in &schema.primary_key {
            let value = row.get(column).ok_or_else(|| {
                EngineError::invalid_change(format!(
                    "Row for `{}` is missing primary-key column `{column}`",
                    schema.name
                ))
            })?;
            charge = checked_dml_add(charge, 64)?;
            charge = checked_dml_add(charge, checked_dml_mul(column.len(), 2)?)?;
            charge = checked_dml_add(charge, checked_dml_mul(estimated_value_bytes(value)?, 2)?)?;
        }
        charge = checked_dml_add(charge, 96)?;
        ensure_dml_work_bytes(checked_dml_add(work_bytes, charge)?)?;
        work_bytes = retain_dml_change(work_bytes, table)?;
        if let Some(columns) = returning {
            result_bytes = retain_returned_row(result_bytes, row, columns)?;
            returned.push(project_returning_row(row, columns, table)?);
        }
        let key = primary_key_row(&schema, row)?;
        work_bytes = checked_dml_add(work_bytes, charge)?;
        changes.push(Change::Delete {
            table: table.to_owned(),
            key,
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
    })
}

fn validate_named_columns(schema: &TableSchema, columns: &[String]) -> Result<()> {
    let mut names = HashSet::with_capacity(columns.len());
    for column in columns {
        if !names.insert(column) {
            return Err(EngineError::invalid_query(format!(
                "Column `{column}` is named more than once"
            )));
        }
        if !schema.columns.is_empty()
            && !schema
                .columns
                .iter()
                .any(|definition| definition.name == *column)
        {
            return Err(EngineError::column_not_found(column, &schema.name));
        }
    }
    Ok(())
}

fn validate_projection(schema: &TableSchema, returning: Option<&[String]>) -> Result<()> {
    if schema.columns.is_empty() {
        return Ok(());
    }
    if let Some(columns) = returning {
        for column in columns {
            if !schema
                .columns
                .iter()
                .any(|definition| definition.name == *column)
            {
                return Err(EngineError::column_not_found(column, &schema.name));
            }
        }
    }
    Ok(())
}

fn column_default(schema: &TableSchema, name: &str) -> Result<Value> {
    if schema.columns.is_empty() {
        return Err(EngineError::unsupported_sql(
            "DEFAULT requires a typed table catalog",
        ));
    }
    schema
        .columns
        .iter()
        .find(|column| column.name == name)
        .map(|column| column.default.clone().unwrap_or(Value::Null))
        .ok_or_else(|| EngineError::column_not_found(name, &schema.name))
}

fn primary_key_row(schema: &TableSchema, row: &Row) -> Result<Row> {
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

fn prospective_insert_row_bytes(
    schema: &TableSchema,
    columns: &[String],
    values: &[SqlValue],
    default_values: bool,
) -> Result<usize> {
    let mut bytes = 32usize;
    if schema.columns.is_empty() {
        for (column, value) in columns.iter().zip(values) {
            let SqlValue::Value(value) = value else {
                continue;
            };
            bytes = checked_dml_add(bytes, 64)?;
            bytes = checked_dml_add(bytes, checked_dml_mul(column.len(), 2)?)?;
            bytes = checked_dml_add(bytes, checked_dml_mul(estimated_value_bytes(value)?, 2)?)?;
        }
        return Ok(bytes);
    }

    for definition in &schema.columns {
        let explicit = (!default_values)
            .then(|| {
                columns
                    .iter()
                    .position(|column| column == &definition.name)
                    .and_then(|index| values.get(index))
            })
            .flatten();
        let value = match explicit {
            Some(SqlValue::Value(value)) => value,
            Some(SqlValue::Default) | None => definition.default.as_ref().unwrap_or(&Value::Null),
        };
        bytes = checked_dml_add(bytes, 64)?;
        bytes = checked_dml_add(bytes, checked_dml_mul(definition.name.len(), 2)?)?;
        bytes = checked_dml_add(bytes, checked_dml_mul(estimated_value_bytes(value)?, 2)?)?;
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
            schema: TableSchema {
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
        let returning = self.parse_returning()?;
        Ok(WriteStatement::Insert {
            table,
            columns,
            values,
            returning,
        })
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
            Token::Number(value) => Number::from_str(&value)
                .map(Value::Number)
                .map(SqlValue::Value)
                .map_err(|_| EngineError::invalid_query(format!("Invalid number `{value}`"))),
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
    use crate::{ApplyOutcome, ChangeBatch, InMemoryStorage, IndexDefinition, StorageReader};

    struct UnexpectedStopStorage {
        inner: InMemoryStorage,
        mutation_calls: usize,
    }

    impl StorageReader for UnexpectedStopStorage {
        fn visit_table(
            &self,
            _table: &str,
            _visitor: &mut dyn FnMut(&Row) -> Result<VisitControl>,
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
            visitor: &mut dyn FnMut(&Row) -> Result<VisitControl>,
        ) -> Result<Option<VisitOutcome>> {
            self.inner.visit_index(table, columns, key, visitor)
        }

        fn table_schema(&self, table: &str) -> Result<TableSchema> {
            self.inner.table_schema(table)
        }

        fn revision(&self) -> u64 {
            self.inner.revision()
        }
    }

    impl StorageDriver for UnexpectedStopStorage {
        fn define_table(&mut self, schema: TableSchema) -> Result<()> {
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

        fn replace_table_snapshot(
            &mut self,
            schema: TableSchema,
            rows: Vec<Row>,
        ) -> Result<ApplyOutcome> {
            self.mutation_calls += 1;
            self.inner.replace_table_snapshot(schema, rows)
        }

        fn replace_table(&mut self, table: &str, rows: Vec<Row>) -> Result<ApplyOutcome> {
            self.mutation_calls += 1;
            self.inner.replace_table(table, rows)
        }

        fn apply_batch(&mut self, batch: &ChangeBatch) -> Result<ApplyOutcome> {
            self.mutation_calls += 1;
            self.inner.apply_batch(batch)
        }

        fn define_index(&mut self, definition: IndexDefinition) -> Result<()> {
            self.mutation_calls += 1;
            self.inner.define_index(definition)
        }

        fn drop_index(&mut self, name: &str) -> Result<()> {
            self.mutation_calls += 1;
            self.inner.drop_index(name)
        }

        fn apply_row_changes_unrevisioned(&mut self, changes: Vec<Change>) -> Result<()> {
            self.mutation_calls += 1;
            self.inner.apply_row_changes_unrevisioned(changes)
        }

        fn replace_table_unrevisioned(&mut self, table: &str, rows: Vec<Row>) -> Result<()> {
            self.mutation_calls += 1;
            self.inner.replace_table_unrevisioned(table, rows)
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

    fn nested_json(depth: usize) -> Value {
        (0..depth).fold(Value::Null, |value, _| Value::Array(vec![value]))
    }

    fn storage() -> InMemoryStorage {
        let mut storage = InMemoryStorage::default();
        storage
            .define_table(TableSchema {
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
        execute(storage, &statement).unwrap()
    }

    #[test]
    fn dml_uses_exact_insert_lookups_and_one_streaming_update_or_delete_scan() {
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
            (before_access.0 + 1, before_access.1)
        );
        assert_eq!(
            storage.visitor_counts(),
            (before_visitors.0 + 2, before_visitors.1)
        );

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
            (before_access.0 + 1, before_access.1)
        );
        assert_eq!(
            storage.visitor_counts(),
            (before_visitors.0 + 2, before_visitors.1)
        );
    }

    #[test]
    fn dml_work_and_returning_budgets_fail_before_cloning_large_visited_values() {
        let mut storage = storage();
        let huge = "x".repeat(crate::storage::MAX_LOGICAL_ROW_BYTES - 256);
        storage
            .replace_table(
                "items",
                (0..19)
                    .map(|id| row(json!({"id": id, "value": huge})))
                    .collect(),
            )
            .unwrap();
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
            "UPDATE items SET value = 'changed' WHERE id = 1",
            "DELETE FROM items WHERE id = 1",
        ] {
            let mut inner = storage();
            inner
                .replace_table("items", vec![row(json!({"id": 1, "value": "unchanged"}))])
                .unwrap();
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
}
