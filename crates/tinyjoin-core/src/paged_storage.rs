use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashSet},
    rc::Rc,
};

#[cfg(test)]
use crate::paged_codec::{
    CatalogHeader, encode_catalog_header_record, encode_catalog_index_record,
};
use crate::{
    ApplyOutcome, Btree, ColumnType, EngineError, ExecuteResult, IndexDefinition, PageDevice,
    PageId, Pager, Result, Row, RowChange, StorageReader, TableDefinition, TreeId, VisitControl,
    VisitOutcome,
    paged_codec::{
        CATALOG_TREE_ID, CatalogIndexRecord, CatalogKey, CatalogTableRecord, FIRST_USER_TREE_ID,
        RecordLayout, StoredRecord, decode_catalog_header_record, decode_catalog_index_record,
        decode_catalog_key, decode_catalog_table_record, encode_catalog_schema,
        encode_catalog_table_record_with_schema, encode_primary_key,
        encode_secondary_index_entry_key, encode_secondary_index_prefix, index_entry_primary_key,
        leading_key_component, secondary_index_entry_matches_prefix, secondary_index_primary_key,
        secondary_index_primary_key_for_definition,
    },
    row::RowRef,
    storage::{KeyOrder, KeyRange, RowWriteUsage, normalize_row, preflight_row_write_set},
};
/// A relational view over the crash-safe paged B-tree store.
///
/// Table definitions and row changes publish directly through the pager's atomic generation
/// switch. The broader SQL/DDL storage driver remains deliberately unavailable until every
/// operation can share that publication path.
pub(crate) struct PagedStorage<D: PageDevice> {
    pager: RefCell<Pager<D>>,
    revision: u64,
    next_tree_id: TreeId,
    // Shared with each script candidate, which copies them only if it changes the catalog.
    tables: Rc<BTreeMap<String, PagedTable>>,
    indexes: Rc<BTreeMap<String, PagedIndex>>,
    recovery_required: bool,
    #[cfg(test)]
    validated_row_count: std::cell::Cell<usize>,
}

#[derive(Clone, Debug)]
pub(crate) struct PagedTable {
    /// The table's columns. [`Self::set_schema`] changes them, keeping `layout` and `catalog` in
    /// step. All three are shared, since the catalog is copied whenever a statement changes it,
    /// and readers hold the schema while they run.
    pub(crate) schema: Rc<TableDefinition>,
    layout: Rc<RecordLayout>,
    catalog: Rc<CatalogSchema>,
    pub(crate) tree_id: TreeId,
    pub(crate) root_page_id: Option<PageId>,
    pub(crate) row_count: usize,
    /// The fingerprint of every row in this table.
    pub(crate) hash: u64,
}

impl PagedTable {
    pub(crate) fn new(
        schema: TableDefinition,
        tree_id: TreeId,
        root_page_id: Option<PageId>,
        row_count: usize,
        hash: u64,
    ) -> Result<Self> {
        Ok(Self {
            layout: Rc::new(RecordLayout::new(&schema)?),
            catalog: Rc::new(CatalogSchema::new(&schema)?),
            schema: Rc::new(schema),
            tree_id,
            root_page_id,
            row_count,
            hash,
        })
    }

    pub(crate) fn set_schema(&mut self, schema: TableDefinition) -> Result<()> {
        self.layout = Rc::new(RecordLayout::new(&schema)?);
        self.catalog = Rc::new(CatalogSchema::new(&schema)?);
        self.schema = Rc::new(schema);
        Ok(())
    }

    /// The fingerprint of the table's columns and primary key, which the database fingerprint
    /// binds to the table's rows.
    pub(crate) fn columns_fingerprint(&self) -> u64 {
        self.catalog.columns_fingerprint
    }

    /// The table's catalog record, around the schema it encoded when the schema was set.
    pub(crate) fn catalog_record(&self) -> Result<(Vec<u8>, Vec<u8>)> {
        encode_catalog_table_record_with_schema(
            &self.schema.name,
            self.tree_id,
            self.root_page_id,
            self.row_count as u64,
            self.hash,
            &self.catalog.encoded,
        )
    }

    /// One of this table's stored entries, read in place.
    pub(crate) fn record<'a>(&'a self, key: &'a [u8], value: &'a [u8]) -> Result<StoredRecord<'a>> {
        StoredRecord::new(&self.schema, &self.layout, key, value)
    }

    /// The type of the leading primary-key column, which orders this table's keys.
    pub(crate) fn leading_key_type(&self) -> ColumnType {
        self.layout.key_types()[0]
    }

    /// The types of `columns`, in the order given.
    pub(crate) fn column_types(&self, columns: &[String]) -> Result<Vec<ColumnType>> {
        columns
            .iter()
            .map(|name| {
                self.schema
                    .columns
                    .iter()
                    .find(|column| column.name == *name)
                    .map(|column| column.data_type)
                    .ok_or_else(|| EngineError::column_not_found(name, &self.schema.name))
            })
            .collect()
    }
}

/// What a table's catalog record and fingerprint need from its schema, which only a schema change
/// changes, so that a commit need not encode the schema again.
#[derive(Debug)]
struct CatalogSchema {
    /// The schema as its catalog record holds it.
    encoded: Vec<u8>,
    /// A fingerprint of the table's column definitions: their names, types, nullability,
    /// defaults, and order, and which of them form the primary key.
    columns_fingerprint: u64,
}

impl CatalogSchema {
    fn new(schema: &TableDefinition) -> Result<Self> {
        let columns = serde_json::json!({
            "columns": schema.columns,
            "primaryKey": schema.primary_key,
        });
        let mut hasher = crate::hash::Hasher::new();
        hasher.write_bytes(&crate::paged_codec::encode_canonical_json(&columns)?);
        Ok(Self {
            encoded: encode_catalog_schema(schema)?,
            columns_fingerprint: hasher.finish(),
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PagedIndex {
    pub(crate) definition: IndexDefinition,
    pub(crate) tree_id: TreeId,
    pub(crate) root_page_id: Option<PageId>,
    pub(crate) entry_count: usize,
}

#[cfg(test)]
struct PagedRowChange {
    next: Option<Row>,
}

/// The three byte estimates protect different allocations and must stay independent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PagedWriteUsage {
    row_write: RowWriteUsage,
    input_bytes: usize,
    prepared_bytes: usize,
    operations: usize,
}

impl PagedWriteUsage {
    /// This usage with `other`'s added, failing as the write-set validator would once any budget
    /// passes its limit.
    pub(crate) fn plus(self, other: Self) -> Result<Self> {
        let usage = Self {
            row_write: self.row_write.plus(other.row_write)?,
            input_bytes: self
                .input_bytes
                .checked_add(other.input_bytes)
                .ok_or_else(batch_too_large)?,
            prepared_bytes: self
                .prepared_bytes
                .checked_add(other.prepared_bytes)
                .ok_or_else(batch_too_large)?,
            operations: self
                .operations
                .checked_add(other.operations)
                .ok_or_else(batch_too_large)?,
        };
        if usage.operations > MAX_PAGED_BATCH_OPERATIONS {
            return Err(operation_limit());
        }
        ensure_batch_bytes(usage.input_bytes)?;
        ensure_batch_bytes(usage.prepared_bytes)?;
        Ok(usage)
    }

    /// This usage without `other`'s, which it includes.
    pub(crate) fn minus(self, other: Self) -> Self {
        Self {
            row_write: self.row_write.minus(other.row_write),
            input_bytes: self.input_bytes - other.input_bytes,
            prepared_bytes: self.prepared_bytes - other.prepared_bytes,
            operations: self.operations - other.operations,
        }
    }
}

/// A unique-index value that one changed row claims, and the committed rows already holding it,
/// by primary key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UniqueClaim {
    pub(crate) tree_id: TreeId,
    pub(crate) prefix: Box<[u8]>,
    pub(crate) owners: Vec<Vec<u8>>,
}

/// What one change adds to a write set: its usage, and the unique-index values it claims.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChangeCost {
    pub(crate) usage: PagedWriteUsage,
    pub(crate) claims: Vec<UniqueClaim>,
}

/// The unique-index values a write set claims.
#[cfg(test)]
pub(crate) type UniquePrefixes = BTreeSet<(TreeId, Box<[u8]>)>;

/// What validating a whole write set at once finds, which a transaction's statement-by-statement
/// totals must equal.
#[cfg(test)]
pub(crate) struct ValidatedRowWrites {
    pub(crate) usage: PagedWriteUsage,
    pub(crate) unique_prefixes: UniquePrefixes,
}

const MAX_PAGED_BATCH_OPERATIONS: usize = 1_000_000;
const MAX_PAGED_BATCH_BYTES: usize = 16 * 1024 * 1024;
type LoadedCatalog = (
    TreeId,
    BTreeMap<String, PagedTable>,
    BTreeMap<String, PagedIndex>,
);

impl<D: PageDevice> PagedStorage<D> {
    /// Opens and validates a previously published paged database.
    pub(crate) fn open(device: D) -> Result<Self> {
        let mut pager = Pager::open_or_create(device)?;
        let revision = pager.database_revision();
        crate::revision::validate_database_revision(revision)
            .map_err(|error| storage_corrupt(error.message))?;
        let Some(catalog_root_page_id) = pager.catalog_root_page_id() else {
            if revision != 0 || pager.active_metadata().superblock.live_data_page_count != 0 {
                return Err(storage_corrupt(
                    "A database without a catalog root must have revision zero and no live data pages",
                ));
            }
            return Ok(Self {
                pager: RefCell::new(pager),
                revision,
                next_tree_id: FIRST_USER_TREE_ID,
                tables: Rc::default(),
                indexes: Rc::default(),
                recovery_required: false,
                #[cfg(test)]
                validated_row_count: std::cell::Cell::new(0),
            });
        };

        let (next_tree_id, tables, indexes) =
            load_and_validate_catalog(&mut pager, catalog_root_page_id)?;
        Ok(Self {
            pager: RefCell::new(pager),
            revision,
            next_tree_id,
            tables: Rc::new(tables),
            indexes: Rc::new(indexes),
            recovery_required: false,
            #[cfg(test)]
            validated_row_count: std::cell::Cell::new(0),
        })
    }

    pub(crate) fn into_device(self) -> D {
        self.pager.into_inner().into_device()
    }

    /// The fingerprint of every row in this database, as published in the superblock.
    #[cfg(test)]
    pub(crate) fn database_hash(&self) -> u64 {
        self.pager.borrow().database_hash()
    }

    pub(crate) fn ensure_readiness(&self) -> Result<()> {
        self.ensure_ready()
    }

    pub(crate) fn execute_script(
        &mut self,
        statements: Vec<crate::statement::Statement>,
    ) -> Result<Vec<ExecuteResult>> {
        self.ensure_ready()?;
        let publication = {
            let mut pager = self.pager.borrow_mut();
            crate::paged_script::execute(
                &mut pager,
                self.revision,
                self.next_tree_id,
                self.tables.clone(),
                self.indexes.clone(),
                statements,
            )
        };
        self.accept_script_publication(publication)
            .map(|(_, results)| results)
    }

    fn accept_script_publication(
        &mut self,
        publication: Result<crate::paged_script::ScriptPublication>,
    ) -> Result<(u64, Vec<ExecuteResult>)> {
        let publication = match publication {
            Ok(publication) => publication,
            Err(error) => {
                if error.code == "RECOVERY_REQUIRED" || self.pager.borrow().is_recovery_required() {
                    self.recovery_required = true;
                }
                return Err(error);
            }
        };
        let crate::paged_script::ScriptPublication {
            committed,
            revision,
            next_tree_id,
            tables,
            indexes,
            results,
        } = publication;
        if committed {
            self.revision = revision;
            self.next_tree_id = next_tree_id;
            self.tables = tables;
            self.indexes = indexes;
        }
        Ok((revision, results))
    }
    #[cfg(test)]
    pub(crate) fn validated_row_count(&self) -> usize {
        self.validated_row_count.get()
    }

    /// Rejects two SQL upserts which target the same canonical page key in one statement.
    ///
    /// SQL planning still uses JSON-shaped logical keys for its in-memory compatibility path, but
    /// typed page keys intentionally canonicalize aliases such as FLOAT `0`, `0.0`, and `-0.0`.
    /// A delete followed by an upsert remains valid for a primary-key spelling change.
    pub(crate) fn validate_sql_row_change_sequence(
        &self,
        input_changes: &[RowChange],
    ) -> Result<()> {
        self.ensure_ready()?;
        let mut upserts = BTreeMap::<String, BTreeSet<Vec<u8>>>::new();
        for change in input_changes {
            let (table_name, input, is_upsert) = match change {
                RowChange::Upsert { table, row } => (table, row, true),
                RowChange::Delete { table, key } => (table, key, false),
            };
            let table = self
                .tables
                .get(table_name)
                .ok_or_else(|| EngineError::table_not_found(table_name))?;
            let key = encode_primary_key(&table.schema, input)?;
            if is_upsert && !upserts.entry(table_name.clone()).or_default().insert(key) {
                return Err(EngineError::constraint_violation(format!(
                    "SQL statement would write canonical primary key in `{table_name}` more than once"
                )));
            }
        }
        Ok(())
    }

    /// Validates one change of a transaction's write set, whose committed row is `base`, and
    /// measures what it adds to the write set's usage exactly as [`Self::validate_row_write_set`]
    /// does for each change. The catalog operations charged once per changed table are left to
    /// [`Self::write_set_usage`], and conflicts between claims to the caller.
    pub(crate) fn change_cost(
        &self,
        table_name: &str,
        change: &RowChange,
        base: Option<&Row>,
    ) -> Result<ChangeCost> {
        self.ensure_ready()?;
        #[cfg(test)]
        self.validated_row_count
            .set(self.validated_row_count.get() + 1);
        let table = self
            .tables
            .get(table_name)
            .ok_or_else(|| EngineError::table_not_found(table_name))?;
        let indexes = self
            .indexes
            .values()
            .filter(|index| index.definition.table == table_name)
            .collect::<Vec<_>>();
        let mut schemas = BTreeMap::new();
        schemas.insert(table_name, &*table.schema);
        let definitions = indexes
            .iter()
            .map(|index| &index.definition)
            .collect::<Vec<_>>();
        let row_write = preflight_row_write_set(
            std::slice::from_ref(change),
            &schemas,
            &definitions,
            RowWriteUsage::default(),
        )?;
        let (input, is_delete) = match change {
            RowChange::Upsert { row, .. } => (row, false),
            RowChange::Delete { key, .. } => (key, true),
        };
        let input_bytes = table_name
            .len()
            .checked_add(estimated_row_bytes(input)?)
            .and_then(|bytes| bytes.checked_add(64))
            .ok_or_else(batch_too_large)?;
        let row = if is_delete {
            input.clone()
        } else {
            normalize_row(&table.schema, input.clone())?
        };
        let key = encode_primary_key(&table.schema, &row)?;
        let base_bytes = base.map_or(Ok(0), estimated_row_bytes)?;
        let next_bytes = if is_delete {
            0
        } else {
            estimated_row_bytes(&row)?
        };
        let mut prepared_bytes = key
            .len()
            .checked_add(base_bytes)
            .and_then(|bytes| bytes.checked_add(next_bytes))
            .and_then(|bytes| bytes.checked_add(96))
            .ok_or_else(batch_too_large)?;
        let mut claims = Vec::new();
        if !is_delete {
            for index in indexes.iter().filter(|index| index.definition.unique) {
                let Some(prefix) =
                    encode_secondary_index_prefix(&table.schema, &index.definition, &row)?
                else {
                    continue;
                };
                prepared_bytes = prepared_bytes
                    .checked_add(prefix.len() + key.len() + 64)
                    .ok_or_else(batch_too_large)?;
                let owners =
                    committed_index_primary_keys(&mut self.pager.borrow_mut(), index, &prefix)?;
                for owner in &owners {
                    prepared_bytes = prepared_bytes
                        .checked_add(owner.len())
                        .ok_or_else(batch_too_large)?;
                }
                claims.push(UniqueClaim {
                    tree_id: index.tree_id,
                    prefix: prefix.into_boxed_slice(),
                    owners,
                });
            }
        }
        ensure_batch_bytes(input_bytes)?;
        ensure_batch_bytes(prepared_bytes)?;
        Ok(ChangeCost {
            usage: PagedWriteUsage {
                row_write,
                input_bytes,
                prepared_bytes,
                operations: 1 + 2 * indexes.len(),
            },
            claims,
        })
    }

    /// The unique-index values a stored row holds, which a claim by another row conflicts with
    /// unless this row gives them up.
    pub(crate) fn unique_values(
        &self,
        table_name: &str,
        row: &Row,
    ) -> Result<Vec<(TreeId, Vec<u8>)>> {
        let table = self
            .tables
            .get(table_name)
            .ok_or_else(|| EngineError::table_not_found(table_name))?;
        let mut values = Vec::new();
        for index in self
            .indexes
            .values()
            .filter(|index| index.definition.table == table_name && index.definition.unique)
        {
            if let Some(prefix) =
                encode_secondary_index_prefix(&table.schema, &index.definition, row)?
            {
                values.push((index.tree_id, prefix));
            }
        }
        Ok(values)
    }

    /// The error for two rows holding one value of the unique index `tree_id`.
    pub(crate) fn unique_violation_in(&self, tree_id: TreeId) -> EngineError {
        let name = self
            .indexes
            .values()
            .find(|index| index.tree_id == tree_id)
            .map_or("unknown", |index| index.definition.name.as_str());
        unique_violation(name)
    }

    /// A write set's complete usage: the sum of its changes' costs, and the catalog operations
    /// charged once for each changed table. Fails once any budget passes its limit.
    pub(crate) fn write_set_usage<'t>(
        &self,
        changes: PagedWriteUsage,
        changed_tables: impl Iterator<Item = &'t str>,
    ) -> Result<PagedWriteUsage> {
        let mut operations = 0usize;
        for table in changed_tables {
            let index_count = self
                .indexes
                .values()
                .filter(|index| index.definition.table == table)
                .count();
            operations = operations
                .checked_add(1 + index_count)
                .ok_or_else(batch_too_large)?;
        }
        changes.plus(PagedWriteUsage {
            operations,
            ..PagedWriteUsage::default()
        })
    }

    /// Validates a whole write set at once: the reference for the totals a transaction keeps as
    /// it stages each statement.
    #[cfg(test)]
    pub(crate) fn validate_row_write_set(
        &self,
        input_changes: &[RowChange],
    ) -> Result<ValidatedRowWrites> {
        self.ensure_ready()?;
        self.validated_row_count
            .set(self.validated_row_count.get() + input_changes.len());
        let mut usage = preflight_batch(input_changes, &self.tables, &self.indexes)?;
        self.validate_sql_row_change_sequence(input_changes)?;
        let mut retained_bytes = usage.prepared_bytes;
        let mut tables = BTreeMap::<String, BTreeMap<Vec<u8>, PagedRowChange>>::new();
        for change in input_changes {
            let (table_name, input, is_delete) = match change {
                RowChange::Upsert { table, row } => (table, row, false),
                RowChange::Delete { table, key } => (table, key, true),
            };
            let table = self
                .tables
                .get(table_name)
                .expect("preflight resolved every changed table");
            let row = if is_delete {
                input.clone()
            } else {
                normalize_row(&table.schema, input.clone())?
            };
            let key = encode_primary_key(&table.schema, &row)?;
            let table_changes = tables.entry(table_name.clone()).or_default();
            if let Some(existing) = table_changes.get_mut(&key) {
                let previous_bytes = existing.next.as_ref().map_or(Ok(0), estimated_row_bytes)?;
                let next = (!is_delete).then_some(row);
                let next_bytes = next.as_ref().map_or(Ok(0), estimated_row_bytes)?;
                retained_bytes = retained_bytes
                    .checked_sub(previous_bytes)
                    .and_then(|bytes| bytes.checked_add(next_bytes))
                    .ok_or_else(batch_too_large)?;
                ensure_batch_bytes(retained_bytes)?;
                existing.next = next;
                continue;
            }

            let old = lookup_encoded_primary_key(&mut self.pager.borrow_mut(), table, &key)?;
            let next = (!is_delete).then_some(row);
            let old_bytes = old.as_ref().map_or(Ok(0), estimated_row_bytes)?;
            let next_bytes = next.as_ref().map_or(Ok(0), estimated_row_bytes)?;
            let change_bytes = key
                .len()
                .checked_add(old_bytes)
                .and_then(|bytes| bytes.checked_add(next_bytes))
                .and_then(|bytes| bytes.checked_add(96))
                .ok_or_else(batch_too_large)?;
            retained_bytes = retained_bytes
                .checked_add(change_bytes)
                .ok_or_else(batch_too_large)?;
            ensure_batch_bytes(retained_bytes)?;
            table_changes.insert(key, PagedRowChange { next });
        }

        let (prepared_bytes, unique_prefixes) =
            self.validate_changed_unique_indexes(&tables, retained_bytes)?;
        usage.prepared_bytes = prepared_bytes;
        Ok(ValidatedRowWrites {
            usage,
            unique_prefixes,
        })
    }

    #[cfg(test)]
    fn validate_changed_unique_indexes(
        &self,
        tables: &BTreeMap<String, BTreeMap<Vec<u8>, PagedRowChange>>,
        mut retained_bytes: usize,
    ) -> Result<(usize, UniquePrefixes)> {
        let mut changed_prefixes = UniquePrefixes::new();
        for (table_name, changes) in tables {
            let table = &self.tables[table_name];
            for index in self
                .indexes
                .values()
                .filter(|index| index.definition.table == *table_name && index.definition.unique)
            {
                for (primary_key, change) in changes {
                    let Some(row) = &change.next else {
                        continue;
                    };
                    let Some(prefix) =
                        encode_secondary_index_prefix(&table.schema, &index.definition, row)?
                    else {
                        continue;
                    };
                    retained_bytes = retained_bytes
                        .checked_add(prefix.len() + primary_key.len() + 64)
                        .ok_or_else(batch_too_large)?;
                    ensure_batch_bytes(retained_bytes)?;
                    let claim = (index.tree_id, prefix.into_boxed_slice());
                    if changed_prefixes.contains(&claim) {
                        return Err(unique_violation(&index.definition.name));
                    }

                    let prefix = claim.1.as_ref();

                    let existing_primary_keys =
                        committed_index_primary_keys(&mut self.pager.borrow_mut(), index, prefix)?;
                    for existing_primary_key in existing_primary_keys {
                        retained_bytes = retained_bytes
                            .checked_add(existing_primary_key.len())
                            .ok_or_else(batch_too_large)?;
                        ensure_batch_bytes(retained_bytes)?;
                        if existing_primary_key == *primary_key {
                            continue;
                        }
                        let existing_moves =
                            if let Some(existing) = changes.get(&existing_primary_key) {
                                match &existing.next {
                                    None => true,
                                    Some(row) => {
                                        encode_secondary_index_prefix(
                                            &table.schema,
                                            &index.definition,
                                            row,
                                        )?
                                        .as_deref()
                                            != Some(prefix)
                                    }
                                }
                            } else {
                                false
                            };
                        if !existing_moves {
                            return Err(unique_violation(&index.definition.name));
                        }
                    }
                    changed_prefixes.insert(claim);
                }
            }
        }
        Ok((retained_bytes, changed_prefixes))
    }

    /// Commits a transaction's staged rows, which remain staged if the commit fails.
    pub(crate) fn commit_transaction(
        &mut self,
        transaction: &crate::paged_transaction::PagedTransaction,
    ) -> Result<ApplyOutcome> {
        self.ensure_ready()?;
        let changes = transaction.changed_rows();
        let publication = {
            let mut pager = self.pager.borrow_mut();
            crate::paged_script::execute_changed_rows(
                &mut pager,
                self.revision,
                self.next_tree_id,
                self.tables.clone(),
                self.indexes.clone(),
                &changes,
            )
        };
        let (revision, _) = self.accept_script_publication(publication)?;
        Ok(ApplyOutcome {
            revision,
            tables: transaction.touched_tables().into_iter().collect(),
            keys: transaction.changed_keys(),
        })
    }

    fn ensure_ready(&self) -> Result<()> {
        if self.recovery_required || self.pager.borrow().is_recovery_required() {
            Err(EngineError::new(
                "RECOVERY_REQUIRED",
                "The paged database has an unknown commit outcome; reopen its page device before continuing",
            ))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
fn preflight_batch(
    changes: &[RowChange],
    tables: &BTreeMap<String, PagedTable>,
    indexes: &BTreeMap<String, PagedIndex>,
) -> Result<PagedWriteUsage> {
    let schemas = tables
        .iter()
        .map(|(name, table)| (name.as_str(), &*table.schema))
        .collect();
    let definitions = indexes
        .values()
        .map(|index| &index.definition)
        .collect::<Vec<_>>();
    let mut usage = PagedWriteUsage::default();
    usage.row_write = preflight_row_write_set(changes, &schemas, &definitions, usage.row_write)?;
    let mut bytes = usage.input_bytes;
    let mut operations = usage.operations;
    let mut changed_tables = BTreeSet::new();
    let mut index_counts = BTreeMap::<&str, usize>::new();
    for index in indexes.values() {
        *index_counts
            .entry(index.definition.table.as_str())
            .or_default() += 1;
    }
    for change in changes {
        let (table, row) = match change {
            RowChange::Upsert { table, row } | RowChange::Delete { table, key: row } => {
                (table, row)
            }
        };
        if !tables.contains_key(table) {
            return Err(EngineError::table_not_found(table));
        }
        let index_count = index_counts.get(table.as_str()).copied().unwrap_or(0);
        operations = operations
            .checked_add(1 + 2 * index_count)
            .ok_or_else(batch_too_large)?;
        if operations > MAX_PAGED_BATCH_OPERATIONS {
            return Err(operation_limit());
        }
        let row_bytes = estimated_row_bytes(row)?;
        bytes = bytes
            .checked_add(table.len())
            .and_then(|bytes| bytes.checked_add(row_bytes))
            .and_then(|bytes| bytes.checked_add(64))
            .ok_or_else(batch_too_large)?;
        ensure_batch_bytes(bytes)?;
        changed_tables.insert(table.as_str());
    }
    let affected_index_count = changed_tables.iter().try_fold(0usize, |count, table| {
        count
            .checked_add(index_counts.get(table).copied().unwrap_or(0))
            .ok_or_else(batch_too_large)
    })?;
    operations = operations
        .checked_add(changed_tables.len())
        .and_then(|operations| operations.checked_add(affected_index_count))
        .ok_or_else(batch_too_large)?;
    if operations > MAX_PAGED_BATCH_OPERATIONS {
        return Err(operation_limit());
    }
    usage.input_bytes = bytes;
    usage.operations = operations;
    Ok(usage)
}

fn operation_limit() -> EngineError {
    EngineError::new(
        "TRANSACTION_TOO_LARGE",
        format!(
            "A paged batch cannot require more than {MAX_PAGED_BATCH_OPERATIONS} row, index, and catalog operations"
        ),
    )
}

#[cfg(test)]
fn lookup_encoded_primary_key<D: PageDevice>(
    pager: &mut Pager<D>,
    table: &PagedTable,
    key: &[u8],
) -> Result<Option<Row>> {
    let Some(root) = table.root_page_id else {
        return Ok(None);
    };
    Btree::get(pager, root, table.tree_id, key)?
        .map(|value| table.record(key, &value)?.to_row())
        .transpose()
}

fn committed_index_primary_keys<D: PageDevice>(
    pager: &mut Pager<D>,
    index: &PagedIndex,
    prefix: &[u8],
) -> Result<Vec<Vec<u8>>> {
    let Some(root) = index.root_page_id else {
        return Ok(Vec::new());
    };
    let mut cursor = Btree::cursor_from(pager, root, index.tree_id, prefix)?;
    let mut primary_keys = Vec::new();
    while let Some((entry_key, value)) = cursor.next_entry(pager)? {
        if !secondary_index_entry_matches_prefix(entry_key, prefix) {
            break;
        }
        if !value.is_empty() {
            return Err(storage_corrupt(format!(
                "Secondary index `{}` contains a non-empty value",
                index.definition.name
            )));
        }
        primary_keys.push(secondary_index_primary_key(entry_key, prefix)?.to_vec());
        if index.definition.unique && primary_keys.len() > 1 {
            return Err(storage_corrupt(format!(
                "Unique index `{}` contains duplicate values",
                index.definition.name
            )));
        }
    }
    Ok(primary_keys)
}

pub(crate) fn adjusted_count(
    current: usize,
    inserted: usize,
    deleted: usize,
    kind: &str,
) -> Result<usize> {
    current
        .checked_sub(deleted)
        .and_then(|count| count.checked_add(inserted))
        .ok_or_else(|| storage_corrupt(format!("A {kind} count overflowed or underflowed")))
}

fn estimated_row_bytes(row: &Row) -> Result<usize> {
    estimated_object_bytes(row, 0)
}

fn estimated_json_bytes(value: &serde_json::Value, depth: usize) -> Result<usize> {
    if depth > 64 {
        return Err(EngineError::invalid_change(
            "A paged batch value cannot exceed 64 levels",
        ));
    }
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) => Ok(8),
        serde_json::Value::Number(_) => Ok(32),
        serde_json::Value::String(value) => value
            .len()
            .checked_mul(6)
            .and_then(|bytes| bytes.checked_add(32))
            .ok_or_else(batch_too_large),
        serde_json::Value::Array(values) => values.iter().try_fold(64usize, |bytes, value| {
            bytes
                .checked_add(estimated_json_bytes(value, depth + 1)?)
                .and_then(|bytes| bytes.checked_add(16))
                .ok_or_else(batch_too_large)
        }),
        serde_json::Value::Object(values) => estimated_object_bytes(values, depth),
    }
}

fn estimated_object_bytes(values: &Row, depth: usize) -> Result<usize> {
    if depth > 64 {
        return Err(EngineError::invalid_change(
            "A paged batch value cannot exceed 64 levels",
        ));
    }
    values.iter().try_fold(64usize, |bytes, (key, value)| {
        let key_bytes = key.len().checked_mul(6).ok_or_else(batch_too_large)?;
        let value_bytes = estimated_json_bytes(value, depth + 1)?;
        bytes
            .checked_add(key_bytes)
            .and_then(|bytes| bytes.checked_add(value_bytes))
            .and_then(|bytes| bytes.checked_add(64))
            .ok_or_else(batch_too_large)
    })
}

pub(crate) fn ensure_batch_bytes(bytes: usize) -> Result<()> {
    if bytes > MAX_PAGED_BATCH_BYTES {
        Err(batch_too_large())
    } else {
        Ok(())
    }
}

pub(crate) fn batch_too_large() -> EngineError {
    EngineError::new(
        "TRANSACTION_TOO_LARGE",
        format!("A paged batch cannot retain more than {MAX_PAGED_BATCH_BYTES} bytes"),
    )
}

pub(crate) fn unique_violation(index: &str) -> EngineError {
    EngineError::constraint_violation(format!("Index `{index}` would contain duplicate values"))
}

impl<D: PageDevice> StorageReader for PagedStorage<D> {
    fn ensure_readable(&self) -> Result<()> {
        self.ensure_ready()
    }

    fn visit_table(
        &self,
        table: &str,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.ensure_ready()?;
        let table = self
            .tables
            .get(table)
            .ok_or_else(|| EngineError::table_not_found(table))?;
        let Some(root) = table.root_page_id else {
            return Ok(VisitOutcome::Complete);
        };
        let mut cursor = Btree::cursor(&mut self.pager.borrow_mut(), root, table.tree_id)?;
        loop {
            let next = cursor.next_entry(&mut self.pager.borrow_mut())?;
            let Some((key, value)) = next else {
                break;
            };
            let row = RowRef::record(table.record(key, &value)?);
            if visitor(&row)? == VisitControl::Stop {
                return Ok(VisitOutcome::Stopped);
            }
        }
        Ok(VisitOutcome::Complete)
    }

    fn visits_in_key_order(&self, _table: &str) -> bool {
        true
    }

    fn visit_table_range(
        &self,
        table: &str,
        range: &KeyRange,
        order: KeyOrder,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.ensure_ready()?;
        let table = self
            .tables
            .get(table)
            .ok_or_else(|| EngineError::table_not_found(table))?;
        let Some(root) = table.root_page_id else {
            return Ok(VisitOutcome::Complete);
        };
        let key_type = table.leading_key_type();
        let mut cursor = match order {
            KeyOrder::Ascending => Btree::cursor_from(
                &mut self.pager.borrow_mut(),
                root,
                table.tree_id,
                range.start(),
            )?,
            KeyOrder::Descending => Btree::cursor_before(
                &mut self.pager.borrow_mut(),
                root,
                table.tree_id,
                range.end().as_deref(),
            )?,
        };
        loop {
            let next = cursor.next_entry(&mut self.pager.borrow_mut())?;
            let Some((key, value)) = next else {
                break;
            };
            if !range.contains(leading_key_component(key, key_type)?) {
                break;
            }
            if visitor(&RowRef::record(table.record(key, &value)?))? == VisitControl::Stop {
                return Ok(VisitOutcome::Stopped);
            }
        }
        Ok(VisitOutcome::Complete)
    }

    fn table_row_count(&self, table: &str) -> Result<usize> {
        self.ensure_ready()?;
        self.tables
            .get(table)
            .map(|table| table.row_count)
            .ok_or_else(|| EngineError::table_not_found(table))
    }

    fn lookup_primary_key(&self, table: &str, key: &Row) -> Result<Option<Row>> {
        self.ensure_ready()?;
        let table = self
            .tables
            .get(table)
            .ok_or_else(|| EngineError::table_not_found(table))?;
        let encoded_key = encode_primary_key(&table.schema, key)?;
        let Some(root) = table.root_page_id else {
            return Ok(None);
        };
        Btree::get(
            &mut self.pager.borrow_mut(),
            root,
            table.tree_id,
            &encoded_key,
        )?
        .map(|value| table.record(&encoded_key, &value)?.to_row())
        .transpose()
    }

    fn index_definition(&self, name: &str) -> Option<IndexDefinition> {
        // StorageReader cannot express failure on this metadata probe. After an ambiguous commit
        // this may report the last confirmed definition, but every operation which can consume it
        // is fallible and rejects through `ensure_ready` until the device is reopened.
        self.indexes.get(name).map(|index| index.definition.clone())
    }

    fn indexes_for_table(&self, table: &str) -> Result<Vec<IndexDefinition>> {
        self.ensure_ready()?;
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

    fn visit_index(
        &self,
        table: &str,
        columns: &[String],
        key: &Row,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<Option<VisitOutcome>> {
        self.ensure_ready()?;
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
        let Some(prefix) =
            encode_secondary_index_prefix(&table_data.schema, &index.definition, key)?
        else {
            return Ok(Some(VisitOutcome::Complete));
        };
        let Some(index_root) = index.root_page_id else {
            return Ok(Some(VisitOutcome::Complete));
        };
        let table_root = table_data.root_page_id.ok_or_else(|| {
            storage_corrupt(format!(
                "Non-empty index `{}` references empty table `{table}`",
                index.definition.name
            ))
        })?;

        let mut cursor = Btree::cursor_from(
            &mut self.pager.borrow_mut(),
            index_root,
            index.tree_id,
            &prefix,
        )?;
        loop {
            let next = cursor.next_entry(&mut self.pager.borrow_mut())?;
            let Some((entry_key, value)) = next else {
                break;
            };
            if !secondary_index_entry_matches_prefix(entry_key, &prefix) {
                break;
            }
            if !value.is_empty() {
                return Err(storage_corrupt(format!(
                    "Secondary index `{}` contains a non-empty value",
                    index.definition.name
                )));
            }
            let primary_key = secondary_index_primary_key_for_definition(
                &table_data.schema,
                &index.definition,
                entry_key,
            )?;
            if secondary_index_primary_key(entry_key, &prefix)? != primary_key {
                return Err(storage_corrupt(format!(
                    "Secondary index `{}` tuple boundary is inconsistent",
                    index.definition.name
                )));
            }
            let row_value = Btree::get(
                &mut self.pager.borrow_mut(),
                table_root,
                table_data.tree_id,
                primary_key,
            )?
            .ok_or_else(|| {
                storage_corrupt(format!(
                    "Secondary index `{}` contains a dangling primary key",
                    index.definition.name
                ))
            })?;
            let row = RowRef::record(table_data.record(primary_key, &row_value)?);
            if visitor(&row)? == VisitControl::Stop {
                return Ok(Some(VisitOutcome::Stopped));
            }
        }
        Ok(Some(VisitOutcome::Complete))
    }

    fn visit_index_range(
        &self,
        table: &str,
        columns: &[String],
        range: &KeyRange,
        limit: usize,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<Option<VisitOutcome>> {
        self.ensure_ready()?;
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
        let Some(index_root) = index.root_page_id else {
            return Ok(Some(VisitOutcome::Complete));
        };
        let table_root = table_data.root_page_id.ok_or_else(|| {
            storage_corrupt(format!(
                "Non-empty index `{}` references empty table `{table}`",
                index.definition.name
            ))
        })?;
        let types = table_data.column_types(columns)?;
        // Rows are visited in primary-key order, as a table visit would find them.
        let mut primary_keys = BTreeSet::new();
        {
            let mut pager = self.pager.borrow_mut();
            let mut cursor =
                Btree::cursor_from(&mut pager, index_root, index.tree_id, range.start())?;
            while let Some((entry, value)) = cursor.next_entry(&mut pager)? {
                if !range.contains(leading_key_component(entry, types[0])?) {
                    break;
                }
                if primary_keys.len() == limit {
                    return Ok(None);
                }
                primary_keys.insert(ranged_index_primary_key(
                    &index.definition,
                    entry,
                    &value,
                    &types,
                )?);
            }
        }
        for primary_key in &primary_keys {
            let value = Btree::get(
                &mut self.pager.borrow_mut(),
                table_root,
                table_data.tree_id,
                primary_key,
            )?
            .ok_or_else(|| dangling_index_entry(&index.definition))?;
            if visitor(&RowRef::record(table_data.record(primary_key, &value)?))?
                == VisitControl::Stop
            {
                return Ok(Some(VisitOutcome::Stopped));
            }
        }
        Ok(Some(VisitOutcome::Complete))
    }

    fn table_schema(&self, table: &str) -> Result<Rc<TableDefinition>> {
        self.ensure_ready()?;
        self.tables
            .get(table)
            .map(|table| Rc::clone(&table.schema))
            .ok_or_else(|| EngineError::table_not_found(table))
    }

    fn revision(&self) -> u64 {
        // The trait is infallible: return the last confirmed revision, not a claim about an
        // ambiguously published commit. Callers must reopen after `RECOVERY_REQUIRED`.
        self.revision
    }
}

fn load_and_validate_catalog<D: PageDevice>(
    pager: &mut Pager<D>,
    catalog_root_page_id: PageId,
) -> Result<LoadedCatalog> {
    let mut header = None;
    let mut table_records = BTreeMap::new();
    let mut index_records = BTreeMap::new();
    let mut cursor = Btree::validating_cursor(pager, catalog_root_page_id, CATALOG_TREE_ID)?;
    while let Some((key, value)) = cursor.next(pager)? {
        match decode_catalog_key(&key)? {
            CatalogKey::Header => {
                if header
                    .replace(decode_catalog_header_record(&key, &value)?)
                    .is_some()
                {
                    return Err(storage_corrupt("The catalog contains multiple headers"));
                }
            }
            CatalogKey::Table(name) => {
                let record = decode_catalog_table_record(&key, &value)?;
                if table_records.insert(name.clone(), record).is_some() {
                    return Err(storage_corrupt(format!(
                        "The catalog contains table `{name}` more than once"
                    )));
                }
            }
            CatalogKey::Index(name) => {
                let record = decode_catalog_index_record(&key, &value)?;
                if index_records.insert(name.clone(), record).is_some() {
                    return Err(storage_corrupt(format!(
                        "The catalog contains index `{name}` more than once"
                    )));
                }
            }
        }
    }
    let header = header.ok_or_else(|| storage_corrupt("The catalog header is missing"))?;
    if header.table_count as usize != table_records.len()
        || header.index_count as usize != index_records.len()
    {
        return Err(storage_corrupt(format!(
            "Catalog header counts ({}, {}) do not match its table and index records ({}, {})",
            header.table_count,
            header.index_count,
            table_records.len(),
            index_records.len()
        )));
    }

    let mut tree_ids = BTreeSet::new();
    tree_ids.insert(CATALOG_TREE_ID);
    for record in table_records.values() {
        validate_catalog_tree_id(record.tree_id, header.next_tree_id, &mut tree_ids)?;
    }
    for record in index_records.values() {
        validate_catalog_tree_id(record.tree_id, header.next_tree_id, &mut tree_ids)?;
        validate_index_against_catalog(&record.definition, &table_records)?;
    }
    if tree_ids.len() != 1 + table_records.len() + index_records.len() {
        return Err(storage_corrupt("Catalog tree IDs are not unique"));
    }

    for record in table_records.values() {
        validate_table_tree(pager, record)?;
    }
    for record in index_records.values() {
        validate_index_tree(pager, record, &table_records)?;
    }

    let mut tables = BTreeMap::new();
    for (name, record) in table_records {
        let row_count = usize::try_from(record.row_count)
            .map_err(|_| storage_corrupt("A table row count cannot fit in memory"))?;
        let table = PagedTable::new(
            record.schema,
            record.tree_id,
            record.root_page_id,
            row_count,
            record.hash,
        )?;
        tables.insert(name, table);
    }
    let mut indexes = BTreeMap::new();
    for (name, record) in index_records {
        let entry_count = usize::try_from(record.entry_count)
            .map_err(|_| storage_corrupt("An index entry count cannot fit in memory"))?;
        let index = PagedIndex {
            definition: record.definition,
            tree_id: record.tree_id,
            root_page_id: record.root_page_id,
            entry_count,
        };
        indexes.insert(name, index);
    }
    Ok((header.next_tree_id, tables, indexes))
}

fn validate_catalog_tree_id(
    tree_id: TreeId,
    next_tree_id: TreeId,
    seen: &mut BTreeSet<TreeId>,
) -> Result<()> {
    if !(FIRST_USER_TREE_ID..next_tree_id).contains(&tree_id) {
        return Err(storage_corrupt(format!(
            "Catalog tree ID {tree_id} is outside its declared range"
        )));
    }
    if !seen.insert(tree_id) {
        return Err(storage_corrupt(format!(
            "Catalog tree ID {tree_id} is used more than once"
        )));
    }
    Ok(())
}

fn validate_index_against_catalog(
    definition: &IndexDefinition,
    tables: &BTreeMap<String, CatalogTableRecord>,
) -> Result<()> {
    let table = tables.get(&definition.table).ok_or_else(|| {
        storage_corrupt(format!(
            "Index `{}` references missing table `{}`",
            definition.name, definition.table
        ))
    })?;
    for column in &definition.columns {
        let column = table
            .schema
            .columns
            .iter()
            .find(|candidate| candidate.name == *column)
            .ok_or_else(|| {
                storage_corrupt(format!(
                    "Index `{}` references missing column `{column}`",
                    definition.name
                ))
            })?;
        if matches!(
            column.data_type,
            crate::ColumnType::Float | crate::ColumnType::Json
        ) {
            return Err(storage_corrupt(format!(
                "Index `{}` uses an unsupported column type",
                definition.name
            )));
        }
    }
    Ok(())
}

fn validate_table_tree<D: PageDevice>(
    pager: &mut Pager<D>,
    record: &CatalogTableRecord,
) -> Result<()> {
    let Some(root) = record.root_page_id else {
        return if record.row_count == 0 {
            Ok(())
        } else {
            Err(storage_corrupt(format!(
                "Table `{}` claims {} rows without a tree root",
                record.schema.name, record.row_count
            )))
        };
    };
    let layout = RecordLayout::new(&record.schema)?;
    let mut count = 0_u64;
    let mut cursor = Btree::validating_cursor(pager, root, record.tree_id)?;
    while let Some((key, value)) = cursor.next_entry(pager)? {
        validated_row(&record.schema, &layout, key, &value)?;
        count = count
            .checked_add(1)
            .ok_or_else(|| storage_corrupt("A table row count overflowed"))?;
    }
    if count != record.row_count {
        return Err(storage_corrupt(format!(
            "Table `{}` catalog count {} does not match its {count} rows",
            record.schema.name, record.row_count
        )));
    }
    Ok(())
}

fn validate_index_tree<D: PageDevice>(
    pager: &mut Pager<D>,
    record: &CatalogIndexRecord,
    tables: &BTreeMap<String, CatalogTableRecord>,
) -> Result<()> {
    let table = tables.get(&record.definition.table).ok_or_else(|| {
        storage_corrupt(format!(
            "Index `{}` references missing table `{}`",
            record.definition.name, record.definition.table
        ))
    })?;
    let expected_entry_count = expected_index_entry_count(pager, table, &record.definition)?;
    if record.entry_count != expected_entry_count {
        return Err(storage_corrupt(format!(
            "Index `{}` catalog count {} does not match its {expected_entry_count} eligible table rows",
            record.definition.name, record.entry_count
        )));
    }
    let Some(root) = record.root_page_id else {
        return Ok(());
    };
    let table_root = table.root_page_id.ok_or_else(|| {
        storage_corrupt(format!(
            "Non-empty index `{}` references an empty table",
            record.definition.name
        ))
    })?;
    let layout = RecordLayout::new(&table.schema)?;
    let mut count = 0_u64;
    let mut unique_prefixes = HashSet::new();
    let mut cursor = Btree::validating_cursor(pager, root, record.tree_id)?;
    while let Some((entry_key, value)) = cursor.next(pager)? {
        if !value.is_empty() {
            return Err(storage_corrupt(format!(
                "Secondary index `{}` contains a non-empty value",
                record.definition.name
            )));
        }
        let primary_key = secondary_index_primary_key_for_definition(
            &table.schema,
            &record.definition,
            &entry_key,
        )?;
        let row_value =
            Btree::get(pager, table_root, table.tree_id, primary_key)?.ok_or_else(|| {
                storage_corrupt(format!(
                    "Secondary index `{}` contains a dangling primary key",
                    record.definition.name
                ))
            })?;
        // Every table row was validated strictly before its indexes are checked.
        let row = StoredRecord::new(&table.schema, &layout, primary_key, &row_value)?.to_row()?;
        let expected = encode_secondary_index_entry_key(&table.schema, &record.definition, &row)?;
        if expected.as_deref() != Some(entry_key.as_slice()) {
            return Err(storage_corrupt(format!(
                "Secondary index `{}` entry does not match its table row",
                record.definition.name
            )));
        }
        if record.definition.unique {
            let prefix = encode_secondary_index_prefix(&table.schema, &record.definition, &row)?
                .ok_or_else(|| {
                    storage_corrupt(format!(
                        "Unique index `{}` contains a NULL entry",
                        record.definition.name
                    ))
                })?;
            if !unique_prefixes.insert(prefix) {
                return Err(storage_corrupt(format!(
                    "Unique index `{}` contains duplicate values",
                    record.definition.name
                )));
            }
        }
        count = count
            .checked_add(1)
            .ok_or_else(|| storage_corrupt("An index entry count overflowed"))?;
    }
    if count != record.entry_count {
        return Err(storage_corrupt(format!(
            "Index `{}` catalog count {} does not match its {count} entries",
            record.definition.name, record.entry_count
        )));
    }
    Ok(())
}

fn expected_index_entry_count<D: PageDevice>(
    pager: &mut Pager<D>,
    table: &CatalogTableRecord,
    definition: &IndexDefinition,
) -> Result<u64> {
    let Some(root) = table.root_page_id else {
        return Ok(0);
    };
    // Catalog opening has already validated every table row and its declared count.
    // A unique index without nullable columns must contain one entry per row.
    // Its entry validation also rejects duplicate prefixes and checks every key
    // against its row, proving complete coverage without scanning the table again.
    // Retain the scan for indexes without uniqueness: counts alone do not prove
    // coverage if a corrupt tree contains duplicate entries across leaves.
    if definition.unique
        && definition.columns.iter().all(|name| {
            table
                .schema
                .columns
                .iter()
                .any(|column| column.name == *name && !column.nullable)
        })
    {
        return Ok(table.row_count);
    }
    let layout = RecordLayout::new(&table.schema)?;
    let mut count = 0_u64;
    let mut cursor = Btree::cursor(pager, root, table.tree_id)?;
    while let Some((key, value)) = cursor.next_entry(pager)? {
        let row = StoredRecord::new(&table.schema, &layout, key, &value)?.to_row()?;
        if encode_secondary_index_entry_key(&table.schema, definition, &row)?.is_some() {
            count = count
                .checked_add(1)
                .ok_or_else(|| storage_corrupt("An expected index entry count overflowed"))?;
        }
    }
    Ok(count)
}

/// Decodes a stored row, rejecting any encoding this engine would not have written and any row
/// that does not match its schema. Opening a database checks every row this way. Reads check only
/// what reading safely requires, since pages are verified as they are loaded.
pub(crate) fn validated_row(
    schema: &TableDefinition,
    layout: &RecordLayout,
    key: &[u8],
    value: &[u8],
) -> Result<Row> {
    let row = StoredRecord::new(schema, layout, key, value)?.to_row_strictly()?;
    let normalized = normalize_row(schema, row.clone()).map_err(|error| {
        storage_corrupt(format!(
            "Stored row in `{}` does not match its schema: {}",
            schema.name, error.message
        ))
    })?;
    if normalized != row {
        return Err(storage_corrupt(format!(
            "Stored row in `{}` is not normalized",
            schema.name
        )));
    }
    Ok(row)
}

/// The primary key of an index entry within a range visit, checking only what the visit relies on.
pub(crate) fn ranged_index_primary_key(
    definition: &IndexDefinition,
    entry: &[u8],
    value: &[u8],
    types: &[ColumnType],
) -> Result<Vec<u8>> {
    if !value.is_empty() {
        return Err(storage_corrupt(format!(
            "Secondary index `{}` contains a non-empty value",
            definition.name
        )));
    }
    Ok(index_entry_primary_key(entry, types)?.to_vec())
}

pub(crate) fn dangling_index_entry(definition: &IndexDefinition) -> EngineError {
    storage_corrupt(format!(
        "Secondary index `{}` contains a dangling primary key",
        definition.name
    ))
}

pub(crate) fn limit_error(message: impl Into<String>) -> EngineError {
    EngineError::new("STORAGE_LIMIT", message)
}

pub(crate) fn storage_corrupt(message: impl Into<String>) -> EngineError {
    EngineError::new("STORAGE_CORRUPT", message)
}

#[cfg(test)]
mod tests {
    use crate::hash::EMPTY_HASH;
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        ColumnDefinition, ColumnType, Engine, InMemoryStorage, MemoryPageDevice, PageDevice,
        QueryResult,
    };

    fn row(value: Value) -> Row {
        value.as_object().unwrap().clone()
    }

    fn schema(name: &str, columns: &[(&str, ColumnType, bool)]) -> TableDefinition {
        TableDefinition {
            name: name.to_owned(),
            primary_key: vec!["id".to_owned()],
            columns: columns
                .iter()
                .map(|(name, data_type, nullable)| ColumnDefinition {
                    name: (*name).to_owned(),
                    data_type: *data_type,
                    nullable: *nullable,
                    default: None,
                })
                .collect(),
        }
    }

    fn source() -> InMemoryStorage {
        let mut engine = Engine::default();
        for sql in fixture_sql() {
            engine.execute_sql(sql, &[]).unwrap();
        }
        engine.into_storage()
    }

    fn fixture_sql() -> [&'static str; 6] {
        [
            "CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
            "CREATE TABLE posts (\
                id INTEGER PRIMARY KEY, \
                author_id INTEGER NOT NULL, \
                state TEXT, \
                rank INTEGER NOT NULL\
            )",
            "INSERT INTO authors (id, name) VALUES (1, 'Ada'), (2, 'Lin')",
            "INSERT INTO posts (id, author_id, state, rank) VALUES \
                (10, 1, 'draft', 2), (11, 1, 'live', 1), (12, 2, NULL, 3)",
            "CREATE INDEX posts_state_rank ON posts (state, rank)",
            "CREATE INDEX posts_author ON posts (author_id)",
        ]
    }

    fn execute_sql<D: PageDevice>(
        storage: &mut PagedStorage<D>,
        sql: &str,
        params: &[Value],
    ) -> Result<ExecuteResult> {
        let statement = crate::statement::parse(sql, params)?;
        storage
            .execute_script(vec![statement])?
            .pop()
            .ok_or_else(|| EngineError::new("INTERNAL_ERROR", "SQL produced no result"))
    }

    /// Builds tests through the SQL publication path.
    fn page_native_fixture<D: PageDevice>(device: D) -> Result<PagedStorage<D>> {
        let mut storage = PagedStorage::open(device)?;
        for sql in fixture_sql() {
            execute_sql(&mut storage, sql, &[])?;
        }
        Ok(storage)
    }

    fn query_pair(source: &InMemoryStorage, sql: &str) -> (QueryResult, QueryResult) {
        let expected = Engine::new(source.clone()).query_sql(sql, &[]).unwrap();
        let paged = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
        let actual = Engine::new(paged).query_sql(sql, &[]).unwrap();
        (expected, actual)
    }

    #[test]
    fn write_set_usage_charges_catalog_operations_once_per_table() {
        let mut storage = PagedStorage::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        for sql in [
            "CREATE TABLE first_table (id INTEGER PRIMARY KEY, value TEXT)",
            "CREATE UNIQUE INDEX first_unique ON first_table (value)",
            "CREATE INDEX first_secondary ON first_table (value)",
            "CREATE TABLE second_table (id INTEGER PRIMARY KEY, value TEXT)",
            "CREATE UNIQUE INDEX second_unique ON second_table (value)",
        ] {
            execute_sql(&mut storage, sql, &[]).unwrap();
        }
        let change = |table: &str, id: i64, value: &str| RowChange::Upsert {
            table: table.to_owned(),
            row: row(json!({"id": id, "value": value})),
        };
        let changes = vec![
            change("first_table", 1, "one"),
            change("first_table", 2, "two"),
            change("second_table", 1, "one"),
        ];
        let mut usage = PagedWriteUsage::default();
        let mut claims = UniquePrefixes::new();
        for change in &changes {
            let RowChange::Upsert { table, .. } = change else {
                unreachable!()
            };
            let cost = storage.change_cost(table, change, None).unwrap();
            // One row and two maintained indexes, or one row and one index.
            assert_eq!(
                cost.usage.operations,
                if table == "first_table" { 5 } else { 3 }
            );
            usage = usage.plus(cost.usage).unwrap();
            claims.extend(
                cost.claims
                    .into_iter()
                    .map(|claim| (claim.tree_id, claim.prefix)),
            );
        }
        let usage = storage
            .write_set_usage(usage, ["first_table", "second_table"].into_iter())
            .unwrap();
        // Then each table once, with its two or one catalog indexes.
        assert_eq!(usage.operations, 2 * 5 + 3 + 3 + 2);
        let complete = storage.validate_row_write_set(&changes).unwrap();
        assert_eq!(usage, complete.usage);
        assert_eq!(claims, complete.unique_prefixes);
    }

    #[test]
    fn write_set_usage_preserves_exact_paged_byte_and_operation_boundaries() {
        let mut storage = PagedStorage::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        for sql in [
            "CREATE TABLE items (id INTEGER PRIMARY KEY, value TEXT)",
            "CREATE UNIQUE INDEX items_unique ON items (value)",
        ] {
            execute_sql(&mut storage, sql, &[]).unwrap();
        }
        let change = RowChange::Upsert {
            table: "items".to_owned(),
            row: row(json!({"id": 1, "value": "one"})),
        };
        let addition = storage.change_cost("items", &change, None).unwrap().usage;
        // Seed each independent budget just below its limit, leaving the other two empty.
        // This checks that the totals enforce cumulative, inclusive bounds.
        for budget in 0..3 {
            for extra in [0, 1] {
                let mut usage = PagedWriteUsage::default();
                match budget {
                    0 => usage.input_bytes = MAX_PAGED_BATCH_BYTES - addition.input_bytes + extra,
                    1 => {
                        usage.prepared_bytes =
                            MAX_PAGED_BATCH_BYTES - addition.prepared_bytes + extra
                    }
                    _ => {
                        usage.operations = MAX_PAGED_BATCH_OPERATIONS - addition.operations + extra
                    }
                }
                let result = usage.plus(addition);
                if extra == 0 {
                    let result = result.unwrap();
                    match budget {
                        0 => assert_eq!(result.input_bytes, MAX_PAGED_BATCH_BYTES),
                        1 => assert_eq!(result.prepared_bytes, MAX_PAGED_BATCH_BYTES),
                        _ => assert_eq!(result.operations, MAX_PAGED_BATCH_OPERATIONS),
                    }
                } else {
                    assert_eq!(result.err().unwrap().code, "TRANSACTION_TOO_LARGE");
                }
            }
        }
    }

    #[test]
    fn builds_reopens_and_preserves_query_aggregate_and_join_results() {
        let source = source();
        for sql in [
            "SELECT id, state FROM posts ORDER BY id",
            "SELECT state, COUNT(*) AS rows FROM posts GROUP BY state ORDER BY state NULLS LAST",
            "SELECT p.id AS post_id, a.name AS author FROM posts p JOIN authors a ON p.author_id = a.id ORDER BY p.id",
        ] {
            let (expected, actual) = query_pair(&source, sql);
            assert_eq!(actual.rows, expected.rows, "{sql}");
        }

        let paged = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
        let revision = paged.revision();
        let device = paged.into_device();
        let reopened = PagedStorage::open(device).unwrap();
        assert_eq!(reopened.revision(), revision);
        assert_eq!(reopened.table_row_count("posts").unwrap(), 3);
        assert_eq!(
            Engine::new(reopened)
                .query_sql("SELECT id FROM posts ORDER BY id", &[])
                .unwrap()
                .rows,
            vec![
                row(json!({"id": 10})),
                row(json!({"id": 11})),
                row(json!({"id": 12}))
            ]
        );
    }

    #[test]
    fn primary_and_secondary_lookup_cover_absent_empty_composite_and_null_keys() {
        let paged = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
        assert_eq!(
            paged
                .lookup_primary_key("posts", &row(json!({"id": 11})))
                .unwrap()
                .unwrap()["state"],
            "live"
        );
        assert!(
            paged
                .lookup_primary_key("posts", &row(json!({"id": 99})))
                .unwrap()
                .is_none()
        );
        assert!(
            paged
                .visit_index(
                    "posts",
                    &["missing".to_owned()],
                    &row(json!({"missing": "x"})),
                    &mut |_| Ok(VisitControl::Continue),
                )
                .unwrap()
                .is_none()
        );

        let mut ids = Vec::new();
        assert_eq!(
            paged
                .visit_index(
                    "posts",
                    &["state".to_owned(), "rank".to_owned()],
                    &row(json!({"state": "live", "rank": 1})),
                    &mut |row| {
                        ids.push(row.to_row()?["id"].as_i64().unwrap());
                        Ok(VisitControl::Continue)
                    },
                )
                .unwrap(),
            Some(VisitOutcome::Complete)
        );
        assert_eq!(ids, vec![11]);
        assert_eq!(
            paged
                .visit_index(
                    "posts",
                    &["state".to_owned(), "rank".to_owned()],
                    &row(json!({"state": "missing", "rank": 1})),
                    &mut |_| panic!("an absent key must not visit rows"),
                )
                .unwrap(),
            Some(VisitOutcome::Complete)
        );
        assert_eq!(
            paged
                .visit_index(
                    "posts",
                    &["state".to_owned(), "rank".to_owned()],
                    &row(json!({"state": null, "rank": 3})),
                    &mut |_| panic!("NULL equality must not visit rows"),
                )
                .unwrap(),
            Some(VisitOutcome::Complete)
        );
    }

    #[test]
    fn visitors_can_stop_without_materializing_the_remaining_table_or_index() {
        let paged = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut table_visits = 0;
        assert_eq!(
            paged
                .visit_table("posts", &mut |_| {
                    table_visits += 1;
                    assert!(
                        paged
                            .lookup_primary_key("authors", &row(json!({"id": 1})))
                            .unwrap()
                            .is_some()
                    );
                    Ok(VisitControl::Stop)
                })
                .unwrap(),
            VisitOutcome::Stopped
        );
        assert_eq!(table_visits, 1);

        let mut index_visits = 0;
        assert_eq!(
            paged
                .visit_index(
                    "posts",
                    &["author_id".to_owned()],
                    &row(json!({"author_id": 1})),
                    &mut |_| {
                        index_visits += 1;
                        Ok(VisitControl::Stop)
                    },
                )
                .unwrap(),
            Some(VisitOutcome::Stopped)
        );
        assert_eq!(index_visits, 1);
    }

    #[test]
    fn validators_reject_nonzero_counts_without_tree_roots() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let table = CatalogTableRecord {
            schema: schema("missing_rows", &[("id", ColumnType::Integer, false)]),
            tree_id: FIRST_USER_TREE_ID,
            root_page_id: None,
            row_count: 1,
            hash: crate::hash::EMPTY_HASH,
        };
        assert_eq!(
            validate_table_tree(&mut pager, &table).unwrap_err().code,
            "STORAGE_CORRUPT"
        );

        let index = CatalogIndexRecord {
            definition: IndexDefinition {
                name: "missing_entries".to_owned(),
                table: "missing_rows".to_owned(),
                columns: vec!["id".to_owned()],
                unique: false,
            },
            tree_id: FIRST_USER_TREE_ID + 1,
            root_page_id: None,
            entry_count: 1,
        };
        assert_eq!(
            validate_index_tree(&mut pager, &index, &BTreeMap::new())
                .unwrap_err()
                .code,
            "STORAGE_CORRUPT"
        );
    }

    #[test]
    fn empty_tables_and_indexes_reopen_without_allocated_roots() {
        let mut paged = PagedStorage::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        execute_sql(
            &mut paged,
            "CREATE TABLE empty (id INTEGER PRIMARY KEY, label TEXT)",
            &[],
        )
        .unwrap();
        execute_sql(&mut paged, "CREATE INDEX empty_label ON empty (label)", &[]).unwrap();
        assert_eq!(paged.table_row_count("empty").unwrap(), 0);
        let reopened = PagedStorage::open(paged.into_device()).unwrap();
        assert_eq!(reopened.scan_table("empty").unwrap(), Vec::<Row>::new());
        assert_eq!(
            reopened
                .visit_index(
                    "empty",
                    &["label".to_owned()],
                    &row(json!({"label": "none"})),
                    &mut |_| Ok(VisitControl::Continue),
                )
                .unwrap(),
            Some(VisitOutcome::Complete)
        );
    }

    #[test]
    fn reopening_rejects_catalog_count_mismatch() {
        let paged = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
        let device = paged.into_device();
        let mut pager = Pager::open_or_create(device).unwrap();
        let root = pager.catalog_root_page_id().unwrap();
        let revision = pager.database_revision();
        let mut transaction = pager.begin_write().unwrap();
        let (key, value) = encode_catalog_header_record(&CatalogHeader {
            next_tree_id: FIRST_USER_TREE_ID + 6,
            table_count: 3,
            index_count: 2,
        })
        .unwrap();
        let root = Btree::upsert(&mut transaction, root, CATALOG_TREE_ID, &key, &value)
            .unwrap()
            .root_page_id;
        transaction
            .commit(revision, EMPTY_HASH, Some(root))
            .unwrap();
        let error = match PagedStorage::open(pager.into_device()) {
            Ok(_) => panic!("a mismatched catalog count must fail closed"),
            Err(error) => error,
        };
        assert_eq!(error.code, "STORAGE_CORRUPT");
    }

    #[test]
    fn reopening_rejects_an_index_on_a_float_or_missing_column() {
        // Keys are encoded without checking their schema again, so the catalog must refuse an
        // index that CREATE INDEX would have refused.
        for column in ["rating", "missing"] {
            let mut paged = PagedStorage::open(MemoryPageDevice::new(0).unwrap()).unwrap();
            execute_sql(
                &mut paged,
                "CREATE TABLE items (id INTEGER PRIMARY KEY, rating FLOAT)",
                &[],
            )
            .unwrap();
            let tree_id = paged.next_tree_id;
            let mut pager = Pager::open_or_create(paged.into_device()).unwrap();
            let mut root = pager.catalog_root_page_id().unwrap();
            let revision = pager.database_revision();
            let mut transaction = pager.begin_write().unwrap();
            for (key, value) in [
                encode_catalog_header_record(&CatalogHeader {
                    next_tree_id: tree_id + 1,
                    table_count: 1,
                    index_count: 1,
                })
                .unwrap(),
                encode_catalog_index_record(&CatalogIndexRecord {
                    definition: IndexDefinition {
                        name: "items_lookup".to_owned(),
                        table: "items".to_owned(),
                        columns: vec![column.to_owned()],
                        unique: false,
                    },
                    tree_id,
                    root_page_id: None,
                    entry_count: 0,
                })
                .unwrap(),
            ] {
                root = Btree::upsert(&mut transaction, root, CATALOG_TREE_ID, &key, &value)
                    .unwrap()
                    .root_page_id;
            }
            transaction
                .commit(revision, EMPTY_HASH, Some(root))
                .unwrap();
            let error = match PagedStorage::open(pager.into_device()) {
                Ok(_) => panic!("an index on column `{column}` must fail closed"),
                Err(error) => error,
            };
            assert_eq!(error.code, "STORAGE_CORRUPT");
            assert!(error.message.contains("items_lookup"), "{}", error.message);
        }
    }

    #[test]
    fn rootless_live_pages_are_not_an_empty_database() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut transaction = pager.begin_write().unwrap();
        Btree::create(&mut transaction, 99).unwrap();
        transaction.commit(0, EMPTY_HASH, None).unwrap();
        assert_eq!(pager.active_metadata().superblock.live_data_page_count, 1);
        let device = pager.into_device();

        let error = match PagedStorage::open(device.clone()) {
            Ok(_) => panic!("rootless live pages must not open as an empty database"),
            Err(error) => error,
        };
        assert_eq!(error.code, "STORAGE_CORRUPT");

        let error = match page_native_fixture(device) {
            Ok(_) => panic!("rootless live pages must not be initialized"),
            Err(error) => error,
        };
        assert_eq!(error.code, "STORAGE_CORRUPT");
    }

    #[test]
    fn reopening_rejects_an_index_missing_eligible_table_rows() {
        for index_name in ["posts_author", "posts_rank"] {
            let mut paged = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
            execute_sql(
                &mut paged,
                "CREATE UNIQUE INDEX posts_rank ON posts (rank)",
                &[],
            )
            .unwrap();
            let index = paged.indexes.get(index_name).unwrap().clone();
            let device = paged.into_device();
            let mut pager = Pager::open_or_create(device).unwrap();
            let catalog_root = pager.catalog_root_page_id().unwrap();
            let revision = pager.database_revision();
            let mut transaction = pager.begin_write().unwrap();
            let (key, value) = encode_catalog_index_record(&CatalogIndexRecord {
                definition: index.definition,
                tree_id: index.tree_id,
                root_page_id: None,
                entry_count: 0,
            })
            .unwrap();
            let catalog_root = Btree::upsert(
                &mut transaction,
                catalog_root,
                CATALOG_TREE_ID,
                &key,
                &value,
            )
            .unwrap()
            .root_page_id;
            transaction
                .commit(revision, EMPTY_HASH, Some(catalog_root))
                .unwrap();

            let error = match PagedStorage::open(pager.into_device()) {
                Ok(_) => panic!("an incomplete secondary index must fail closed"),
                Err(error) => error,
            };
            assert_eq!(error.code, "STORAGE_CORRUPT");
        }
    }

    #[test]
    fn reopening_validates_required_and_nullable_index_counts() {
        let mut paged = PagedStorage::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        for sql in [
            "CREATE TABLE items (id INTEGER PRIMARY KEY, category TEXT NOT NULL, rank INTEGER NOT NULL, label TEXT)",
            "INSERT INTO items VALUES (1, 'same', 1, NULL), (2, 'same', 2, 'present'), (3, 'other', 3, NULL)",
            "CREATE INDEX items_category ON items (category)",
            "CREATE UNIQUE INDEX items_rank ON items (rank)",
            "CREATE UNIQUE INDEX items_category_rank ON items (category, rank)",
            "CREATE UNIQUE INDEX items_category_label ON items (category, label)",
        ] {
            execute_sql(&mut paged, sql, &[]).unwrap();
        }
        let expected = paged.scan_table("items").unwrap();
        let reopened = PagedStorage::open(paged.into_device()).unwrap();
        assert_eq!(reopened.scan_table("items").unwrap(), expected);
        for (index, entry_count) in [
            ("items_category", 3),
            ("items_rank", 3),
            ("items_category_rank", 3),
            ("items_category_label", 1),
        ] {
            assert_eq!(reopened.indexes[index].entry_count, entry_count);
        }
        let mut matches = Vec::new();
        reopened
            .visit_index(
                "items",
                &["category".to_owned(), "label".to_owned()],
                &row(json!({"category": "same", "label": "present"})),
                &mut |item| {
                    matches.push(item.to_row()?);
                    Ok(VisitControl::Continue)
                },
            )
            .unwrap();
        assert_eq!(matches, vec![expected[1].clone()]);
    }

    #[test]
    fn reopening_rejects_null_in_a_required_index_column() {
        let mut paged = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
        execute_sql(
            &mut paged,
            "CREATE UNIQUE INDEX posts_rank ON posts (rank)",
            &[],
        )
        .unwrap();
        let table = paged.tables["posts"].clone();
        let mut pager = Pager::open_or_create(paged.into_device()).unwrap();
        let catalog_root = pager.catalog_root_page_id().unwrap();
        let revision = pager.database_revision();
        let mut transaction = pager.begin_write().unwrap();
        let invalid_row = row(json!({"id": 10, "author_id": 1, "state": "draft", "rank": null}));
        let primary_key = encode_primary_key(&table.schema, &invalid_row).unwrap();
        let row_value = crate::paged_codec::encode_row(&table.schema, &invalid_row).unwrap();
        let table_root = Btree::upsert(
            &mut transaction,
            table.root_page_id.unwrap(),
            table.tree_id,
            &primary_key,
            &row_value,
        )
        .unwrap()
        .root_page_id;
        let (key, value) = crate::paged_codec::encode_catalog_table_record(&CatalogTableRecord {
            schema: TableDefinition::clone(&table.schema),
            tree_id: table.tree_id,
            root_page_id: Some(table_root),
            row_count: table.row_count as u64,
            hash: table.hash,
        })
        .unwrap();
        let catalog_root = Btree::upsert(
            &mut transaction,
            catalog_root,
            CATALOG_TREE_ID,
            &key,
            &value,
        )
        .unwrap()
        .root_page_id;
        transaction
            .commit(revision, EMPTY_HASH, Some(catalog_root))
            .unwrap();
        let error = match PagedStorage::open(pager.into_device()) {
            Ok(_) => panic!("required index columns must still reject stored NULLs"),
            Err(error) => error,
        };
        assert_eq!(error.code, "STORAGE_CORRUPT");
    }

    #[test]
    fn reopening_rejects_duplicate_required_unique_index_prefixes() {
        let paged = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
        // Both posts by author 1 have valid, distinct entry keys and the catalog
        // count is correct. Claiming uniqueness must still reject their shared prefix.
        let mut index = paged.indexes["posts_author"].clone();
        index.definition.unique = true;
        let mut pager = Pager::open_or_create(paged.into_device()).unwrap();
        let catalog_root = pager.catalog_root_page_id().unwrap();
        let revision = pager.database_revision();
        let (key, value) = encode_catalog_index_record(&CatalogIndexRecord {
            definition: index.definition,
            tree_id: index.tree_id,
            root_page_id: index.root_page_id,
            entry_count: index.entry_count as u64,
        })
        .unwrap();
        let mut transaction = pager.begin_write().unwrap();
        let catalog_root = Btree::upsert(
            &mut transaction,
            catalog_root,
            CATALOG_TREE_ID,
            &key,
            &value,
        )
        .unwrap()
        .root_page_id;
        transaction
            .commit(revision, EMPTY_HASH, Some(catalog_root))
            .unwrap();
        let error = match PagedStorage::open(pager.into_device()) {
            Ok(_) => panic!("the correct entry count must not hide duplicate unique values"),
            Err(error) => error,
        };
        assert_eq!(error.code, "STORAGE_CORRUPT");
    }

    #[test]
    fn reopening_rejects_a_required_index_entry_mismatching_its_row() {
        let mut paged = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
        execute_sql(
            &mut paged,
            "CREATE UNIQUE INDEX posts_rank ON posts (rank)",
            &[],
        )
        .unwrap();
        let table = paged.tables["posts"].clone();
        let index = paged.indexes["posts_rank"].clone();
        let mut pager = Pager::open_or_create(paged.into_device()).unwrap();
        let catalog_root = pager.catalog_root_page_id().unwrap();
        let revision = pager.database_revision();
        let original_row = row(json!({"id": 10, "author_id": 1, "state": "draft", "rank": 2}));
        let mut mismatched_row = original_row.clone();
        mismatched_row.insert("rank".to_owned(), json!(99));
        let original_key =
            encode_secondary_index_entry_key(&table.schema, &index.definition, &original_row)
                .unwrap()
                .unwrap();
        let mismatched_key =
            encode_secondary_index_entry_key(&table.schema, &index.definition, &mismatched_row)
                .unwrap()
                .unwrap();
        let mut transaction = pager.begin_write().unwrap();
        let index_root = Btree::delete(
            &mut transaction,
            index.root_page_id.unwrap(),
            index.tree_id,
            &original_key,
        )
        .unwrap()
        .root_page_id;
        let index_root = Btree::upsert(
            &mut transaction,
            index_root.unwrap(),
            index.tree_id,
            &mismatched_key,
            &[],
        )
        .unwrap()
        .root_page_id;
        let (key, value) = encode_catalog_index_record(&CatalogIndexRecord {
            definition: index.definition,
            tree_id: index.tree_id,
            root_page_id: Some(index_root),
            entry_count: index.entry_count as u64,
        })
        .unwrap();
        let catalog_root = Btree::upsert(
            &mut transaction,
            catalog_root,
            CATALOG_TREE_ID,
            &key,
            &value,
        )
        .unwrap()
        .root_page_id;
        transaction
            .commit(revision, EMPTY_HASH, Some(catalog_root))
            .unwrap();
        let error = match PagedStorage::open(pager.into_device()) {
            Ok(_) => panic!("an index entry with the correct count must still match its row"),
            Err(error) => error,
        };
        assert_eq!(error.code, "STORAGE_CORRUPT");
    }
}
