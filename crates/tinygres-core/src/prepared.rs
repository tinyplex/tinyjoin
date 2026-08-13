use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::storage::StorageReader;
use crate::{
    Change, ChangeBatch, EngineError, InMemoryStorage, IndexDefinition, Result, Row, StorageDriver,
    TableSchema,
};

const MAGIC: &[u8; 8] = b"TGRCMIT\0";
const FORMAT_VERSION: u16 = 1;
const FLAGS: u16 = 0;
const HEADER_LENGTH: usize = 20;
const MAX_COMMIT_BYTES: usize = 16 * 1024 * 1024;
const MAX_OPERATIONS: usize = 200_000;
const MAX_ROWS_PER_REPLACEMENT: usize = 100_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedCommit {
    bytes: Vec<u8>,
    revision_before: u64,
    revision_after: u64,
    tables: Vec<String>,
}

impl PreparedCommit {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub fn revision_before(&self) -> u64 {
        self.revision_before
    }

    pub fn revision_after(&self) -> u64 {
        self.revision_after
    }

    pub fn tables(&self) -> &[String] {
        &self.tables
    }

    pub(crate) fn from_states(
        before: &InMemoryStorage,
        after: &InMemoryStorage,
        tables: &[String],
    ) -> Result<Option<Self>> {
        let operations = diff_states(before, after)?;
        if operations.is_empty() && before.revision() == after.revision() {
            return Ok(None);
        }
        let tables = tables.iter().cloned().collect::<BTreeSet<_>>();
        if tables.is_empty() {
            return Err(invalid_commit(
                "A state-changing prepared commit must name an affected table",
            ));
        }
        let payload = CommitPayload {
            revision_before: before.revision(),
            revision_after: after.revision(),
            tables: tables.into_iter().collect(),
            operations,
        };
        validate_payload(&payload)?;
        let tables = payload.tables.clone();
        let bytes = encode_payload(&payload)?;
        Ok(Some(Self {
            bytes,
            revision_before: payload.revision_before,
            revision_after: payload.revision_after,
            tables,
        }))
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<(Self, CommitPayload)> {
        let payload = decode_payload(bytes)?;
        validate_payload(&payload)?;
        let tables = payload.tables.clone();
        Ok((
            Self {
                bytes: bytes.to_vec(),
                revision_before: payload.revision_before,
                revision_after: payload.revision_after,
                tables,
            },
            payload,
        ))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CommitPayload {
    revision_before: u64,
    revision_after: u64,
    tables: Vec<String>,
    operations: Vec<CommitOperation>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "type", deny_unknown_fields)]
enum CommitOperation {
    CreateTable { schema: TableSchema },
    DropTable { table: String },
    ReplaceTable { schema: TableSchema, rows: Vec<Row> },
    DeleteRows { table: String, keys: Vec<Row> },
    UpsertRows { table: String, rows: Vec<Row> },
    CreateIndex { definition: IndexDefinition },
    DropIndex { name: String, table: String },
}

impl CommitOperation {
    fn table(&self) -> &str {
        match self {
            Self::CreateTable { schema } | Self::ReplaceTable { schema, .. } => &schema.name,
            Self::DropTable { table }
            | Self::DeleteRows { table, .. }
            | Self::UpsertRows { table, .. }
            | Self::DropIndex { table, .. } => table,
            Self::CreateIndex { definition } => &definition.table,
        }
    }
}

pub(crate) fn apply_payload(
    storage: &InMemoryStorage,
    payload: &CommitPayload,
) -> Result<InMemoryStorage> {
    if storage.revision() != payload.revision_before {
        return Err(EngineError::new(
            "PREPARED_COMMIT_REVISION_MISMATCH",
            format!(
                "Prepared commit starts at revision {}, but the database is at revision {}",
                payload.revision_before,
                storage.revision()
            ),
        ));
    }

    let mut candidate = storage.clone();
    for operation in &payload.operations {
        match operation {
            CommitOperation::CreateTable { schema } => candidate.define_table(schema.clone())?,
            CommitOperation::DropTable { table } => candidate.drop_table(table)?,
            CommitOperation::ReplaceTable { schema, rows } => {
                let table = schema.name.clone();
                candidate.drop_table(&table)?;
                candidate.define_table(schema.clone())?;
                candidate.replace_table_unrevisioned(&table, rows.clone())?;
            }
            CommitOperation::DeleteRows { table, keys } => {
                apply_changes_unrevisioned(
                    &mut candidate,
                    ChangeBatch {
                        changes: keys
                            .iter()
                            .cloned()
                            .map(|key| Change::Delete {
                                table: table.clone(),
                                key,
                            })
                            .collect(),
                        ..ChangeBatch::default()
                    },
                )?;
            }
            CommitOperation::UpsertRows { table, rows } => {
                apply_changes_unrevisioned(
                    &mut candidate,
                    ChangeBatch {
                        changes: rows
                            .iter()
                            .cloned()
                            .map(|row| Change::Upsert {
                                table: table.clone(),
                                row,
                            })
                            .collect(),
                        ..ChangeBatch::default()
                    },
                )?;
            }
            CommitOperation::CreateIndex { definition } => {
                candidate.define_index(definition.clone())?
            }
            CommitOperation::DropIndex { name, .. } => candidate.drop_index(name)?,
        }
    }
    candidate.set_revision(payload.revision_after)?;
    Ok(candidate)
}

fn apply_changes_unrevisioned(candidate: &mut InMemoryStorage, batch: ChangeBatch) -> Result<()> {
    let revision = candidate.revision();
    candidate.apply_batch(&batch)?;
    candidate.set_revision(revision)?;
    Ok(())
}

fn diff_states(before: &InMemoryStorage, after: &InMemoryStorage) -> Result<Vec<CommitOperation>> {
    let before_tables = before.table_names().collect::<BTreeSet<_>>();
    let after_tables = after.table_names().collect::<BTreeSet<_>>();
    let mut replaced_tables = BTreeSet::new();
    let mut operations = Vec::new();

    // Indexes must disappear before their tables or changed schemas.
    let before_indexes = before.index_names().collect::<BTreeSet<_>>();
    let after_indexes = after.index_names().collect::<BTreeSet<_>>();
    for name in before_indexes.difference(&after_indexes) {
        let definition = before
            .index_definition(name)
            .expect("the index name came from this catalog");
        operations.push(CommitOperation::DropIndex {
            name: (*name).to_owned(),
            table: definition.table,
        });
    }
    for name in before_indexes.intersection(&after_indexes) {
        let previous = before
            .index_definition(name)
            .expect("the index name came from this catalog");
        let next = after
            .index_definition(name)
            .expect("the index name came from this catalog");
        if previous != next {
            operations.push(CommitOperation::DropIndex {
                name: (*name).to_owned(),
                table: previous.table,
            });
        }
    }

    for table in before_tables.difference(&after_tables) {
        operations.push(CommitOperation::DropTable {
            table: (*table).to_owned(),
        });
    }
    for table in after_tables.difference(&before_tables) {
        let schema = after.table_schema(table)?;
        let rows = after
            .table_rows(table)?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        operations.push(CommitOperation::CreateTable { schema });
        if !rows.is_empty() {
            operations.push(CommitOperation::UpsertRows {
                table: (*table).to_owned(),
                rows,
            });
        }
    }
    for table in before_tables.intersection(&after_tables) {
        let previous_schema = before.table_schema(table)?;
        let next_schema = after.table_schema(table)?;
        let previous_rows = before.table_rows(table)?;
        let next_rows = after.table_rows(table)?;
        if previous_schema != next_schema {
            replaced_tables.insert(*table);
            operations.push(CommitOperation::ReplaceTable {
                schema: next_schema,
                rows: next_rows.values().cloned().collect(),
            });
            continue;
        }

        let mut keys = Vec::new();
        for (key, row) in previous_rows {
            if !next_rows.contains_key(key) {
                keys.push(primary_key(&previous_schema, row)?);
            }
        }
        if !keys.is_empty() {
            operations.push(CommitOperation::DeleteRows {
                table: (*table).to_owned(),
                keys,
            });
        }
        let rows = next_rows
            .iter()
            .filter(|(key, row)| previous_rows.get(*key) != Some(*row))
            .map(|(_, row)| row.clone())
            .collect::<Vec<_>>();
        if !rows.is_empty() {
            operations.push(CommitOperation::UpsertRows {
                table: (*table).to_owned(),
                rows,
            });
        }
    }

    for name in after_indexes {
        let next = after
            .index_definition(name)
            .expect("the index name came from this catalog");
        if before.index_definition(name).as_ref() != Some(&next)
            || replaced_tables.contains(next.table.as_str())
        {
            operations.push(CommitOperation::CreateIndex { definition: next });
        }
    }
    if operations.len() > MAX_OPERATIONS {
        return Err(invalid_commit(format!(
            "A prepared commit cannot contain more than {MAX_OPERATIONS} operations"
        )));
    }
    Ok(operations)
}

fn primary_key(schema: &TableSchema, row: &Row) -> Result<Row> {
    let mut key = Row::new();
    for name in &schema.primary_key {
        key.insert(
            name.clone(),
            row.get(name)
                .expect("stored rows always contain their primary key")
                .clone(),
        );
    }
    Ok(key)
}

fn encode_payload(payload: &CommitPayload) -> Result<Vec<u8>> {
    let body = serde_json::to_vec(payload)
        .map_err(|error| invalid_commit(format!("Could not encode prepared commit: {error}")))?;
    if body.len() > MAX_COMMIT_BYTES - HEADER_LENGTH {
        return Err(EngineError::new(
            "PREPARED_COMMIT_TOO_LARGE",
            format!("A prepared commit cannot exceed {MAX_COMMIT_BYTES} bytes"),
        ));
    }
    let mut bytes = Vec::with_capacity(HEADER_LENGTH + body.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&FLAGS.to_le_bytes());
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&crate::snapshot::crc32(&body).to_le_bytes());
    bytes.extend_from_slice(&body);
    Ok(bytes)
}

fn decode_payload(bytes: &[u8]) -> Result<CommitPayload> {
    if bytes.len() < HEADER_LENGTH {
        return Err(invalid_commit("Prepared commit is shorter than its header"));
    }
    if bytes.len() > MAX_COMMIT_BYTES {
        return Err(EngineError::new(
            "PREPARED_COMMIT_TOO_LARGE",
            format!("A prepared commit cannot exceed {MAX_COMMIT_BYTES} bytes"),
        ));
    }
    if &bytes[..8] != MAGIC {
        return Err(invalid_commit(
            "Prepared commit has an invalid file signature",
        ));
    }
    let version = u16::from_le_bytes([bytes[8], bytes[9]]);
    if version != FORMAT_VERSION {
        return Err(EngineError::new(
            "UNSUPPORTED_PREPARED_COMMIT",
            format!("Prepared commit format version {version} is not supported"),
        ));
    }
    let flags = u16::from_le_bytes([bytes[10], bytes[11]]);
    if flags != FLAGS {
        return Err(EngineError::new(
            "UNSUPPORTED_PREPARED_COMMIT",
            format!("Prepared commit uses unsupported format flags 0x{flags:04x}"),
        ));
    }
    let length = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
    if HEADER_LENGTH.checked_add(length) != Some(bytes.len()) {
        return Err(invalid_commit(
            "Prepared commit length does not match its header",
        ));
    }
    let body = &bytes[HEADER_LENGTH..];
    let expected = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    if crate::snapshot::crc32(body) != expected {
        return Err(invalid_commit(
            "Prepared commit payload checksum does not match",
        ));
    }
    serde_json::from_slice(body)
        .map_err(|error| invalid_commit(format!("Prepared commit payload is not valid: {error}")))
}

fn validate_payload(payload: &CommitPayload) -> Result<()> {
    crate::revision::validate_database_revision(payload.revision_before)
        .map_err(|error| invalid_commit(error.message))?;
    crate::revision::validate_database_revision(payload.revision_after)
        .map_err(|error| invalid_commit(error.message))?;
    if payload.operations.is_empty() && payload.revision_after == payload.revision_before {
        return Err(invalid_commit(
            "A prepared commit must change database state",
        ));
    }
    if payload.operations.len() > MAX_OPERATIONS {
        return Err(invalid_commit(format!(
            "A prepared commit cannot contain more than {MAX_OPERATIONS} operations"
        )));
    }
    if payload.revision_after < payload.revision_before
        || payload.revision_after > payload.revision_before.saturating_add(1)
    {
        return Err(invalid_commit(
            "A prepared commit revision must stay unchanged or advance exactly once",
        ));
    }
    if payload.revision_after == payload.revision_before
        && payload
            .operations
            .iter()
            .any(|operation| !matches!(operation, CommitOperation::CreateTable { .. }))
    {
        return Err(invalid_commit(
            "A same-revision prepared commit may only define empty tables",
        ));
    }
    if payload.tables.is_empty()
        || payload.tables.iter().any(|table| table.trim().is_empty())
        || !payload.tables.windows(2).all(|pair| pair[0] < pair[1])
    {
        return Err(invalid_commit(
            "Prepared commit tables must be non-empty, unique, and sorted",
        ));
    }
    for operation in &payload.operations {
        if operation.table().trim().is_empty() {
            return Err(invalid_commit(
                "A prepared commit table name cannot be empty",
            ));
        }
        if payload
            .tables
            .binary_search_by(|table| table.as_str().cmp(operation.table()))
            .is_err()
        {
            return Err(invalid_commit(format!(
                "Prepared commit operation for `{}` is missing from its affected tables",
                operation.table()
            )));
        }
        match operation {
            CommitOperation::ReplaceTable { rows, .. }
            | CommitOperation::UpsertRows { rows, .. } => {
                if rows.len() > MAX_ROWS_PER_REPLACEMENT {
                    return Err(invalid_commit(format!(
                        "A prepared commit operation cannot contain more than {MAX_ROWS_PER_REPLACEMENT} rows"
                    )));
                }
            }
            CommitOperation::DeleteRows { keys, .. } => {
                if keys.len() > MAX_ROWS_PER_REPLACEMENT {
                    return Err(invalid_commit(format!(
                        "A prepared commit operation cannot contain more than {MAX_ROWS_PER_REPLACEMENT} keys"
                    )));
                }
            }
            _ => {}
        }
    }
    if payload.revision_after == payload.revision_before {
        let operation_tables = payload
            .operations
            .iter()
            .map(CommitOperation::table)
            .collect::<BTreeSet<_>>();
        let affected_tables = payload.tables.iter().map(String::as_str).collect();
        if operation_tables != affected_tables {
            return Err(invalid_commit(
                "A same-revision prepared commit must name exactly its defined tables",
            ));
        }
    }
    Ok(())
}

fn invalid_commit(message: impl Into<String>) -> EngineError {
    EngineError::new("INVALID_PREPARED_COMMIT", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{StorageReader, revision::MAX_DATABASE_REVISION};

    fn create_table_payload(revision: u64) -> CommitPayload {
        CommitPayload {
            revision_before: revision,
            revision_after: revision,
            tables: vec!["items".to_owned()],
            operations: vec![CommitOperation::CreateTable {
                schema: TableSchema {
                    name: "items".to_owned(),
                    primary_key: vec!["id".to_owned()],
                    columns: vec![],
                },
            }],
        }
    }

    #[test]
    fn prepared_commit_revision_bound_is_validated_before_replay() {
        let maximum_bytes = encode_payload(&create_table_payload(MAX_DATABASE_REVISION)).unwrap();
        let (_, maximum) = PreparedCommit::decode(&maximum_bytes).unwrap();
        let mut storage = InMemoryStorage::default();
        storage.set_revision(MAX_DATABASE_REVISION).unwrap();
        let replayed = apply_payload(&storage, &maximum).unwrap();
        assert_eq!(replayed.revision(), MAX_DATABASE_REVISION);
        assert_eq!(replayed.table_schema("items").unwrap().name, "items");

        let unsupported = create_table_payload(MAX_DATABASE_REVISION + 1);
        let bytes = encode_payload(&unsupported).unwrap();
        let error = PreparedCommit::decode(&bytes).unwrap_err();
        assert_eq!(error.code, "INVALID_PREPARED_COMMIT");
        assert_eq!(storage.revision(), MAX_DATABASE_REVISION);
        assert_eq!(
            storage.table_schema("items").unwrap_err().code,
            "TABLE_NOT_FOUND"
        );
    }
}
