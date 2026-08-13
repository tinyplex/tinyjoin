#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ApplyOutcome, Change, ChangeBatch, ColumnDefinition, ColumnType, EngineError, IndexDefinition,
    Result, Row, TableSchema,
};

const MAX_COLUMNS: usize = 256;

pub trait StorageDriver {
    fn define_table(&mut self, schema: TableSchema) -> Result<()>;
    fn drop_table(&mut self, table: &str) -> Result<()>;
    fn add_column(&mut self, table: &str, column: ColumnDefinition) -> Result<()>;
    fn replace_table_snapshot(
        &mut self,
        schema: TableSchema,
        rows: Vec<Row>,
    ) -> Result<ApplyOutcome>;
    fn replace_table(&mut self, table: &str, rows: Vec<Row>) -> Result<ApplyOutcome>;
    fn apply_batch(&mut self, batch: &ChangeBatch) -> Result<ApplyOutcome>;
    fn scan_table(&self, table: &str) -> Result<Vec<Row>>;
    fn lookup_primary_key(&self, table: &str, key: &Row) -> Result<Option<Row>>;
    fn define_index(&mut self, definition: IndexDefinition) -> Result<()>;
    fn drop_index(&mut self, name: &str) -> Result<()>;
    fn index_definition(&self, name: &str) -> Option<IndexDefinition>;
    fn indexes_for_table(&self, table: &str) -> Result<Vec<IndexDefinition>>;
    fn lookup_index(&self, table: &str, columns: &[String], key: &Row) -> Result<Option<Vec<Row>>>;
    fn table_schema(&self, table: &str) -> Result<TableSchema>;
    #[doc(hidden)]
    fn replace_table_unrevisioned(&mut self, table: &str, rows: Vec<Row>) -> Result<()>;
    #[doc(hidden)]
    fn advance_revision(&mut self) -> Result<u64>;
    fn revision(&self) -> u64;
}

#[derive(Clone, Debug, Default)]
pub struct InMemoryStorage {
    revision: u64,
    tables: BTreeMap<String, TableData>,
    indexes: BTreeMap<String, IndexData>,
    #[cfg(test)]
    scan_count: Cell<usize>,
    #[cfg(test)]
    lookup_count: Cell<usize>,
}

#[derive(Clone, Debug)]
struct TableData {
    schema: TableSchema,
    rows: BTreeMap<String, Row>,
}

#[derive(Clone, Debug)]
struct IndexData {
    definition: IndexDefinition,
    postings: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StorageSnapshot {
    revision: u64,
    tables: Vec<TableSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    indexes: Vec<IndexDefinition>,
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
            indexes: self
                .indexes
                .values()
                .map(|index| index.definition.clone())
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
                let row = normalize_row(&table.schema, row).map_err(snapshot_validation_error)?;
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

        let mut storage = Self {
            revision: snapshot.revision,
            tables,
            indexes: BTreeMap::new(),
            #[cfg(test)]
            scan_count: Cell::new(0),
            #[cfg(test)]
            lookup_count: Cell::new(0),
        };
        for definition in snapshot.indexes {
            storage
                .define_index(definition)
                .map_err(snapshot_validation_error)?;
        }
        Ok(storage)
    }

    #[cfg(test)]
    pub(crate) fn access_counts(&self) -> (usize, usize) {
        (self.scan_count.get(), self.lookup_count.get())
    }

    fn rebuilt_indexes_for_table(
        &self,
        table: &str,
        rows: &BTreeMap<String, Row>,
    ) -> Result<BTreeMap<String, IndexData>> {
        let mut indexes = self.indexes.clone();
        for index in indexes.values_mut() {
            if index.definition.table == table {
                index.postings = build_postings(&index.definition, rows)?;
            }
        }
        Ok(indexes)
    }

    pub(crate) fn table_names(&self) -> impl Iterator<Item = &str> {
        self.tables.keys().map(String::as_str)
    }

    pub(crate) fn table_rows(&self, table: &str) -> Result<&BTreeMap<String, Row>> {
        self.tables
            .get(table)
            .map(|table| &table.rows)
            .ok_or_else(|| EngineError::table_not_found(table))
    }

    pub(crate) fn index_names(&self) -> impl Iterator<Item = &str> {
        self.indexes.keys().map(String::as_str)
    }

    pub(crate) fn set_revision(&mut self, revision: u64) {
        self.revision = revision;
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

    fn drop_table(&mut self, table: &str) -> Result<()> {
        if self.tables.remove(table).is_none() {
            return Err(EngineError::table_not_found(table));
        }
        self.indexes
            .retain(|_, index| index.definition.table != table);
        Ok(())
    }

    fn add_column(&mut self, table: &str, column: ColumnDefinition) -> Result<()> {
        let current = self
            .tables
            .get(table)
            .ok_or_else(|| EngineError::table_not_found(table))?;
        if current.schema.columns.is_empty() {
            return Err(EngineError::unsupported_sql(format!(
                "ALTER TABLE ADD COLUMN requires a typed table catalog for `{table}`"
            )));
        }
        if current.schema.columns.len() >= MAX_COLUMNS {
            return Err(EngineError::invalid_schema(format!(
                "Table `{table}` cannot contain more than {MAX_COLUMNS} columns"
            )));
        }
        if current
            .schema
            .columns
            .iter()
            .any(|existing| existing.name == column.name)
        {
            return Err(EngineError::column_already_exists(&column.name, table));
        }

        let mut schema = current.schema.clone();
        schema.columns.push(column.clone());
        validate_schema(&schema)?;
        let value = column.default.clone().unwrap_or(Value::Null);
        let mut rows = BTreeMap::new();
        for (key, row) in &current.rows {
            let mut row = row.clone();
            row.insert(column.name.clone(), value.clone());
            let row = normalize_row(&schema, row)?;
            rows.insert(key.clone(), row);
        }

        let target = self
            .tables
            .get_mut(table)
            .expect("the altered table was resolved above");
        target.schema = schema;
        target.rows = rows;
        Ok(())
    }

    fn replace_table_snapshot(
        &mut self,
        schema: TableSchema,
        rows: Vec<Row>,
    ) -> Result<ApplyOutcome> {
        let table = schema.name.clone();
        let mut candidate = self.clone();
        candidate.define_table(schema)?;
        let outcome = candidate.replace_table(&table, rows)?;
        *self = candidate;
        Ok(outcome)
    }

    fn replace_table(&mut self, table: &str, rows: Vec<Row>) -> Result<ApplyOutcome> {
        self.replace_table_unrevisioned(table, rows)?;
        self.advance_revision()?;

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
                    let row = normalize_row(&target.schema, row.clone())?;
                    let key = row_key(&target.schema, &row)?;
                    target.rows.insert(key, row);
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

        for table in &changed_tables {
            let rows = &candidate
                .tables
                .get(table)
                .expect("a changed table was resolved above")
                .rows;
            candidate.indexes = candidate.rebuilt_indexes_for_table(table, rows)?;
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
        #[cfg(test)]
        self.scan_count.set(self.scan_count.get() + 1);
        self.tables
            .get(table)
            .map(|table| table.rows.values().cloned().collect())
            .ok_or_else(|| EngineError::table_not_found(table))
    }

    fn lookup_primary_key(&self, table: &str, key: &Row) -> Result<Option<Row>> {
        #[cfg(test)]
        self.lookup_count.set(self.lookup_count.get() + 1);
        let table = self
            .tables
            .get(table)
            .ok_or_else(|| EngineError::table_not_found(table))?;
        let key = row_key(&table.schema, key)?;
        Ok(table.rows.get(&key).cloned())
    }

    fn define_index(&mut self, definition: IndexDefinition) -> Result<()> {
        validate_index_definition(&definition, &self.tables)?;
        if self.indexes.contains_key(&definition.name) {
            return Err(EngineError::index_already_exists(&definition.name));
        }
        let rows = &self
            .tables
            .get(&definition.table)
            .expect("index validation resolved the table")
            .rows;
        let postings = build_postings(&definition, rows)?;
        self.indexes.insert(
            definition.name.clone(),
            IndexData {
                definition,
                postings,
            },
        );
        Ok(())
    }

    fn drop_index(&mut self, name: &str) -> Result<()> {
        if self.indexes.remove(name).is_none() {
            return Err(EngineError::new(
                "INDEX_NOT_FOUND",
                format!("Index `{name}` is not defined"),
            ));
        }
        Ok(())
    }

    fn index_definition(&self, name: &str) -> Option<IndexDefinition> {
        self.indexes.get(name).map(|index| index.definition.clone())
    }

    fn indexes_for_table(&self, table: &str) -> Result<Vec<IndexDefinition>> {
        if !self.tables.contains_key(table) {
            return Err(EngineError::table_not_found(table));
        }
        Ok(self
            .indexes
            .values()
            .filter(|index| index.definition.table == table)
            .map(|index| index.definition.clone())
            .collect())
    }

    fn lookup_index(&self, table: &str, columns: &[String], key: &Row) -> Result<Option<Vec<Row>>> {
        #[cfg(test)]
        self.lookup_count.set(self.lookup_count.get() + 1);
        let table_data = self
            .tables
            .get(table)
            .ok_or_else(|| EngineError::table_not_found(table))?;
        let Some(index) = self
            .indexes
            .values()
            .find(|index| index.definition.table == table && index.definition.columns == columns)
        else {
            return Ok(None);
        };
        let Some(index_key) = index_key(&index.definition, key)? else {
            return Ok(Some(vec![]));
        };
        let rows = index
            .postings
            .get(&index_key)
            .into_iter()
            .flatten()
            .filter_map(|primary_key| table_data.rows.get(primary_key).cloned())
            .collect();
        Ok(Some(rows))
    }

    fn table_schema(&self, table: &str) -> Result<TableSchema> {
        self.tables
            .get(table)
            .map(|table| table.schema.clone())
            .ok_or_else(|| EngineError::table_not_found(table))
    }

    fn replace_table_unrevisioned(&mut self, table: &str, rows: Vec<Row>) -> Result<()> {
        let current = self
            .tables
            .get(table)
            .ok_or_else(|| EngineError::table_not_found(table))?;
        let mut replacement = BTreeMap::new();

        for row in rows {
            let row = normalize_row(&current.schema, row)?;
            let key = row_key(&current.schema, &row)?;
            if replacement.insert(key, row).is_some() {
                return Err(EngineError::invalid_change(format!(
                    "Snapshot for `{table}` contains a duplicate primary key"
                )));
            }
        }

        let indexes = self.rebuilt_indexes_for_table(table, &replacement)?;
        self.tables
            .get_mut(table)
            .expect("table was checked above")
            .rows = replacement;
        self.indexes = indexes;
        Ok(())
    }

    fn advance_revision(&mut self) -> Result<u64> {
        self.revision = next_revision(self.revision)?;
        Ok(self.revision)
    }

    fn revision(&self) -> u64 {
        self.revision
    }
}

fn validate_index_definition(
    definition: &IndexDefinition,
    tables: &BTreeMap<String, TableData>,
) -> Result<()> {
    if definition.name.trim().is_empty() {
        return Err(EngineError::invalid_schema("An index name cannot be empty"));
    }
    if definition.columns.is_empty() {
        return Err(EngineError::invalid_schema(format!(
            "Index `{}` must contain at least one column",
            definition.name
        )));
    }
    let table = tables
        .get(&definition.table)
        .ok_or_else(|| EngineError::table_not_found(&definition.table))?;
    if table.schema.columns.is_empty() {
        return Err(EngineError::unsupported_sql(format!(
            "Index `{}` requires a typed table catalog",
            definition.name
        )));
    }
    let mut seen = HashSet::new();
    for name in &definition.columns {
        if !seen.insert(name) {
            return Err(EngineError::invalid_schema(format!(
                "Index `{}` names column `{name}` more than once",
                definition.name
            )));
        }
        let column = table
            .schema
            .columns
            .iter()
            .find(|column| column.name == *name)
            .ok_or_else(|| EngineError::column_not_found(name, &definition.table))?;
        if matches!(column.data_type, ColumnType::Float | ColumnType::Json) {
            return Err(EngineError::unsupported_sql(format!(
                "Index `{}` cannot use {} column `{name}`; only boolean, integer, and text columns are supported",
                definition.name,
                column_type_name(column.data_type)
            )));
        }
    }
    Ok(())
}

fn build_postings(
    definition: &IndexDefinition,
    rows: &BTreeMap<String, Row>,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut postings: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (primary_key, row) in rows {
        // PostgreSQL's default UNIQUE semantics treat every key containing NULL
        // as distinct. NULL comparisons cannot use an equality lookup either.
        let Some(key) = index_key(definition, row)? else {
            continue;
        };
        let entries = postings.entry(key).or_default();
        if definition.unique && !entries.is_empty() {
            return Err(EngineError::constraint_violation(format!(
                "Index `{}` would contain duplicate values",
                definition.name
            )));
        }
        entries.insert(primary_key.clone());
    }
    Ok(postings)
}

fn index_key(definition: &IndexDefinition, row: &Row) -> Result<Option<String>> {
    let mut values = Vec::with_capacity(definition.columns.len());
    for column in &definition.columns {
        let value = row.get(column).ok_or_else(|| {
            EngineError::invalid_change(format!(
                "Row for `{}` is missing indexed column `{column}`",
                definition.table
            ))
        })?;
        if value == &Value::Null {
            return Ok(None);
        }
        values.push(value);
    }
    serde_json::to_string(&values).map(Some).map_err(|error| {
        EngineError::invalid_change(format!("Could not encode index key: {error}"))
    })
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

    if schema.columns.is_empty() {
        return Ok(());
    }

    let mut catalog_columns = HashSet::new();
    for column in &schema.columns {
        if column.name.trim().is_empty() {
            return Err(EngineError::invalid_schema(format!(
                "Table `{}` contains an empty column name",
                schema.name
            )));
        }
        if !catalog_columns.insert(column.name.as_str()) {
            return Err(EngineError::invalid_schema(format!(
                "Table `{}` declares column `{}` more than once",
                schema.name, column.name
            )));
        }
        if schema.primary_key.contains(&column.name) && column.nullable {
            return Err(EngineError::invalid_schema(format!(
                "Primary-key column `{}` in `{}` cannot be nullable",
                column.name, schema.name
            )));
        }
        if let Some(default) = &column.default {
            validate_value(column, default, &schema.name).map_err(|error| {
                EngineError::invalid_schema(format!(
                    "Default for column `{}` is invalid: {}",
                    column.name, error.message
                ))
            })?;
        }
    }

    for column in &schema.primary_key {
        if !catalog_columns.contains(column.as_str()) {
            return Err(EngineError::invalid_schema(format!(
                "Primary-key column `{column}` is not declared in table `{}`",
                schema.name
            )));
        }
    }
    Ok(())
}

pub(crate) fn normalize_row(schema: &TableSchema, mut row: Row) -> Result<Row> {
    if schema.columns.is_empty() {
        return Ok(row);
    }

    for name in row.keys() {
        if !schema.columns.iter().any(|column| column.name == *name) {
            return Err(EngineError::column_not_found(name, &schema.name));
        }
    }

    for column in &schema.columns {
        if !row.contains_key(&column.name) {
            let value = column.default.clone().unwrap_or(Value::Null);
            row.insert(column.name.clone(), value);
        }
        validate_value(
            column,
            row.get(&column.name)
                .expect("the catalog column was populated above"),
            &schema.name,
        )?;
    }
    Ok(row)
}

fn validate_value(column: &ColumnDefinition, value: &Value, table: &str) -> Result<()> {
    if value == &Value::Null {
        if column.nullable {
            return Ok(());
        }
        return Err(EngineError::constraint_violation(format!(
            "Column `{}` in `{table}` cannot be null",
            column.name
        )));
    }

    let valid = match column.data_type {
        ColumnType::Boolean => value.is_boolean(),
        ColumnType::Integer => is_javascript_safe_integer(value),
        ColumnType::Float => value.is_number(),
        ColumnType::Text => value.is_string(),
        ColumnType::Json => true,
    };
    if valid {
        Ok(())
    } else {
        Err(EngineError::type_mismatch(format!(
            "Column `{}` in `{table}` expects {}",
            column.name,
            column_type_name(column.data_type)
        )))
    }
}

fn is_javascript_safe_integer(value: &Value) -> bool {
    const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
    value
        .as_u64()
        .is_some_and(|number| number <= MAX_SAFE_INTEGER)
        || value.as_i64().is_some_and(|number| {
            number >= -(MAX_SAFE_INTEGER as i64) && number <= MAX_SAFE_INTEGER as i64
        })
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

pub(crate) fn row_key(schema: &TableSchema, row: &Row) -> Result<String> {
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
                columns: vec![],
            })
            .unwrap();
        storage
    }

    fn typed_storage() -> InMemoryStorage {
        let mut storage = InMemoryStorage::default();
        storage
            .define_table(TableSchema {
                name: "users".to_owned(),
                primary_key: vec!["id".to_owned()],
                columns: vec![
                    ColumnDefinition {
                        name: "id".to_owned(),
                        data_type: ColumnType::Integer,
                        nullable: false,
                        default: None,
                    },
                    ColumnDefinition {
                        name: "email".to_owned(),
                        data_type: ColumnType::Text,
                        nullable: true,
                        default: None,
                    },
                ],
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
                columns: vec![],
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
    fn replace_table_snapshot_atomically_defines_and_populates_a_table() {
        let mut storage = InMemoryStorage::default();
        let outcome = storage
            .replace_table_snapshot(
                TableSchema {
                    name: "posts".to_owned(),
                    primary_key: vec!["id".to_owned()],
                    columns: vec![],
                },
                vec![row(json!({"id": 1, "title": "first"}))],
            )
            .unwrap();

        assert_eq!(
            outcome,
            ApplyOutcome {
                revision: 1,
                tables: vec!["posts".to_owned()],
            }
        );
        assert_eq!(
            storage.scan_table("posts").unwrap(),
            vec![row(json!({"id": 1, "title": "first"}))]
        );
    }

    #[test]
    fn failed_table_snapshot_does_not_leave_an_empty_schema() {
        let mut storage = InMemoryStorage::default();
        let error = storage
            .replace_table_snapshot(
                TableSchema {
                    name: "posts".to_owned(),
                    primary_key: vec!["id".to_owned()],
                    columns: vec![],
                },
                vec![
                    row(json!({"id": 1, "title": "duplicate"})),
                    row(json!({"id": 1, "title": "duplicate"})),
                ],
            )
            .unwrap_err();

        assert_eq!(error.code, "INVALID_CHANGE");
        assert_eq!(storage.revision(), 0);
        assert_eq!(
            storage.scan_table("posts").unwrap_err().code,
            "TABLE_NOT_FOUND"
        );
    }

    #[test]
    fn conflicting_table_snapshot_preserves_existing_schema_rows_and_revision() {
        let mut storage = storage();
        storage
            .replace_table("posts", vec![row(json!({"id": 1, "title": "kept"}))])
            .unwrap();

        let error = storage
            .replace_table_snapshot(
                TableSchema {
                    name: "posts".to_owned(),
                    primary_key: vec!["slug".to_owned()],
                    columns: vec![],
                },
                vec![row(json!({"slug": "replacement"}))],
            )
            .unwrap_err();

        assert_eq!(error.code, "INVALID_SCHEMA");
        assert_eq!(storage.revision(), 1);
        assert_eq!(
            storage.scan_table("posts").unwrap(),
            vec![row(json!({"id": 1, "title": "kept"}))]
        );
        storage
            .define_table(TableSchema {
                name: "posts".to_owned(),
                primary_key: vec!["id".to_owned()],
                columns: vec![],
            })
            .unwrap();
    }

    #[test]
    fn snapshot_round_trips_complete_state_deterministically() {
        let mut storage = InMemoryStorage::default();
        storage
            .define_table(TableSchema {
                name: "empty".to_owned(),
                primary_key: vec!["id".to_owned()],
                columns: vec![],
            })
            .unwrap();
        storage
            .define_table(TableSchema {
                name: "memberships".to_owned(),
                primary_key: vec!["team_id".to_owned(), "user_id".to_owned()],
                columns: vec![],
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
                columns: vec![],
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
            indexes: vec![],
            tables: vec![TableSnapshot {
                schema: TableSchema {
                    name: "posts".to_owned(),
                    primary_key: vec!["id".to_owned()],
                    columns: vec![],
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
            columns: vec![],
        };
        let invalid_snapshots = [
            StorageSnapshot {
                revision: 0,
                indexes: vec![],
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
                indexes: vec![],
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

    #[test]
    fn snapshot_persists_definitions_and_rebuilds_index_postings() {
        let mut storage = typed_storage();
        storage
            .replace_table(
                "users",
                vec![
                    row(json!({"id": 1, "email": "one@example.com"})),
                    row(json!({"id": 2, "email": "two@example.com"})),
                ],
            )
            .unwrap();
        storage
            .define_index(IndexDefinition {
                name: "users_email".to_owned(),
                table: "users".to_owned(),
                columns: vec!["email".to_owned()],
                unique: true,
            })
            .unwrap();

        let bytes = storage.export_snapshot().unwrap();
        let restored = InMemoryStorage::from_snapshot(&bytes).unwrap();
        assert_eq!(restored.export_snapshot().unwrap(), bytes);
        assert_eq!(
            restored
                .lookup_index(
                    "users",
                    &["email".to_owned()],
                    &row(json!({"email": "two@example.com"})),
                )
                .unwrap()
                .unwrap(),
            vec![row(json!({"id": 2, "email": "two@example.com"}))]
        );
    }

    #[test]
    fn previous_snapshot_versions_restore_without_indexes() {
        let storage = typed_storage();
        for version in [1_u16, 2_u16] {
            let mut bytes = storage.export_snapshot().unwrap();
            bytes[8..10].copy_from_slice(&version.to_le_bytes());
            let restored = InMemoryStorage::from_snapshot(&bytes).unwrap();
            assert!(restored.indexes_for_table("users").unwrap().is_empty());
        }
    }

    #[test]
    fn snapshot_rejects_invalid_or_non_unique_index_definitions() {
        let schema = typed_storage().table_schema("users").unwrap();
        let invalid = StorageSnapshot {
            revision: 8,
            tables: vec![TableSnapshot {
                schema,
                rows: vec![
                    row(json!({"id": 1, "email": "same@example.com"})),
                    row(json!({"id": 2, "email": "same@example.com"})),
                ],
            }],
            indexes: vec![IndexDefinition {
                name: "users_email".to_owned(),
                table: "users".to_owned(),
                columns: vec!["email".to_owned()],
                unique: true,
            }],
        };
        assert_eq!(
            InMemoryStorage::from_snapshot(&crate::snapshot::encode(&invalid).unwrap())
                .unwrap_err()
                .code,
            "INVALID_SNAPSHOT"
        );

        let mut duplicate_definitions = invalid;
        duplicate_definitions.tables[0].rows.pop();
        duplicate_definitions
            .indexes
            .push(duplicate_definitions.indexes[0].clone());
        assert_eq!(
            InMemoryStorage::from_snapshot(
                &crate::snapshot::encode(&duplicate_definitions).unwrap()
            )
            .unwrap_err()
            .code,
            "INVALID_SNAPSHOT"
        );
    }
}
