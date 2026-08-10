use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ApplyOutcome, Change, ChangeBatch, EngineError, Result, Row, TableSchema};

pub trait StorageDriver {
    fn define_table(&mut self, schema: TableSchema) -> Result<()>;
    fn replace_table(&mut self, table: &str, rows: Vec<Row>) -> Result<ApplyOutcome>;
    fn apply_batch(&mut self, batch: &ChangeBatch) -> Result<ApplyOutcome>;
    fn scan_table(&self, table: &str) -> Result<Vec<Row>>;
    fn revision(&self) -> u64;
}

#[derive(Clone, Debug, Default)]
pub struct InMemoryStorage {
    revision: u64,
    tables: BTreeMap<String, TableData>,
}

#[derive(Clone, Debug)]
struct TableData {
    schema: TableSchema,
    rows: BTreeMap<String, Row>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StorageSnapshot {
    revision: u64,
    tables: Vec<TableSnapshot>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TableSnapshot {
    schema: TableSchema,
    rows: Vec<Row>,
}

impl InMemoryStorage {
    pub fn export_snapshot(&self) -> Result<Vec<u8>> {
        let snapshot = StorageSnapshot {
            revision: self.revision,
            tables: self
                .tables
                .values()
                .map(|table| TableSnapshot {
                    schema: table.schema.clone(),
                    rows: table.rows.values().cloned().collect(),
                })
                .collect(),
        };
        crate::snapshot::encode(&snapshot)
    }

    pub fn import_snapshot(&mut self, bytes: &[u8]) -> Result<()> {
        let replacement = Self::from_snapshot(bytes)?;
        *self = replacement;
        Ok(())
    }

    fn from_snapshot(bytes: &[u8]) -> Result<Self> {
        let snapshot: StorageSnapshot = crate::snapshot::decode(bytes)?;
        let mut tables = BTreeMap::new();

        for table in snapshot.tables {
            validate_schema(&table.schema).map_err(snapshot_validation_error)?;
            let table_name = table.schema.name.clone();
            let mut rows = BTreeMap::new();

            for row in table.rows {
                let key = row_key(&table.schema, &row).map_err(snapshot_validation_error)?;
                if rows.insert(key, row).is_some() {
                    return Err(EngineError::invalid_snapshot(format!(
                        "Table `{table_name}` contains a duplicate primary key"
                    )));
                }
            }

            if tables
                .insert(
                    table_name.clone(),
                    TableData {
                        schema: table.schema,
                        rows,
                    },
                )
                .is_some()
            {
                return Err(EngineError::invalid_snapshot(format!(
                    "Snapshot contains table `{table_name}` more than once"
                )));
            }
        }

        Ok(Self {
            revision: snapshot.revision,
            tables,
        })
    }
}

impl StorageDriver for InMemoryStorage {
    fn define_table(&mut self, schema: TableSchema) -> Result<()> {
        validate_schema(&schema)?;

        if let Some(existing) = self.tables.get(&schema.name) {
            if existing.schema == schema {
                return Ok(());
            }
            return Err(EngineError::invalid_schema(format!(
                "Table `{}` is already defined with a different schema",
                schema.name
            )));
        }

        self.tables.insert(
            schema.name.clone(),
            TableData {
                schema,
                rows: BTreeMap::new(),
            },
        );
        Ok(())
    }

    fn replace_table(&mut self, table: &str, rows: Vec<Row>) -> Result<ApplyOutcome> {
        let current = self
            .tables
            .get(table)
            .ok_or_else(|| EngineError::table_not_found(table))?;
        let mut replacement = BTreeMap::new();

        for row in rows {
            let key = row_key(&current.schema, &row)?;
            if replacement.insert(key, row).is_some() {
                return Err(EngineError::invalid_change(format!(
                    "Snapshot for `{table}` contains a duplicate primary key"
                )));
            }
        }

        self.tables
            .get_mut(table)
            .expect("table was checked above")
            .rows = replacement;
        self.revision = next_revision(self.revision)?;

        Ok(ApplyOutcome {
            revision: self.revision,
            tables: vec![table.to_owned()],
        })
    }

    fn apply_batch(&mut self, batch: &ChangeBatch) -> Result<ApplyOutcome> {
        if batch.changes.is_empty() {
            return Ok(ApplyOutcome {
                revision: self.revision,
                tables: vec![],
            });
        }

        // Apply to a clone first so a malformed later change cannot leave a partial batch.
        let mut candidate = self.clone();
        let mut changed_tables = BTreeSet::new();

        for change in &batch.changes {
            match change {
                Change::Upsert { table, row } => {
                    let target = candidate
                        .tables
                        .get_mut(table)
                        .ok_or_else(|| EngineError::table_not_found(table))?;
                    let key = row_key(&target.schema, row)?;
                    target.rows.insert(key, row.clone());
                    changed_tables.insert(table.clone());
                }
                Change::Delete { table, key } => {
                    let target = candidate
                        .tables
                        .get_mut(table)
                        .ok_or_else(|| EngineError::table_not_found(table))?;
                    let key = row_key(&target.schema, key)?;
                    target.rows.remove(&key);
                    changed_tables.insert(table.clone());
                }
            }
        }

        candidate.revision = next_revision(self.revision)?;
        let outcome = ApplyOutcome {
            revision: candidate.revision,
            tables: changed_tables.into_iter().collect(),
        };
        *self = candidate;
        Ok(outcome)
    }

    fn scan_table(&self, table: &str) -> Result<Vec<Row>> {
        self.tables
            .get(table)
            .map(|table| table.rows.values().cloned().collect())
            .ok_or_else(|| EngineError::table_not_found(table))
    }

    fn revision(&self) -> u64 {
        self.revision
    }
}

fn validate_schema(schema: &TableSchema) -> Result<()> {
    if schema.name.trim().is_empty() {
        return Err(EngineError::invalid_schema("A table name cannot be empty"));
    }
    if schema.primary_key.is_empty() {
        return Err(EngineError::invalid_schema(format!(
            "Table `{}` must declare at least one primary-key column",
            schema.name
        )));
    }

    let mut columns = HashSet::new();
    for column in &schema.primary_key {
        if column.trim().is_empty() {
            return Err(EngineError::invalid_schema(format!(
                "Table `{}` contains an empty primary-key column",
                schema.name
            )));
        }
        if !columns.insert(column) {
            return Err(EngineError::invalid_schema(format!(
                "Table `{}` declares primary-key column `{column}` more than once",
                schema.name
            )));
        }
    }
    Ok(())
}

fn row_key(schema: &TableSchema, row: &Row) -> Result<String> {
    let mut values = Vec::with_capacity(schema.primary_key.len());
    for column in &schema.primary_key {
        let value = row.get(column).ok_or_else(|| {
            EngineError::invalid_change(format!(
                "Row for `{}` is missing primary-key column `{column}`",
                schema.name
            ))
        })?;
        if value == &Value::Null {
            return Err(EngineError::invalid_change(format!(
                "Primary-key column `{column}` in `{}` cannot be null",
                schema.name
            )));
        }
        values.push(value);
    }
    serde_json::to_string(&values).map_err(|error| {
        EngineError::invalid_change(format!("Could not encode primary key: {error}"))
    })
}

fn next_revision(revision: u64) -> Result<u64> {
    revision
        .checked_add(1)
        .ok_or_else(|| EngineError::new("REVISION_OVERFLOW", "Database revision overflowed"))
}

fn snapshot_validation_error(error: EngineError) -> EngineError {
    EngineError::invalid_snapshot(error.message)
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::*;

    fn row(value: Value) -> Row {
        value
            .as_object()
            .expect("test row must be an object")
            .clone()
    }

    fn storage() -> InMemoryStorage {
        let mut storage = InMemoryStorage::default();
        storage
            .define_table(TableSchema {
                name: "posts".to_owned(),
                primary_key: vec!["id".to_owned()],
            })
            .unwrap();
        storage
    }

    #[test]
    fn replace_table_is_atomic() {
        let mut storage = storage();
        storage
            .replace_table("posts", vec![row(json!({"id": 1, "title": "kept"}))])
            .unwrap();

        let error = storage
            .replace_table(
                "posts",
                vec![
                    row(json!({"id": 2, "title": "duplicate"})),
                    row(json!({"id": 2, "title": "duplicate"})),
                ],
            )
            .unwrap_err();

        assert_eq!(error.code, "INVALID_CHANGE");
        assert_eq!(
            storage.scan_table("posts").unwrap(),
            vec![row(json!({"id": 1, "title": "kept"}))]
        );
        assert_eq!(storage.revision(), 1);
    }

    #[test]
    fn failed_change_batch_rolls_back_every_change() {
        let mut storage = storage();
        storage
            .replace_table("posts", vec![row(json!({"id": 1, "title": "before"}))])
            .unwrap();

        let error = storage
            .apply_batch(&ChangeBatch {
                changes: vec![
                    Change::Upsert {
                        table: "posts".to_owned(),
                        row: row(json!({"id": 1, "title": "after"})),
                    },
                    Change::Upsert {
                        table: "missing".to_owned(),
                        row: row(json!({"id": 2})),
                    },
                ],
                ..ChangeBatch::default()
            })
            .unwrap_err();

        assert_eq!(error.code, "TABLE_NOT_FOUND");
        assert_eq!(
            storage.scan_table("posts").unwrap(),
            vec![row(json!({"id": 1, "title": "before"}))]
        );
        assert_eq!(storage.revision(), 1);
    }

    #[test]
    fn delete_uses_composite_primary_key() {
        let mut storage = InMemoryStorage::default();
        storage
            .define_table(TableSchema {
                name: "memberships".to_owned(),
                primary_key: vec!["team_id".to_owned(), "user_id".to_owned()],
            })
            .unwrap();
        storage
            .replace_table(
                "memberships",
                vec![
                    row(json!({"team_id": 1, "user_id": 1, "role": "owner"})),
                    row(json!({"team_id": 1, "user_id": 2, "role": "member"})),
                ],
            )
            .unwrap();

        storage
            .apply_batch(&ChangeBatch {
                changes: vec![Change::Delete {
                    table: "memberships".to_owned(),
                    key: row(json!({"team_id": 1, "user_id": 2})),
                }],
                ..ChangeBatch::default()
            })
            .unwrap();

        assert_eq!(
            storage.scan_table("memberships").unwrap(),
            vec![row(json!({"team_id": 1, "user_id": 1, "role": "owner"}))]
        );
    }

    #[test]
    fn primary_key_must_be_present_and_non_null() {
        let mut storage = storage();
        for invalid in [Map::new(), row(json!({"id": null}))] {
            let error = storage.replace_table("posts", vec![invalid]).unwrap_err();
            assert_eq!(error.code, "INVALID_CHANGE");
        }
    }

    #[test]
    fn snapshot_round_trips_complete_state_deterministically() {
        let mut storage = InMemoryStorage::default();
        storage
            .define_table(TableSchema {
                name: "empty".to_owned(),
                primary_key: vec!["id".to_owned()],
            })
            .unwrap();
        storage
            .define_table(TableSchema {
                name: "memberships".to_owned(),
                primary_key: vec!["team_id".to_owned(), "user_id".to_owned()],
            })
            .unwrap();
        storage
            .replace_table(
                "memberships",
                vec![
                    row(json!({"team_id": 2, "user_id": 1, "role": "member"})),
                    row(json!({"team_id": 1, "user_id": 2, "role": "owner"})),
                ],
            )
            .unwrap();
        storage
            .apply_batch(&ChangeBatch {
                changes: vec![Change::Upsert {
                    table: "memberships".to_owned(),
                    row: row(json!({"team_id": 1, "user_id": 2, "role": "admin"})),
                }],
                ..ChangeBatch::default()
            })
            .unwrap();

        let first = storage.export_snapshot().unwrap();
        assert_eq!(storage.export_snapshot().unwrap(), first);

        let mut restored = InMemoryStorage::default();
        restored.import_snapshot(&first).unwrap();

        assert_eq!(restored.revision(), 2);
        assert!(restored.scan_table("empty").unwrap().is_empty());
        assert_eq!(
            restored.scan_table("memberships").unwrap(),
            vec![
                row(json!({"team_id": 1, "user_id": 2, "role": "admin"})),
                row(json!({"team_id": 2, "user_id": 1, "role": "member"})),
            ]
        );
        assert_eq!(restored.export_snapshot().unwrap(), first);

        // The restored schema is present and retains its composite primary key.
        restored
            .define_table(TableSchema {
                name: "memberships".to_owned(),
                primary_key: vec!["team_id".to_owned(), "user_id".to_owned()],
            })
            .unwrap();
    }

    #[test]
    fn failed_snapshot_import_preserves_live_state() {
        let mut storage = storage();
        storage
            .replace_table("posts", vec![row(json!({"id": 1, "title": "kept"}))])
            .unwrap();

        let invalid = StorageSnapshot {
            revision: 99,
            tables: vec![TableSnapshot {
                schema: TableSchema {
                    name: "posts".to_owned(),
                    primary_key: vec!["id".to_owned()],
                },
                rows: vec![
                    row(json!({"id": 2, "title": "duplicate"})),
                    row(json!({"id": 2, "title": "duplicate"})),
                ],
            }],
        };
        let error = storage
            .import_snapshot(&crate::snapshot::encode(&invalid).unwrap())
            .unwrap_err();

        assert_eq!(error.code, "INVALID_SNAPSHOT");
        assert_eq!(storage.revision(), 1);
        assert_eq!(
            storage.scan_table("posts").unwrap(),
            vec![row(json!({"id": 1, "title": "kept"}))]
        );
    }

    #[test]
    fn snapshot_rejects_duplicate_tables_and_invalid_primary_keys() {
        let schema = TableSchema {
            name: "posts".to_owned(),
            primary_key: vec!["id".to_owned()],
        };
        let invalid_snapshots = [
            StorageSnapshot {
                revision: 0,
                tables: vec![
                    TableSnapshot {
                        schema: schema.clone(),
                        rows: vec![],
                    },
                    TableSnapshot {
                        schema: schema.clone(),
                        rows: vec![],
                    },
                ],
            },
            StorageSnapshot {
                revision: 0,
                tables: vec![TableSnapshot {
                    schema,
                    rows: vec![row(json!({"title": "missing id"}))],
                }],
            },
        ];

        for snapshot in invalid_snapshots {
            let error =
                InMemoryStorage::from_snapshot(&crate::snapshot::encode(&snapshot).unwrap())
                    .unwrap_err();
            assert_eq!(error.code, "INVALID_SNAPSHOT");
        }
    }
}
