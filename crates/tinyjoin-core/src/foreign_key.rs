//! Foreign keys: that each reference names a row of the table it references, checked as each
//! statement ends, and what deleting or updating a referenced row does to the rows referencing it.

use std::rc::Rc;

use serde_json::Value;

use crate::{
    ComparisonOperator, EngineError, ForeignKeyAction, ForeignKeyDefinition, IndexDefinition,
    Predicate, Result, Row, RowChange, SelectPlan, StorageReader, TableDefinition,
    paged_codec::{EMPTY_RECORD, StoredRecord},
    row::HeldRow,
    statement::{PlannedDml, PreviousRow, change_table},
    storage::normalize_row,
};

/// A foreign key as the catalog keeps it: naming the table it references as the catalog does,
/// and that table's primary key when it named no columns. `indexes` are the unique indexes of
/// `table` itself, which a key referencing its own table may name.
pub(crate) fn resolve(
    storage: &dyn StorageReader,
    table: &TableDefinition,
    indexes: &[IndexDefinition],
    key: &ForeignKeyDefinition,
) -> Result<ForeignKeyDefinition> {
    let exists = |name: &str| name == table.name || storage.table_schema(name).is_ok();
    // PostgreSQL's tools qualify a reference with its default schema, `public`.
    let references = match key.references.strip_prefix("public.") {
        Some(name) if !exists(&key.references) && exists(name) => name,
        _ => key.references.as_str(),
    };
    let (parent, unique) = if references == table.name {
        (Rc::new(table.clone()), indexes.to_vec())
    } else {
        (
            storage.table_schema(references)?,
            storage.indexes_for_table(references)?,
        )
    };
    let referenced_columns = if key.referenced_columns.is_empty() {
        parent.primary_key.clone()
    } else {
        key.referenced_columns.clone()
    };
    let invalid = |message: &str| {
        EngineError::invalid_schema(format!("Foreign key `{}` {message}", key.name))
    };
    if key.columns.is_empty() || referenced_columns.len() != key.columns.len() {
        return Err(invalid("must reference as many columns as it has"));
    }
    for (position, (column, referenced)) in key.columns.iter().zip(&referenced_columns).enumerate()
    {
        if key.columns[..position].contains(column) {
            return Err(invalid("names a column more than once"));
        }
        let data_type = |schema: &TableDefinition, name: &str| {
            schema
                .columns
                .iter()
                .find(|existing| existing.name == name)
                .map(|existing| existing.data_type)
                .ok_or_else(|| EngineError::column_not_found(name, &schema.name))
        };
        if data_type(table, column)? != data_type(&parent, referenced)? {
            return Err(EngineError::type_mismatch(format!(
                "Foreign key `{}` compares `{column}` with `{referenced}`, of another type",
                key.name
            )));
        }
    }
    // The referenced columns must hold each value once: be a primary or unique key.
    let same = |columns: &[String]| {
        columns.len() == referenced_columns.len()
            && columns
                .iter()
                .all(|column| referenced_columns.contains(column))
    };
    if !same(&parent.primary_key)
        && !unique
            .iter()
            .any(|index| index.unique && same(&index.columns))
    {
        return Err(invalid(
            "must reference the primary key, or a unique index's columns",
        ));
    }
    Ok(ForeignKeyDefinition {
        references: references.to_owned(),
        referenced_columns,
        ..key.clone()
    })
}

/// The foreign keys that reference `table`, each with the table it belongs to.
pub(crate) fn referencing(
    storage: &dyn StorageReader,
    table: &str,
) -> Vec<(Rc<TableDefinition>, ForeignKeyDefinition)> {
    let mut keys = Vec::new();
    for child in storage.tables_with_foreign_keys() {
        for key in &child.foreign_keys {
            if key.references == table {
                keys.push((Rc::clone(&child), key.clone()));
            }
        }
    }
    keys
}

/// The foreign keys that need a unique index: those referencing its columns, unless they are
/// also its table's primary key, which holds each of their values once anyway.
pub(crate) fn needing(
    storage: &dyn StorageReader,
    index: &IndexDefinition,
) -> Vec<(Rc<TableDefinition>, ForeignKeyDefinition)> {
    let same = |columns: &[String]| {
        columns.len() == index.columns.len()
            && columns.iter().all(|column| index.columns.contains(column))
    };
    match storage.table_schema(&index.table) {
        Ok(parent) if index.unique && !same(&parent.primary_key) => {
            referencing(storage, &index.table)
                .into_iter()
                .filter(|(_, key)| same(&key.referenced_columns))
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Checks that every row of `table` a new foreign key covers references a row.
pub(crate) fn check_rows(
    storage: &dyn StorageReader,
    table: &TableDefinition,
    key: &ForeignKeyDefinition,
) -> Result<()> {
    for row in rows_where(storage, &table.name, &[], &[], None)? {
        if let Some(values) = values(&row, &key.columns)
            && !survives(
                storage,
                &[],
                &key.references,
                &key.referenced_columns,
                &values,
            )?
        {
            return Err(referencing_violation(&table.name, key));
        }
    }
    Ok(())
}

/// A row a statement changes: the row it replaces, and the row it leaves, where there is one.
struct Changed {
    table: Rc<TableDefinition>,
    old: Option<Row>,
    new: Option<Row>,
}

/// Checks a planned statement's rows against the foreign keys of the tables it changes and of the
/// tables that reference them, and adds the changes their actions make to the rows referencing a
/// row it deletes or updates.
pub(crate) fn enforce(storage: &dyn StorageReader, planned: &mut PlannedDml) -> Result<()> {
    let Some(first) = planned.changes.first() else {
        return Ok(());
    };
    let keyed = storage.tables_with_foreign_keys();
    let name = change_table(first);
    if !keyed.iter().any(|child| {
        child.name == *name || child.foreign_keys.iter().any(|key| key.references == *name)
    }) {
        return Ok(());
    }

    let schema = storage.table_schema(name)?;
    let layout = storage.record_layout(name);
    let decode = |key: &[u8], value: &[u8]| -> Result<Row> {
        let layout = layout
            .as_ref()
            .ok_or_else(|| EngineError::new("INTERNAL_ERROR", "A record has no layout"))?;
        StoredRecord::new(&schema, layout, key, value)?.to_row()
    };
    let mut changed = Vec::with_capacity(planned.changes.len());
    for (position, change) in planned.changes.iter().enumerate() {
        let (key, new) = match change {
            RowChange::Upsert { row, .. } => (primary_key(&schema, row), Some(row.clone())),
            RowChange::Delete { key, .. } => (key.clone(), None),
            RowChange::Put { key, record, .. } => {
                let row = decode(key, record)?;
                (primary_key(&schema, &row), Some(row))
            }
            RowChange::Remove { key, .. } => {
                (primary_key(&schema, &decode(key, EMPTY_RECORD)?), None)
            }
        };
        let old = match planned.previous.get(position) {
            Some(PreviousRow::Read(None)) => None,
            Some(PreviousRow::Read(Some(HeldRow::Map(row)))) => Some(row.clone()),
            Some(PreviousRow::Read(Some(HeldRow::Stored(entry)))) => {
                Some(decode(entry.key(), entry.value())?)
            }
            _ => storage.lookup_primary_key(name, &key)?,
        };
        changed.push(Changed {
            table: Rc::clone(&schema),
            old,
            new,
        });
    }
    // A row an UPDATE moved to a new key replaces, at its new key, the row it was.
    for &(deleted, written) in &planned.moved {
        changed[written].old = changed[deleted].old.take();
    }
    let mut deleted = planned
        .moved
        .iter()
        .map(|(deleted, _)| *deleted)
        .collect::<Vec<_>>();
    deleted.sort_unstable();
    for deleted in deleted.into_iter().rev() {
        changed.remove(deleted);
    }
    let own = changed.len();

    let mut position = 0;
    while position < changed.len() {
        let Some(old) = changed[position].old.clone() else {
            position += 1;
            continue;
        };
        let parent = Rc::clone(&changed[position].table);
        let new = changed[position].new.clone();
        for (child, key) in referencing(storage, &parent.name) {
            let Some(old_values) = values(&old, &key.referenced_columns) else {
                continue;
            };
            let new_values = new.as_ref().map(|new| {
                key.referenced_columns
                    .iter()
                    .map(|column| new.get(column).cloned().unwrap_or(Value::Null))
                    .collect::<Vec<_>>()
            });
            if new_values.as_ref() == Some(&old_values) {
                continue;
            }
            let action = if new.is_some() {
                key.on_update
            } else {
                key.on_delete
            };
            // NO ACTION lets the values survive in a row the statement leaves them in.
            if action == ForeignKeyAction::NoAction
                && survives(
                    storage,
                    &changed,
                    &parent.name,
                    &key.referenced_columns,
                    &old_values,
                )?
            {
                continue;
            }
            for row in rows_where(storage, &child.name, &key.columns, &old_values, None)? {
                let row_key = primary_key(&child, &row);
                let found = changed.iter().position(|other| {
                    other.table.name == child.name
                        && other
                            .old
                            .as_ref()
                            .is_some_and(|old| primary_key(&child, old) == row_key)
                });
                // A row the statement changes itself is checked as it leaves it.
                if found.is_some_and(|found| found < own) {
                    continue;
                }
                let base = match found {
                    Some(found) => match &changed[found].new {
                        Some(new) => new.clone(),
                        None => continue,
                    },
                    None => row.clone(),
                };
                let next = match (action, &new_values) {
                    (ForeignKeyAction::NoAction | ForeignKeyAction::Restrict, _) => {
                        return Err(EngineError::constraint_violation(format!(
                            "Changing a row of `{}` breaks foreign key `{}` of `{}`, whose rows \
                             reference it",
                            parent.name, key.name, child.name
                        )));
                    }
                    (ForeignKeyAction::Cascade, None) => None,
                    (action, _) => {
                        let mut next = base;
                        for (position, column) in key.columns.iter().enumerate() {
                            let value = match action {
                                ForeignKeyAction::Cascade => new_values
                                    .as_ref()
                                    .map_or(Value::Null, |values| values[position].clone()),
                                ForeignKeyAction::SetDefault => child
                                    .columns
                                    .iter()
                                    .find(|definition| definition.name == *column)
                                    .and_then(|definition| definition.default.clone())
                                    .unwrap_or(Value::Null),
                                _ => Value::Null,
                            };
                            next.insert(column.clone(), value);
                        }
                        Some(normalize_row(&child, next)?)
                    }
                };
                match found {
                    Some(found) => changed[found].new = next,
                    None => changed.push(Changed {
                        table: Rc::clone(&child),
                        old: Some(row),
                        new: next,
                    }),
                }
            }
        }
        position += 1;
    }

    for item in &changed {
        let Some(new) = &item.new else { continue };
        for key in &item.table.foreign_keys {
            let Some(new_values) = values(new, &key.columns) else {
                continue;
            };
            // An unchanged reference is checked again only if the statement changes the table it
            // references.
            if item.old.as_ref().and_then(|old| values(old, &key.columns))
                == Some(new_values.clone())
                && !changed
                    .iter()
                    .any(|other| other.table.name == key.references)
            {
                continue;
            }
            if !survives(
                storage,
                &changed,
                &key.references,
                &key.referenced_columns,
                &new_values,
            )? {
                return Err(referencing_violation(&item.table.name, key));
            }
        }
    }

    for item in changed.drain(own..) {
        let table = item.table.name.clone();
        if !planned.outcome.tables.contains(&table) {
            planned.outcome.tables.push(table.clone());
        }
        planned.changes.push(match item.new {
            Some(row) => RowChange::Upsert { table, row },
            None => RowChange::Delete {
                key: primary_key(
                    &item.table,
                    item.old.as_ref().expect("an action changes a stored row"),
                ),
                table,
            },
        });
        planned.previous.push(PreviousRow::Unread);
    }
    Ok(())
}

fn referencing_violation(table: &str, key: &ForeignKeyDefinition) -> EngineError {
    EngineError::constraint_violation(format!(
        "A row of `{table}` breaks foreign key `{}`: no row of `{}` has its values",
        key.name, key.references
    ))
}

/// Whether, once the statement's changes are made, a row of `table` holds `values` in `columns`.
fn survives(
    storage: &dyn StorageReader,
    changed: &[Changed],
    table: &str,
    columns: &[String],
    values: &[Value],
) -> Result<bool> {
    let touched = |row: &Row| {
        changed.iter().any(|item| {
            item.table.name == table
                && item.old.as_ref().is_some_and(|old| {
                    primary_key(&item.table, old) == primary_key(&item.table, row)
                })
        })
    };
    if changed.iter().any(|item| {
        item.table.name == table
            && item
                .new
                .as_ref()
                .and_then(|new| self::values(new, columns))
                .as_deref()
                == Some(values)
    }) {
        return Ok(true);
    }
    // A primary or unique key holds the values in one stored row at most.
    Ok(rows_where(storage, table, columns, values, Some(1))?
        .iter()
        .any(|row| !touched(row)))
}

/// The rows of `table` whose `columns` hold `values`, read as a query reads them.
fn rows_where(
    storage: &dyn StorageReader,
    table: &str,
    columns: &[String],
    values: &[Value],
    limit: Option<usize>,
) -> Result<Vec<Row>> {
    let predicates = columns
        .iter()
        .zip(values)
        .map(|(column, value)| Predicate::Comparison {
            column: column.clone(),
            operator: ComparisonOperator::Eq,
            value: value.clone(),
        })
        .collect::<Vec<_>>();
    let plan = SelectPlan {
        table: table.to_owned(),
        columns: None,
        positional: false,
        array_rows: false,
        ordered_by_outputs: false,
        predicate: (!predicates.is_empty()).then_some(Predicate::And { predicates }),
        order_by: Vec::new(),
        limit,
        offset: 0,
    };
    Ok(crate::query::execute(storage, &plan)?.rows)
}

/// A row's values in `columns`, or none if one of them is NULL, which a foreign key ignores.
fn values(row: &Row, columns: &[String]) -> Option<Vec<Value>> {
    columns
        .iter()
        .map(|column| row.get(column).filter(|value| !value.is_null()).cloned())
        .collect()
}

fn primary_key(schema: &TableDefinition, row: &Row) -> Row {
    let mut key = Row::new();
    for column in &schema.primary_key {
        key.insert(
            column.clone(),
            row.get(column).cloned().unwrap_or(Value::Null),
        );
    }
    key
}
