use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashSet},
};

use crate::{
    ApplyOutcome, Btree, Change, ChangeBatch, EngineError, InMemoryStorage, IndexDefinition,
    PageDevice, PageId, Pager, Result, Row, StorageReader, TableSchema, TreeId, VisitControl,
    VisitOutcome,
    paged_codec::{
        CATALOG_TREE_ID, CatalogHeader, CatalogIndexRecord, CatalogKey, CatalogTableRecord,
        FIRST_USER_TREE_ID, MAX_CATALOG_INDEXES, MAX_CATALOG_TABLES, MAX_PAGED_VALUE_BYTES,
        MAX_STORED_COUNT, MAX_TREE_ID, decode_catalog_header_record, decode_catalog_index_record,
        decode_catalog_key, decode_catalog_table_record, decode_row, encode_catalog_header_record,
        encode_catalog_index_key, encode_catalog_index_record, encode_catalog_table_key,
        encode_catalog_table_record, encode_primary_key, encode_row,
        encode_secondary_index_entry_key, encode_secondary_index_prefix,
        secondary_index_entry_matches_prefix, secondary_index_primary_key,
        secondary_index_primary_key_for_definition,
    },
    paged_schema::{
        AddColumnPlan, DroppedTree, ReplacementIndex, SchemaDropPlan, TableReplacementPlan,
        publish_added_column, publish_schema_drop, publish_table_replacement,
    },
    storage::{
        normalize_row, preflight_change_batch, preflight_row_write_set, schema_with_added_column,
        validate_added_column, validate_index_columns_for_schema, validate_index_definition_shape,
        validate_primary_storage_key_bound, validate_schema, validated_index_key,
    },
};

/// A relational view over the crash-safe paged B-tree store.
///
/// Table definitions and row changes publish directly through the pager's atomic generation
/// switch. The broader SQL/DDL storage driver remains deliberately unavailable until every
/// operation can share that publication path. [`Self::from_in_memory`] is a deterministic one-shot
/// importer.
pub struct PagedStorage<D: PageDevice> {
    pager: RefCell<Pager<D>>,
    revision: u64,
    next_tree_id: TreeId,
    tables: BTreeMap<String, PagedTable>,
    indexes: BTreeMap<String, PagedIndex>,
    recovery_required: bool,
}

#[derive(Clone, Debug)]
struct PagedTable {
    schema: TableSchema,
    tree_id: TreeId,
    root_page_id: Option<PageId>,
    row_count: usize,
}

#[derive(Clone, Debug)]
struct PagedIndex {
    definition: IndexDefinition,
    tree_id: TreeId,
    root_page_id: Option<PageId>,
    entry_count: usize,
}

/// A semantically validated row write-set based on one committed paged revision.
///
/// The future SQL transaction overlay can retain statement deltas and prepare this object only at
/// commit time, then share the exact publication path used by replication batches.
pub(crate) struct PagedRowWriteSet {
    base_revision: u64,
    tables: BTreeMap<String, BTreeMap<Vec<u8>, PagedRowChange>>,
}

struct PagedRowChange {
    old: Option<Row>,
    next: Option<Row>,
}

const MAX_PAGED_BATCH_OPERATIONS: usize = 1_000_000;
const MAX_PAGED_BATCH_BYTES: usize = 16 * 1024 * 1024;
type RowWritePreflight = for<'a> fn(
    &[Change],
    &BTreeMap<&'a str, &'a TableSchema>,
    &[&'a IndexDefinition],
) -> Result<()>;
type LoadedCatalog = (
    TreeId,
    BTreeMap<String, PagedTable>,
    BTreeMap<String, PagedIndex>,
);

impl<D: PageDevice> PagedStorage<D> {
    /// Opens and validates a previously published paged database.
    pub fn open(device: D) -> Result<Self> {
        let mut pager = Pager::open_or_create(device)?;
        let revision = pager.database_revision();
        crate::revision::validate_database_revision(revision)
            .map_err(|error| storage_corrupt(error.message))?;
        let Some(catalog_root_page_id) = pager.catalog_root_page_id() else {
            if revision != 0
                || pager.applied_journal_sequence() != 0
                || pager.active_metadata().superblock.live_data_page_count != 0
            {
                return Err(storage_corrupt(
                    "A database without a catalog root must have revision and journal sequence zero and no live data pages",
                ));
            }
            return Ok(Self {
                pager: RefCell::new(pager),
                revision,
                next_tree_id: FIRST_USER_TREE_ID,
                tables: BTreeMap::new(),
                indexes: BTreeMap::new(),
                recovery_required: false,
            });
        };

        let (next_tree_id, tables, indexes) =
            load_and_validate_catalog(&mut pager, catalog_root_page_id)?;
        Ok(Self {
            pager: RefCell::new(pager),
            revision,
            next_tree_id,
            tables,
            indexes,
            recovery_required: false,
        })
    }

    /// Imports an in-memory database into one atomic paged generation.
    ///
    /// Tree IDs and insertion order are deterministic: tables and indexes are ordered by name,
    /// with all table trees allocated before index trees. The target device must not already hold
    /// a published catalog.
    pub fn from_in_memory(device: D, source: &InMemoryStorage) -> Result<Self> {
        Self::from_in_memory_with_journal_sequence(device, source, 0)
    }

    /// Imports an in-memory database and its recovered journal watermark in one paged generation.
    ///
    /// The watermark records the final legacy journal sequence incorporated into this database.
    /// Later page-native publications preserve it without consulting an external journal.
    pub fn from_in_memory_with_journal_sequence(
        device: D,
        source: &InMemoryStorage,
        applied_journal_sequence: u64,
    ) -> Result<Self> {
        let mut pager = Pager::open_or_create(device)?;
        if pager.catalog_root_page_id().is_some()
            || pager.database_revision() != 0
            || pager.applied_journal_sequence() != 0
            || pager.active_metadata().superblock.live_data_page_count != 0
        {
            return Err(EngineError::new(
                "DATABASE_NOT_EMPTY",
                "Paged import requires a device without a published database",
            ));
        }
        let revision = crate::revision::validate_database_revision(source.revision())?;

        // Reject catalog-wide bounds before cloning schemas or retaining any source rows. Table
        // rows remain borrowed and are streamed into the one pager candidate below.
        let bounds = preflight_import_catalog_bounds(
            source.table_names().count(),
            source.index_names().count(),
        )?;
        let metadata_bytes = preflight_import_metadata(source, &bounds)?;
        let mut next_id = FIRST_USER_TREE_ID;
        let mut table_inputs = Vec::with_capacity(bounds.table_count as usize);
        for name in source.table_names() {
            let tree_id = next_id;
            next_id = advance_import_tree_id(next_id)?;
            let schema = source.table_schema_ref(name)?;
            let rows = source.table_rows(name)?;
            let row_count = u64::try_from(rows.len())
                .map_err(|_| limit_error("An imported table contains too many rows"))?;
            if row_count > MAX_STORED_COUNT {
                return Err(limit_error(format!(
                    "An imported table cannot contain more than {MAX_STORED_COUNT} rows"
                )));
            }
            table_inputs.push(TableInput {
                schema,
                rows,
                row_count,
                tree_id,
                index_count: 0,
                unique_index_count: 0,
            });
        }

        let mut index_inputs = Vec::with_capacity(bounds.index_count as usize);
        for name in source.index_names() {
            let tree_id = next_id;
            next_id = advance_import_tree_id(next_id)?;
            let definition = source.index_definition_ref(name).ok_or_else(|| {
                storage_corrupt(format!("In-memory index `{name}` has no definition"))
            })?;
            let table_index = table_inputs
                .binary_search_by(|input| input.schema.name.as_str().cmp(&definition.table))
                .map_err(|_| {
                    storage_corrupt(format!(
                        "Index `{}` references missing table `{}`",
                        definition.name, definition.table
                    ))
                })?;
            let table = &mut table_inputs[table_index];
            validate_index_columns_for_schema(definition, table.schema)?;
            table.index_count = table
                .index_count
                .checked_add(1)
                .ok_or_else(batch_too_large)?;
            table.unique_index_count = table
                .unique_index_count
                .checked_add(usize::from(definition.unique))
                .ok_or_else(batch_too_large)?;
            index_inputs.push(IndexInput {
                definition,
                tree_id,
            });
        }
        debug_assert_eq!(next_id, bounds.next_tree_id);
        let header_record = encode_catalog_header_record(&CatalogHeader {
            next_tree_id: bounds.next_tree_id,
            table_count: bounds.table_count,
            index_count: bounds.index_count,
        })?;
        preflight_import_operations(&table_inputs, index_inputs.len())?;
        preflight_import_rows(metadata_bytes, &table_inputs, &index_inputs)?;

        let mut transaction = pager.begin_write()?;
        let mut table_records = Vec::with_capacity(table_inputs.len());
        for input in &table_inputs {
            let mut root = None;
            for row in input.rows.values() {
                let key = encode_primary_key(input.schema, row)?;
                if let Some(root) = root {
                    let mut existing = Btree::cursor_from_in_transaction(
                        &mut transaction,
                        root,
                        input.tree_id,
                        &key,
                    )?;
                    if let Some((existing_key, _)) =
                        existing.next_in_transaction(&mut transaction)?
                        && existing_key == key
                    {
                        return Err(EngineError::constraint_violation(format!(
                            "Rows in `{}` collide after canonical primary-key encoding",
                            input.schema.name
                        )));
                    }
                }
                let current = match root {
                    Some(root) => root,
                    None => Btree::create(&mut transaction, input.tree_id)?,
                };
                root = Some(Btree::upsert(
                    &mut transaction,
                    current,
                    input.tree_id,
                    &key,
                    &encode_row(row)?,
                )?);
            }
            table_records.push(CatalogTableRecord {
                schema: input.schema.clone(),
                tree_id: input.tree_id,
                root_page_id: root,
                row_count: input.row_count,
            });
        }

        let mut index_records = Vec::with_capacity(index_inputs.len());
        for input in &index_inputs {
            let definition = input.definition;
            let table = &table_inputs[table_inputs
                .binary_search_by(|candidate| candidate.schema.name.as_str().cmp(&definition.table))
                .expect("the import preflight resolved every index table")];
            let mut root = None;
            let mut entry_count = 0_u64;
            for row in table.rows.values() {
                let Some(key) = encode_secondary_index_entry_key(table.schema, definition, row)?
                else {
                    continue;
                };
                if definition.unique {
                    let prefix = encode_secondary_index_prefix(table.schema, definition, row)?
                        .expect("an encoded index entry always has a prefix");
                    if let Some(root) = root {
                        let mut existing = Btree::cursor_from_in_transaction(
                            &mut transaction,
                            root,
                            input.tree_id,
                            &prefix,
                        )?;
                        if let Some((existing_key, existing_value)) =
                            existing.next_in_transaction(&mut transaction)?
                        {
                            if !existing_value.is_empty() {
                                return Err(storage_corrupt(format!(
                                    "Imported secondary index `{}` contains a non-empty value",
                                    definition.name
                                )));
                            }
                            if secondary_index_entry_matches_prefix(&existing_key, &prefix) {
                                return Err(EngineError::constraint_violation(format!(
                                    "Index `{}` would contain duplicate values",
                                    definition.name
                                )));
                            }
                        }
                    }
                }
                let current = match root {
                    Some(root) => root,
                    None => Btree::create(&mut transaction, input.tree_id)?,
                };
                root = Some(Btree::upsert(
                    &mut transaction,
                    current,
                    input.tree_id,
                    &key,
                    &[],
                )?);
                entry_count = entry_count
                    .checked_add(1)
                    .ok_or_else(|| limit_error("An imported index contains too many entries"))?;
            }
            index_records.push(CatalogIndexRecord {
                definition: definition.clone(),
                tree_id: input.tree_id,
                root_page_id: root,
                entry_count,
            });
        }

        debug_assert!(metadata_bytes <= MAX_PAGED_BATCH_BYTES);

        let mut catalog_root = Btree::create(&mut transaction, CATALOG_TREE_ID)?;
        let (key, value) = header_record;
        catalog_root = Btree::upsert(
            &mut transaction,
            catalog_root,
            CATALOG_TREE_ID,
            &key,
            &value,
        )?;
        drop(key);
        drop(value);
        for record in &table_records {
            let (key, value) = encode_catalog_table_record(record)?;
            catalog_root = Btree::upsert(
                &mut transaction,
                catalog_root,
                CATALOG_TREE_ID,
                &key,
                &value,
            )?;
        }
        for record in &index_records {
            let (key, value) = encode_catalog_index_record(record)?;
            catalog_root = Btree::upsert(
                &mut transaction,
                catalog_root,
                CATALOG_TREE_ID,
                &key,
                &value,
            )?;
        }
        transaction.commit(revision, applied_journal_sequence, Some(catalog_root))?;

        // The published catalog is authoritative. Release every borrowed plan and owned model
        // clone before loading its decoded representation so the two catalog models never overlap.
        drop(table_records);
        drop(index_records);
        drop(index_inputs);
        drop(table_inputs);
        let (next_tree_id, tables, indexes) = load_and_validate_catalog(&mut pager, catalog_root)?;
        Ok(Self {
            pager: RefCell::new(pager),
            revision,
            next_tree_id,
            tables,
            indexes,
            recovery_required: false,
        })
    }

    pub fn into_device(self) -> D {
        self.pager.into_inner().into_device()
    }

    /// Returns the legacy journal sequence incorporated into the active paged generation.
    pub fn applied_journal_sequence(&self) -> u64 {
        self.pager.borrow().applied_journal_sequence()
    }

    pub(crate) fn ensure_readiness(&self) -> Result<()> {
        self.ensure_ready()
    }

    /// Atomically defines one table without advancing the database revision.
    ///
    /// Schema-only initialization deliberately matches [`InMemoryStorage`]: it is durable, but it
    /// does not represent a row-data revision. Defining the identical schema again is a no-op.
    pub fn define_table(&mut self, schema: TableSchema) -> Result<()> {
        self.define_tables(vec![schema])
    }

    /// Atomically defines a collection of tables without advancing the database revision.
    ///
    /// Calling this on a rootless device, including with an empty collection, publishes the empty
    /// catalog header required by later page-native mutations. A failed definition batch publishes
    /// none of its schemas.
    pub fn define_tables(&mut self, schemas: Vec<TableSchema>) -> Result<()> {
        self.define_tables_with_publication(schemas).map(|_| ())
    }

    /// Defines initialization schemas and reports whether one pager generation was published.
    ///
    /// This is the page-native bridge seam for distinguishing a durable catalog publication from
    /// an identical-schema no-op even though initialization deliberately leaves the logical
    /// database revision unchanged.
    pub(crate) fn define_tables_with_publication(
        &mut self,
        schemas: Vec<TableSchema>,
    ) -> Result<bool> {
        self.publish_table_definitions(schemas, false)
    }

    /// Defines a SQL-created table and advances the database revision in the same pager generation.
    pub(crate) fn create_table_and_advance(&mut self, schema: TableSchema) -> Result<u64> {
        let changed = self.publish_table_definitions(vec![schema], true)?;
        debug_assert!(
            changed,
            "SQL CREATE TABLE is planned only for a missing table"
        );
        Ok(self.revision)
    }

    fn publish_table_definitions(
        &mut self,
        schemas: Vec<TableSchema>,
        advance_revision: bool,
    ) -> Result<bool> {
        self.ensure_ready()?;
        if schemas.len() > MAX_CATALOG_TABLES as usize {
            return Err(limit_error(format!(
                "A table-definition batch cannot contain more than {MAX_CATALOG_TABLES} schemas"
            )));
        }

        let mut additions = BTreeMap::<String, TableSchema>::new();
        for schema in schemas {
            let existing = self
                .tables
                .get(&schema.name)
                .map(|table| &table.schema)
                .or_else(|| additions.get(&schema.name));
            if let Some(existing) = existing {
                if existing == &schema {
                    continue;
                }
                return Err(EngineError::invalid_schema(format!(
                    "Table `{}` is already defined with a different schema",
                    schema.name
                )));
            }
            validate_schema(&schema)?;
            additions.insert(schema.name.clone(), schema);
        }
        let initialize_catalog = self.pager.borrow().catalog_root_page_id().is_none();
        if additions.is_empty() && !initialize_catalog {
            return Ok(false);
        }

        let mut next_tree_id = self.next_tree_id;
        let table_count = self
            .tables
            .len()
            .checked_add(additions.len())
            .ok_or_else(|| limit_error("The catalog table count overflowed"))?;
        if table_count > MAX_CATALOG_TABLES as usize {
            return Err(limit_error(format!(
                "A catalog cannot contain more than {MAX_CATALOG_TABLES} tables"
            )));
        }
        let mut new_tables = Vec::with_capacity(additions.len());
        let mut encoded_records = Vec::with_capacity(additions.len());
        let mut retained_bytes = 0usize;
        for schema in additions.into_values() {
            let tree_id = next_tree_id;
            next_tree_id = next_tree_id
                .checked_add(1)
                .ok_or_else(|| limit_error("The catalog tree ID range is exhausted"))?;
            let record = CatalogTableRecord {
                schema: schema.clone(),
                tree_id,
                root_page_id: None,
                row_count: 0,
            };
            let encoded = encode_catalog_table_record(&record)?;
            retained_bytes = retained_bytes
                .checked_add(encoded.0.len())
                .and_then(|bytes| bytes.checked_add(encoded.1.len()))
                .and_then(|bytes| bytes.checked_add(64))
                .ok_or_else(batch_too_large)?;
            ensure_batch_bytes(retained_bytes)?;
            encoded_records.push(encoded);
            new_tables.push((
                schema.name.clone(),
                PagedTable {
                    schema,
                    tree_id,
                    root_page_id: None,
                    row_count: 0,
                },
            ));
        }

        let header = CatalogHeader {
            next_tree_id,
            table_count: u32::try_from(table_count)
                .map_err(|_| limit_error("The catalog contains too many tables"))?,
            index_count: u32::try_from(self.indexes.len())
                .map_err(|_| limit_error("The catalog contains too many indexes"))?,
        };
        // Encode every application-level value before allocating candidate pages.
        let header = encode_catalog_header_record(&header)?;
        let revision = if advance_revision {
            crate::revision::next_database_revision(self.revision)?
        } else {
            self.revision
        };

        let mut pager = self.pager.borrow_mut();
        let applied_journal_sequence = pager.applied_journal_sequence();
        let existing_catalog_root = pager.catalog_root_page_id();
        let mut transaction = pager.begin_write()?;
        let result = (|| {
            let mut catalog_root = match existing_catalog_root {
                Some(root) => root,
                None => Btree::create(&mut transaction, CATALOG_TREE_ID)?,
            };
            catalog_root = Btree::upsert(
                &mut transaction,
                catalog_root,
                CATALOG_TREE_ID,
                &header.0,
                &header.1,
            )?;
            for (key, value) in &encoded_records {
                catalog_root =
                    Btree::upsert(&mut transaction, catalog_root, CATALOG_TREE_ID, key, value)?;
            }
            Ok(catalog_root)
        })();
        let catalog_root = match result {
            Ok(catalog_root) => catalog_root,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        if let Err(error) =
            transaction.commit(revision, applied_journal_sequence, Some(catalog_root))
        {
            if error.code == "RECOVERY_REQUIRED" || pager.is_recovery_required() {
                self.recovery_required = true;
            }
            return Err(error);
        }
        drop(pager);

        self.tables.extend(new_tables);
        self.next_tree_id = next_tree_id;
        self.revision = revision;
        Ok(true)
    }

    /// Builds and publishes one secondary index without collecting its source table.
    ///
    /// The table cursor and the new index share one pager candidate, so the index tree, catalog
    /// record, allocator high-water mark, and database revision become visible atomically.
    pub(crate) fn create_index_and_advance(&mut self, definition: IndexDefinition) -> Result<u64> {
        self.ensure_ready()?;
        if self.indexes.contains_key(&definition.name) {
            return Err(EngineError::index_already_exists(&definition.name));
        }
        validate_index_definition_shape(&definition)?;
        let table = self
            .tables
            .get(&definition.table)
            .ok_or_else(|| EngineError::table_not_found(&definition.table))?;
        validate_index_columns_for_schema(&definition, &table.schema)?;
        if self.indexes.len() >= MAX_CATALOG_INDEXES as usize {
            return Err(limit_error(format!(
                "A catalog cannot contain more than {MAX_CATALOG_INDEXES} indexes"
            )));
        }

        let tree_id = self.next_tree_id;
        let next_tree_id = tree_id
            .checked_add(1)
            .ok_or_else(|| limit_error("The catalog tree ID range is exhausted"))?;
        let revision = crate::revision::next_database_revision(self.revision)?;
        let index_count = u32::try_from(self.indexes.len() + 1)
            .map_err(|_| limit_error("The catalog contains too many indexes"))?;
        let header = encode_catalog_header_record(&CatalogHeader {
            next_tree_id,
            table_count: u32::try_from(self.tables.len())
                .map_err(|_| limit_error("The catalog contains too many tables"))?,
            index_count,
        })?;
        // Validate and bound all caller-controlled catalog content before allocating a page.
        let provisional_record = encode_catalog_index_record(&CatalogIndexRecord {
            definition: definition.clone(),
            tree_id,
            root_page_id: None,
            entry_count: 0,
        })?;
        let catalog_work_bytes = header
            .0
            .len()
            .checked_add(header.1.len())
            .and_then(|bytes| bytes.checked_add(provisional_record.0.len()))
            .and_then(|bytes| bytes.checked_add(provisional_record.1.len()))
            .and_then(|bytes| bytes.checked_add(256))
            .ok_or_else(batch_too_large)?;
        ensure_batch_bytes(catalog_work_bytes)?;

        let table_schema = table.schema.clone();
        let table_tree_id = table.tree_id;
        let table_root_page_id = table.root_page_id;
        let mut pager = self.pager.borrow_mut();
        let applied_journal_sequence = pager.applied_journal_sequence();
        let catalog_root = pager.catalog_root_page_id().ok_or_else(|| {
            storage_corrupt("A non-empty paged catalog must have a published root")
        })?;
        let mut transaction = pager.begin_write()?;
        let result = (|| {
            let mut index_root_page_id = None;
            let mut entry_count = 0usize;
            let mut operations = 2usize; // Catalog header and new index record.
            if let Some(table_root_page_id) = table_root_page_id {
                let mut rows = Btree::cursor_in_transaction(
                    &mut transaction,
                    table_root_page_id,
                    table_tree_id,
                )?;
                while let Some((primary_key, value)) = rows.next_in_transaction(&mut transaction)? {
                    operations = operations.checked_add(1).ok_or_else(batch_too_large)?;
                    if operations > MAX_PAGED_BATCH_OPERATIONS {
                        return Err(operation_limit());
                    }
                    let row = validated_row(&table_schema, &primary_key, &value)?;
                    let Some(index_key) =
                        encode_secondary_index_entry_key(&table_schema, &definition, &row)?
                    else {
                        continue;
                    };
                    let prefix = if definition.unique {
                        encode_secondary_index_prefix(&table_schema, &definition, &row)?
                    } else {
                        None
                    };
                    let decoded_row_bytes = estimated_row_bytes(&row)?;
                    let row_work_bytes = catalog_work_bytes
                        .checked_add(primary_key.len())
                        .and_then(|bytes| bytes.checked_add(value.len()))
                        .and_then(|bytes| bytes.checked_add(decoded_row_bytes))
                        .and_then(|bytes| bytes.checked_add(index_key.len()))
                        .and_then(|bytes| bytes.checked_add(prefix.as_ref().map_or(0, Vec::len)))
                        .and_then(|bytes| bytes.checked_add(256))
                        .ok_or_else(batch_too_large)?;
                    ensure_batch_bytes(row_work_bytes)?;

                    if let (Some(root), Some(prefix)) = (index_root_page_id, prefix.as_deref()) {
                        operations = operations.checked_add(1).ok_or_else(batch_too_large)?;
                        if operations > MAX_PAGED_BATCH_OPERATIONS {
                            return Err(operation_limit());
                        }
                        let mut existing = Btree::cursor_from_in_transaction(
                            &mut transaction,
                            root,
                            tree_id,
                            prefix,
                        )?;
                        if let Some((existing_key, existing_value)) =
                            existing.next_in_transaction(&mut transaction)?
                        {
                            if !existing_value.is_empty() {
                                return Err(storage_corrupt(format!(
                                    "New secondary index `{}` contains a non-empty value",
                                    definition.name
                                )));
                            }
                            if secondary_index_entry_matches_prefix(&existing_key, prefix) {
                                return Err(unique_violation(&definition.name));
                            }
                        }
                    }

                    operations = operations.checked_add(1).ok_or_else(batch_too_large)?;
                    if operations > MAX_PAGED_BATCH_OPERATIONS {
                        return Err(operation_limit());
                    }
                    let root = match index_root_page_id {
                        Some(root) => root,
                        None => Btree::create(&mut transaction, tree_id)?,
                    };
                    index_root_page_id = Some(Btree::upsert(
                        &mut transaction,
                        root,
                        tree_id,
                        &index_key,
                        &[],
                    )?);
                    entry_count = entry_count
                        .checked_add(1)
                        .ok_or_else(|| limit_error("The index entry count overflowed"))?;
                }
            }

            let record = encode_catalog_index_record(&CatalogIndexRecord {
                definition: definition.clone(),
                tree_id,
                root_page_id: index_root_page_id,
                entry_count: entry_count as u64,
            })?;
            let final_catalog_work_bytes = header
                .0
                .len()
                .checked_add(header.1.len())
                .and_then(|bytes| bytes.checked_add(record.0.len()))
                .and_then(|bytes| bytes.checked_add(record.1.len()))
                .and_then(|bytes| bytes.checked_add(256))
                .ok_or_else(batch_too_large)?;
            ensure_batch_bytes(final_catalog_work_bytes)?;
            let catalog_root = Btree::upsert(
                &mut transaction,
                catalog_root,
                CATALOG_TREE_ID,
                &header.0,
                &header.1,
            )?;
            let catalog_root = Btree::upsert(
                &mut transaction,
                catalog_root,
                CATALOG_TREE_ID,
                &record.0,
                &record.1,
            )?;
            Ok((catalog_root, index_root_page_id, entry_count))
        })();
        let (catalog_root, root_page_id, entry_count) = match result {
            Ok(result) => result,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        if let Err(error) =
            transaction.commit(revision, applied_journal_sequence, Some(catalog_root))
        {
            if error.code == "RECOVERY_REQUIRED" || pager.is_recovery_required() {
                self.recovery_required = true;
            }
            return Err(error);
        }
        drop(pager);

        self.indexes.insert(
            definition.name.clone(),
            PagedIndex {
                definition,
                tree_id,
                root_page_id,
                entry_count,
            },
        );
        self.next_tree_id = next_tree_id;
        self.revision = revision;
        Ok(revision)
    }

    /// Drops one secondary index and publishes its reclaimed pages with the catalog update.
    pub(crate) fn drop_index_and_advance(&mut self, name: &str) -> Result<u64> {
        self.ensure_ready()?;
        let index = self.indexes.get(name).ok_or_else(|| {
            EngineError::new("INDEX_NOT_FOUND", format!("Index `{name}` is not defined"))
        })?;
        let tree = DroppedTree {
            tree_id: index.tree_id,
            root_page_id: index.root_page_id,
        };
        let index_count = self
            .indexes
            .len()
            .checked_sub(1)
            .ok_or_else(|| storage_corrupt("The catalog index count underflowed"))?;
        let revision = crate::revision::next_database_revision(self.revision)?;
        let header = encode_catalog_header_record(&CatalogHeader {
            next_tree_id: self.next_tree_id,
            table_count: u32::try_from(self.tables.len())
                .map_err(|_| limit_error("The catalog contains too many tables"))?,
            index_count: u32::try_from(index_count)
                .map_err(|_| limit_error("The catalog contains too many indexes"))?,
        })?;
        let key = encode_catalog_index_key(name)?;
        ensure_batch_bytes(header.0.len() + header.1.len() + key.len() + 192)?;
        let plan = SchemaDropPlan {
            revision,
            header,
            catalog_keys: vec![key],
            trees: vec![tree],
        };

        let mut pager = self.pager.borrow_mut();
        if let Err(error) = publish_schema_drop(&mut pager, plan) {
            if error.code == "RECOVERY_REQUIRED" || pager.is_recovery_required() {
                self.recovery_required = true;
            }
            return Err(error);
        }
        drop(pager);
        self.indexes.remove(name);
        self.revision = revision;
        Ok(revision)
    }

    /// Drops one table and all of its secondary indexes in one durable pager generation.
    pub(crate) fn drop_table_and_advance(&mut self, name: &str) -> Result<u64> {
        self.ensure_ready()?;
        let table = self
            .tables
            .get(name)
            .ok_or_else(|| EngineError::table_not_found(name))?;
        let mut catalog_keys = Vec::new();
        let mut trees = Vec::new();
        let mut retained_bytes = 0usize;
        let mut dropped_index_count = 0usize;
        // BTreeMap iteration gives deterministic index-name order. Reclaim child indexes before
        // their table so a late validation failure can abort one complete candidate.
        for (index_name, index) in self
            .indexes
            .iter()
            .filter(|(_, index)| index.definition.table == name)
        {
            let key = encode_catalog_index_key(index_name)?;
            retained_bytes = retained_bytes
                .checked_add(key.len() + 64)
                .ok_or_else(batch_too_large)?;
            ensure_batch_bytes(retained_bytes)?;
            catalog_keys.push(key);
            trees.push(DroppedTree {
                tree_id: index.tree_id,
                root_page_id: index.root_page_id,
            });
            dropped_index_count += 1;
        }
        let table_key = encode_catalog_table_key(name)?;
        retained_bytes = retained_bytes
            .checked_add(table_key.len() + 64)
            .ok_or_else(batch_too_large)?;
        ensure_batch_bytes(retained_bytes)?;
        catalog_keys.push(table_key);
        trees.push(DroppedTree {
            tree_id: table.tree_id,
            root_page_id: table.root_page_id,
        });

        let table_count = self
            .tables
            .len()
            .checked_sub(1)
            .ok_or_else(|| storage_corrupt("The catalog table count underflowed"))?;
        let index_count = self
            .indexes
            .len()
            .checked_sub(dropped_index_count)
            .ok_or_else(|| storage_corrupt("The catalog index count underflowed"))?;
        let revision = crate::revision::next_database_revision(self.revision)?;
        let header = encode_catalog_header_record(&CatalogHeader {
            next_tree_id: self.next_tree_id,
            table_count: u32::try_from(table_count)
                .map_err(|_| limit_error("The catalog contains too many tables"))?,
            index_count: u32::try_from(index_count)
                .map_err(|_| limit_error("The catalog contains too many indexes"))?,
        })?;
        retained_bytes = retained_bytes
            .checked_add(header.0.len() + header.1.len() + 128)
            .ok_or_else(batch_too_large)?;
        ensure_batch_bytes(retained_bytes)?;
        let plan = SchemaDropPlan {
            revision,
            header,
            catalog_keys,
            trees,
        };

        let mut pager = self.pager.borrow_mut();
        if let Err(error) = publish_schema_drop(&mut pager, plan) {
            if error.code == "RECOVERY_REQUIRED" || pager.is_recovery_required() {
                self.recovery_required = true;
            }
            return Err(error);
        }
        drop(pager);
        self.tables.remove(name);
        self.indexes
            .retain(|_, index| index.definition.table != name);
        self.revision = revision;
        Ok(revision)
    }

    /// Streams `ALTER TABLE ADD COLUMN` into one crash-safe paged generation.
    pub(crate) fn add_column_and_advance(
        &mut self,
        table_name: &str,
        column: &crate::ColumnDefinition,
    ) -> Result<u64> {
        self.ensure_ready()?;
        let table = self
            .tables
            .get(table_name)
            .ok_or_else(|| EngineError::table_not_found(table_name))?;

        // Preserve logical validation and populated-NOT-NULL error order before applying any
        // page-specific work or tree-ID limit, without retaining a prospective schema clone.
        validate_added_column(&table.schema, column)?;
        if table.row_count != 0 && !column.nullable && column.default.is_none() {
            return Err(EngineError::constraint_violation(format!(
                "Column `{}` in `{table_name}` cannot be null",
                column.name
            )));
        }
        // The revision is a public JavaScript number. Reject exhaustion after logical statement
        // validation, but before any catalog clone, row scan, or pager candidate can be created.
        let revision = crate::revision::next_database_revision(self.revision)?;

        // Bound borrowed metadata before constructing the owned prospective schema and plan.
        let current_schema_work = estimated_schema_work_bytes(&table.schema)?;
        let column_work = estimated_column_work_bytes(column)?;
        let mut fixed_work_bytes = current_schema_work
            .checked_add(column_work)
            .ok_or_else(batch_too_large)?;
        ensure_batch_bytes(fixed_work_bytes)?;
        let new_schema = schema_with_added_column(&table.schema, column)?;

        let rooted = table.root_page_id.is_some();
        preflight_add_column_operations(table.row_count, rooted)?;
        let (new_tree_id, next_tree_id) = if rooted {
            (
                self.next_tree_id,
                replacement_next_tree_id(self.next_tree_id, 0)?,
            )
        } else {
            (table.tree_id, self.next_tree_id)
        };
        let header = encode_catalog_header_record(&CatalogHeader {
            next_tree_id,
            table_count: u32::try_from(self.tables.len())
                .map_err(|_| limit_error("The catalog contains too many tables"))?,
            index_count: u32::try_from(self.indexes.len())
                .map_err(|_| limit_error("The catalog contains too many indexes"))?,
        })?;
        // Charge the concurrently retained prospective-schema/catalog codec work before cloning
        // the schema into a physical record. The six-times string/default estimates cover the
        // record clone, serde model, and encoded bytes at their encoding peak; exact encoded bytes
        // are retained in the final fixed budget below as a separate defense.
        let new_schema_work = estimated_schema_work_bytes(&new_schema)?;
        fixed_work_bytes = fixed_work_bytes
            // `new_schema` is retained in the plan while the provisional/final catalog record
            // holds a clone and serde holds its temporary model. Charge all three concurrent
            // owned copies before constructing either latter allocation.
            .checked_add(new_schema_work.checked_mul(3).ok_or_else(batch_too_large)?)
            .and_then(|bytes| bytes.checked_add(header.0.len()))
            .and_then(|bytes| bytes.checked_add(header.1.len()))
            .and_then(|bytes| bytes.checked_add(512))
            .ok_or_else(batch_too_large)?;
        ensure_batch_bytes(fixed_work_bytes)?;
        let provisional_table = encode_catalog_table_record(&CatalogTableRecord {
            schema: new_schema.clone(),
            tree_id: new_tree_id,
            root_page_id: table.root_page_id,
            row_count: table.row_count as u64,
        })?;
        fixed_work_bytes = fixed_work_bytes
            .checked_add(provisional_table.0.len())
            .and_then(|bytes| bytes.checked_add(provisional_table.1.len()))
            .ok_or_else(batch_too_large)?;
        ensure_batch_bytes(fixed_work_bytes)?;
        drop(provisional_table);

        let plan = AddColumnPlan {
            revision,
            header,
            old_schema: table.schema.clone(),
            new_schema,
            old_tree: DroppedTree {
                tree_id: table.tree_id,
                root_page_id: table.root_page_id,
            },
            new_tree_id,
            row_count: table.row_count,
            fixed_work_bytes,
        };
        let mut pager = self.pager.borrow_mut();
        let published = match publish_added_column(&mut pager, plan) {
            Ok(published) => published,
            Err(error) => {
                if error.code == "RECOVERY_REQUIRED" || pager.is_recovery_required() {
                    self.recovery_required = true;
                }
                return Err(error);
            }
        };
        drop(pager);

        let table = self
            .tables
            .get_mut(table_name)
            .expect("ALTER TABLE resolved the committed table");
        table.schema = published.schema;
        table.tree_id = published.tree_id;
        table.root_page_id = published.root_page_id;
        self.next_tree_id = next_tree_id;
        self.revision = published.revision;
        Ok(published.revision)
    }

    /// Atomically defines or replaces a complete table snapshot.
    ///
    /// The current Rust/JavaScript API receives the source as a buffered `Vec<Row>`; this method
    /// bounds that buffer and its canonical keys to 16 MiB before opening a pager candidate. The
    /// new table and every affected index get fresh monotonic tree IDs even when `rows` is empty.
    /// Index construction itself streams the fresh candidate table.
    pub fn replace_table_snapshot(
        &mut self,
        schema: TableSchema,
        mut rows: Vec<Row>,
    ) -> Result<ApplyOutcome> {
        self.ensure_ready()?;

        // Preserve the in-memory contract's ordering: validate the logical shape, then resolve an
        // existing-schema conflict before applying page-codec or batch-size constraints.
        validate_schema(&schema)?;
        let existing = self.tables.get(&schema.name);
        if let Some(existing) = existing
            && existing.schema != schema
        {
            return Err(EngineError::invalid_schema(format!(
                "Table `{}` is already defined with a different schema",
                schema.name
            )));
        }

        // Walk borrowed metadata first. These conservative estimates cover the owned plan copy,
        // the codec's temporary JSON model, and encoded catalog bytes, so adversarial metadata is
        // rejected before any of those copies are materialized.
        let mut retained_bytes = estimated_schema_work_bytes(&schema)?;
        let mut affected_index_count = 0usize;
        let mut unique_index_count = 0usize;
        for index in self
            .indexes
            .values()
            .filter(|index| index.definition.table == schema.name)
        {
            affected_index_count = affected_index_count
                .checked_add(1)
                .ok_or_else(batch_too_large)?;
            unique_index_count = unique_index_count
                .checked_add(usize::from(index.definition.unique))
                .ok_or_else(batch_too_large)?;
            retained_bytes = retained_bytes
                .checked_add(estimated_index_definition_work_bytes(&index.definition)?)
                .ok_or_else(batch_too_large)?;
            ensure_batch_bytes(retained_bytes)?;
        }
        ensure_batch_bytes(retained_bytes)?;
        let catalog_rootless = self.pager.borrow().catalog_root_page_id().is_none();
        preflight_snapshot_operations(
            rows.len(),
            affected_index_count,
            unique_index_count,
            existing.is_some(),
            catalog_rootless,
        )?;
        let next_tree_id = replacement_next_tree_id(self.next_tree_id, affected_index_count)?;

        let row_slots = rows
            .len()
            .checked_mul(std::mem::size_of::<Row>())
            .ok_or_else(batch_too_large)?;
        retained_bytes = retained_bytes
            .checked_add(row_slots)
            .ok_or_else(batch_too_large)?;
        ensure_batch_bytes(retained_bytes)?;
        let mut primary_keys = BTreeSet::new();
        let legacy_order_slots = rows
            .len()
            .checked_mul(std::mem::size_of::<(String, usize)>())
            .ok_or_else(batch_too_large)?;
        retained_bytes = retained_bytes
            .checked_add(legacy_order_slots)
            .ok_or_else(batch_too_large)?;
        ensure_batch_bytes(retained_bytes)?;
        let mut legacy_row_order = Vec::with_capacity(rows.len());
        for (row_index, row) in rows.iter_mut().enumerate() {
            let normalized = normalize_row(&schema, std::mem::take(row))?;
            validate_primary_storage_key_bound(&schema, &normalized)?;
            let key = encode_primary_key(&schema, &normalized)?;
            let key_bytes = key.len().checked_add(64).ok_or_else(batch_too_large)?;
            if primary_keys.contains(&key) {
                return Err(EngineError::invalid_change(format!(
                    "Snapshot for `{}` contains a duplicate primary key",
                    schema.name
                )));
            }
            let legacy_key = crate::storage::row_key(&schema, &normalized)?;
            let prospective_bytes = retained_bytes
                .checked_add(estimated_row_bytes(&normalized)?)
                .and_then(|bytes| bytes.checked_add(key_bytes))
                .and_then(|bytes| bytes.checked_add(legacy_key.len()))
                .and_then(|bytes| bytes.checked_add(64))
                .ok_or_else(batch_too_large)?;
            ensure_batch_bytes(prospective_bytes)?;
            assert!(primary_keys.insert(key));
            legacy_row_order.push((legacy_key, row_index));
            retained_bytes = prospective_bytes;
            *row = normalized;
        }
        legacy_row_order.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        for row in &rows {
            encode_row(row)?;
        }
        // Mirror InMemory's deterministic index rebuild order: index name (the BTreeMap order),
        // then legacy JSON primary key. One unique-key set is retained and released per index.
        for index in self
            .indexes
            .values()
            .filter(|index| index.definition.table == schema.name)
        {
            let mut unique_keys = index.definition.unique.then(BTreeSet::new);
            let mut unique_bytes = 0usize;
            for (_, row_index) in &legacy_row_order {
                let row = &rows[*row_index];
                let Some(legacy_index_key) = validated_index_key(&schema, &index.definition, row)?
                else {
                    continue;
                };
                if let Some(unique_keys) = &mut unique_keys {
                    if unique_keys.contains(&legacy_index_key) {
                        return Err(EngineError::constraint_violation(format!(
                            "Index `{}` would contain duplicate values",
                            index.definition.name
                        )));
                    }
                    let prospective_unique_bytes = retained_bytes
                        .checked_add(unique_bytes)
                        .and_then(|bytes| bytes.checked_add(legacy_index_key.len()))
                        .and_then(|bytes| bytes.checked_add(64))
                        .ok_or_else(batch_too_large)?;
                    ensure_batch_bytes(prospective_unique_bytes)?;
                    unique_bytes = unique_bytes
                        .checked_add(legacy_index_key.len())
                        .and_then(|bytes| bytes.checked_add(64))
                        .ok_or_else(batch_too_large)?;
                    assert!(unique_keys.insert(legacy_index_key));
                }
                // Keep the exact page-key codec as a defense after the shared logical validator.
                encode_secondary_index_entry_key(&schema, &index.definition, row)?;
            }
        }

        let table_tree_id = self.next_tree_id;
        let provisional_table = encode_catalog_table_record(&CatalogTableRecord {
            schema: schema.clone(),
            tree_id: table_tree_id,
            root_page_id: None,
            row_count: 0,
        })?;
        let provisional_table_bytes = provisional_table
            .0
            .len()
            .checked_add(provisional_table.1.len())
            .and_then(|bytes| bytes.checked_add(128))
            .ok_or_else(batch_too_large)?;
        retained_bytes = retained_bytes
            .checked_add(provisional_table_bytes)
            .ok_or_else(batch_too_large)?;
        ensure_batch_bytes(retained_bytes)?;
        drop(provisional_table);

        let mut allocated_tree_id = table_tree_id
            .checked_add(1)
            .expect("replacement tree-ID preflight proved the first increment");
        let mut replacement_indexes = Vec::with_capacity(affected_index_count);
        for index in self
            .indexes
            .values()
            .filter(|index| index.definition.table == schema.name)
        {
            let tree_id = allocated_tree_id;
            allocated_tree_id = allocated_tree_id
                .checked_add(1)
                .expect("replacement tree-ID preflight proved every increment");
            replacement_indexes.push(ReplacementIndex {
                definition: index.definition.clone(),
                tree_id,
                old_tree: Some(DroppedTree {
                    tree_id: index.tree_id,
                    root_page_id: index.root_page_id,
                }),
            });
        }
        debug_assert_eq!(allocated_tree_id, next_tree_id);

        let table_count = self
            .tables
            .len()
            .checked_add(usize::from(existing.is_none()))
            .ok_or_else(|| limit_error("The catalog table count overflowed"))?;
        if table_count > MAX_CATALOG_TABLES as usize {
            return Err(limit_error(format!(
                "A catalog cannot contain more than {MAX_CATALOG_TABLES} tables"
            )));
        }
        let revision = crate::revision::next_database_revision(self.revision)?;
        let header = encode_catalog_header_record(&CatalogHeader {
            next_tree_id,
            table_count: u32::try_from(table_count)
                .map_err(|_| limit_error("The catalog contains too many tables"))?,
            index_count: u32::try_from(self.indexes.len())
                .map_err(|_| limit_error("The catalog contains too many indexes"))?,
        })?;
        let header_bytes = header
            .0
            .len()
            .checked_add(header.1.len())
            .ok_or_else(batch_too_large)?;
        retained_bytes = retained_bytes
            .checked_add(header_bytes)
            .ok_or_else(batch_too_large)?;
        for index in &replacement_indexes {
            let provisional = encode_catalog_index_record(&CatalogIndexRecord {
                definition: index.definition.clone(),
                tree_id: index.tree_id,
                root_page_id: None,
                entry_count: 0,
            })?;
            let provisional_bytes = provisional
                .0
                .len()
                .checked_add(provisional.1.len())
                .and_then(|bytes| bytes.checked_add(96))
                .ok_or_else(batch_too_large)?;
            retained_bytes = retained_bytes
                .checked_add(provisional_bytes)
                .ok_or_else(batch_too_large)?;
        }
        ensure_batch_bytes(retained_bytes)?;

        let old_table = existing.map(|table| DroppedTree {
            tree_id: table.tree_id,
            root_page_id: table.root_page_id,
        });
        let table_name = schema.name.clone();
        let plan = TableReplacementPlan {
            revision,
            header,
            schema,
            rows,
            table_tree_id,
            old_table,
            indexes: replacement_indexes,
        };
        let mut pager = self.pager.borrow_mut();
        let published = match publish_table_replacement(&mut pager, plan) {
            Ok(published) => published,
            Err(error) => {
                if error.code == "RECOVERY_REQUIRED" || pager.is_recovery_required() {
                    self.recovery_required = true;
                }
                return Err(error);
            }
        };
        drop(pager);

        self.tables.insert(
            table_name.clone(),
            PagedTable {
                schema: published.schema,
                tree_id: table_tree_id,
                root_page_id: published.table_root_page_id,
                row_count: published.row_count,
            },
        );
        for index in published.indexes {
            self.indexes.insert(
                index.definition.name.clone(),
                PagedIndex {
                    definition: index.definition,
                    tree_id: index.tree_id,
                    root_page_id: index.root_page_id,
                    entry_count: index.entry_count,
                },
            );
        }
        self.next_tree_id = next_tree_id;
        self.revision = revision;
        Ok(ApplyOutcome {
            revision,
            tables: vec![table_name],
        })
    }

    /// Atomically applies page-native row upserts and deletes in one durable generation.
    ///
    /// This narrow mutation surface deliberately remains an inherent method until the remaining
    /// DDL, snapshot-replacement, and explicit transaction operations can satisfy the complete
    /// [`crate::StorageDriver`] contract. Sequential changes to one primary key use last-write
    /// semantics. A non-empty batch advances the revision exactly once even when its final rows
    /// equal the committed rows, matching [`InMemoryStorage`].
    pub fn apply_batch(&mut self, batch: &ChangeBatch) -> Result<ApplyOutcome> {
        if batch.changes.is_empty() {
            return Ok(ApplyOutcome {
                revision: self.revision,
                tables: vec![],
            });
        }

        let write_set = self.prepare_batch_write_set(&batch.changes)?;
        self.commit_row_write_set(write_set)
    }

    fn prepare_batch_write_set(&self, input_changes: &[Change]) -> Result<PagedRowWriteSet> {
        self.prepare_row_write_set_with(input_changes, preflight_change_batch, false)
    }

    pub(crate) fn prepare_row_write_set(
        &self,
        input_changes: &[Change],
    ) -> Result<PagedRowWriteSet> {
        self.prepare_row_write_set_with(input_changes, preflight_row_write_set, true)
    }

    /// Rejects two SQL upserts which target the same canonical page key in one statement.
    ///
    /// SQL planning still uses JSON-shaped logical keys for its in-memory compatibility path, but
    /// typed page keys intentionally canonicalize aliases such as FLOAT `0`, `0.0`, and `-0.0`.
    /// A delete followed by an upsert remains valid for a primary-key spelling change.
    pub(crate) fn validate_sql_row_change_sequence(&self, input_changes: &[Change]) -> Result<()> {
        self.ensure_ready()?;
        let mut upserts = BTreeMap::<String, BTreeSet<Vec<u8>>>::new();
        for change in input_changes {
            let (table_name, input, is_upsert) = match change {
                Change::Upsert { table, row } => (table, row, true),
                Change::Delete { table, key } => (table, key, false),
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

    fn prepare_row_write_set_with(
        &self,
        input_changes: &[Change],
        preflight: RowWritePreflight,
        reject_duplicate_upserts: bool,
    ) -> Result<PagedRowWriteSet> {
        self.ensure_ready()?;
        preflight_batch(input_changes, &self.tables, &self.indexes, preflight)?;
        if reject_duplicate_upserts {
            self.validate_sql_row_change_sequence(input_changes)?;
        }
        let mut retained_bytes = 0usize;
        let mut tables = BTreeMap::<String, BTreeMap<Vec<u8>, PagedRowChange>>::new();
        for change in input_changes {
            let (table_name, input, is_delete) = match change {
                Change::Upsert { table, row } => (table, row, false),
                Change::Delete { table, key } => (table, key, true),
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
            table_changes.insert(key, PagedRowChange { old, next });
        }

        self.validate_changed_unique_indexes(&tables, retained_bytes)?;

        Ok(PagedRowWriteSet {
            base_revision: self.revision,
            tables,
        })
    }

    fn validate_changed_unique_indexes(
        &self,
        tables: &BTreeMap<String, BTreeMap<Vec<u8>, PagedRowChange>>,
        mut retained_bytes: usize,
    ) -> Result<()> {
        for (table_name, changes) in tables {
            let table = &self.tables[table_name];
            for index in self
                .indexes
                .values()
                .filter(|index| index.definition.table == *table_name && index.definition.unique)
            {
                let mut changed_prefixes = BTreeMap::<Vec<u8>, &[u8]>::new();
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
                    if changed_prefixes
                        .insert(prefix.clone(), primary_key)
                        .is_some()
                    {
                        return Err(unique_violation(&index.definition.name));
                    }

                    let existing_primary_keys =
                        committed_index_primary_keys(&mut self.pager.borrow_mut(), index, &prefix)?;
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
                                            != Some(prefix.as_slice())
                                    }
                                }
                            } else {
                                false
                            };
                        if !existing_moves {
                            return Err(unique_violation(&index.definition.name));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn commit_row_write_set(
        &mut self,
        write_set: PagedRowWriteSet,
    ) -> Result<ApplyOutcome> {
        let tables = write_set.tables.keys().cloned().collect();
        self.commit_row_write_set_for_tables(write_set, tables, false)
    }

    /// Publishes a complete explicit SQL transaction in one pager generation.
    ///
    /// `touched_tables` is intentionally independent of the final write-set: an explicit
    /// transaction which changes a row and later restores it still commits one revision, matching
    /// the in-memory engine's transaction contract.
    pub(crate) fn commit_transaction_write_set(
        &mut self,
        write_set: PagedRowWriteSet,
        touched_tables: BTreeSet<String>,
    ) -> Result<ApplyOutcome> {
        self.commit_row_write_set_for_tables(write_set, touched_tables, true)
    }

    fn commit_row_write_set_for_tables(
        &mut self,
        write_set: PagedRowWriteSet,
        outcome_tables: BTreeSet<String>,
        force_revision: bool,
    ) -> Result<ApplyOutcome> {
        self.ensure_ready()?;
        if write_set.base_revision != self.revision {
            return Err(EngineError::new(
                "WRITE_CONFLICT",
                format!(
                    "Paged write-set revision {} does not match current revision {}",
                    write_set.base_revision, self.revision
                ),
            ));
        }
        if write_set.tables.is_empty() && !force_revision {
            return Ok(ApplyOutcome {
                revision: self.revision,
                tables: vec![],
            });
        }
        let PagedRowWriteSet {
            base_revision: _,
            tables: changes,
        } = write_set;

        let revision = crate::revision::next_database_revision(self.revision)?;
        let mut next_tables = self.tables.clone();
        let mut next_indexes = self.indexes.clone();
        let mut pager = self.pager.borrow_mut();
        let applied_journal_sequence = pager.applied_journal_sequence();
        let catalog_root = pager.catalog_root_page_id().ok_or_else(|| {
            storage_corrupt("A non-empty paged catalog must have a published root")
        })?;
        let mut transaction = pager.begin_write()?;
        let result = (|| {
            for (table_name, table_changes) in &changes {
                let table = next_tables
                    .get_mut(table_name)
                    .expect("every changed table was resolved above");
                for (key, change) in table_changes {
                    match &change.next {
                        Some(row) => {
                            let current = match table.root_page_id {
                                Some(root) => root,
                                None => Btree::create(&mut transaction, table.tree_id)?,
                            };
                            table.root_page_id = Some(Btree::upsert(
                                &mut transaction,
                                current,
                                table.tree_id,
                                key,
                                &encode_row(row)?,
                            )?);
                        }
                        None => {
                            if let Some(root) = table.root_page_id {
                                table.root_page_id =
                                    Btree::delete(&mut transaction, root, table.tree_id, key)?.0;
                            }
                        }
                    }
                }
                table.row_count = adjusted_count(
                    table.row_count,
                    table_changes
                        .values()
                        .filter(|change| change.old.is_none() && change.next.is_some())
                        .count(),
                    table_changes
                        .values()
                        .filter(|change| change.old.is_some() && change.next.is_none())
                        .count(),
                    "table row",
                )?;

                for index in next_indexes
                    .values_mut()
                    .filter(|index| index.definition.table == *table_name)
                {
                    let mut inserted = 0usize;
                    let mut deleted = 0usize;
                    for change in table_changes.values() {
                        let old_key = if let Some(old_row) = &change.old
                            && let Some(index_key) = encode_secondary_index_entry_key(
                                &table.schema,
                                &index.definition,
                                old_row,
                            )? {
                            Some(index_key)
                        } else {
                            None
                        };
                        let next_key = if let Some(row) = &change.next
                            && let Some(index_key) = encode_secondary_index_entry_key(
                                &table.schema,
                                &index.definition,
                                row,
                            )? {
                            Some(index_key)
                        } else {
                            None
                        };
                        if old_key == next_key {
                            continue;
                        }
                        if let Some(index_key) = old_key
                            && let Some(root) = index.root_page_id
                        {
                            let (next_root, removed) =
                                Btree::delete(&mut transaction, root, index.tree_id, &index_key)?;
                            if !removed {
                                return Err(storage_corrupt(format!(
                                    "Index `{}` is missing an entry for a committed row",
                                    index.definition.name
                                )));
                            }
                            index.root_page_id = next_root;
                            deleted += 1;
                        }
                        if let Some(index_key) = next_key {
                            let current = match index.root_page_id {
                                Some(root) => root,
                                None => Btree::create(&mut transaction, index.tree_id)?,
                            };
                            index.root_page_id = Some(Btree::upsert(
                                &mut transaction,
                                current,
                                index.tree_id,
                                &index_key,
                                &[],
                            )?);
                            inserted += 1;
                        }
                    }
                    index.entry_count =
                        adjusted_count(index.entry_count, inserted, deleted, "index entry")?;
                }
            }

            let mut catalog_root = catalog_root;
            for table_name in changes.keys() {
                let table = &next_tables[table_name];
                let (key, value) = encode_catalog_table_record(&CatalogTableRecord {
                    schema: table.schema.clone(),
                    tree_id: table.tree_id,
                    root_page_id: table.root_page_id,
                    row_count: table.row_count as u64,
                })?;
                catalog_root = Btree::upsert(
                    &mut transaction,
                    catalog_root,
                    CATALOG_TREE_ID,
                    &key,
                    &value,
                )?;
            }
            for index in next_indexes
                .values()
                .filter(|index| changes.contains_key(index.definition.table.as_str()))
            {
                let (key, value) = encode_catalog_index_record(&CatalogIndexRecord {
                    definition: index.definition.clone(),
                    tree_id: index.tree_id,
                    root_page_id: index.root_page_id,
                    entry_count: index.entry_count as u64,
                })?;
                catalog_root = Btree::upsert(
                    &mut transaction,
                    catalog_root,
                    CATALOG_TREE_ID,
                    &key,
                    &value,
                )?;
            }
            Ok(catalog_root)
        })();

        let catalog_root = match result {
            Ok(catalog_root) => catalog_root,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        if let Err(error) =
            transaction.commit(revision, applied_journal_sequence, Some(catalog_root))
        {
            if error.code == "RECOVERY_REQUIRED" || pager.is_recovery_required() {
                self.recovery_required = true;
            }
            return Err(error);
        }
        drop(pager);

        self.tables = next_tables;
        self.indexes = next_indexes;
        self.revision = revision;
        Ok(ApplyOutcome {
            revision,
            tables: outcome_tables.into_iter().collect(),
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

fn preflight_batch(
    changes: &[Change],
    tables: &BTreeMap<String, PagedTable>,
    indexes: &BTreeMap<String, PagedIndex>,
    preflight: RowWritePreflight,
) -> Result<()> {
    let schemas = tables
        .iter()
        .map(|(name, table)| (name.as_str(), &table.schema))
        .collect();
    let definitions = indexes
        .values()
        .map(|index| &index.definition)
        .collect::<Vec<_>>();
    preflight(changes, &schemas, &definitions)?;
    let mut bytes = 0usize;
    let mut operations = 0usize;
    let mut changed_tables = BTreeSet::new();
    let mut index_counts = BTreeMap::<&str, usize>::new();
    for index in indexes.values() {
        *index_counts
            .entry(index.definition.table.as_str())
            .or_default() += 1;
    }
    for change in changes {
        let (table, row) = match change {
            Change::Upsert { table, row } | Change::Delete { table, key: row } => (table, row),
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
    Ok(())
}

fn estimated_schema_work_bytes(schema: &TableSchema) -> Result<usize> {
    let mut bytes = 512usize
        .checked_add(estimated_string_work_bytes(&schema.name)?)
        .ok_or_else(batch_too_large)?;
    for column in &schema.primary_key {
        bytes = bytes
            .checked_add(estimated_string_work_bytes(column)?)
            .and_then(|bytes| bytes.checked_add(64))
            .ok_or_else(batch_too_large)?;
    }
    for column in &schema.columns {
        bytes = bytes
            .checked_add(estimated_string_work_bytes(&column.name)?)
            .and_then(|bytes| bytes.checked_add(192))
            .ok_or_else(batch_too_large)?;
        if let Some(default) = &column.default {
            bytes = bytes
                .checked_add(crate::storage::estimated_value_bytes(default)?)
                .and_then(|bytes| bytes.checked_add(64))
                .ok_or_else(batch_too_large)?;
        }
    }
    Ok(bytes)
}

fn estimated_column_work_bytes(column: &crate::ColumnDefinition) -> Result<usize> {
    let mut bytes = 192usize
        .checked_add(estimated_string_work_bytes(&column.name)?)
        .ok_or_else(batch_too_large)?;
    if let Some(default) = &column.default {
        bytes = bytes
            .checked_add(crate::storage::estimated_value_bytes(default)?)
            .and_then(|bytes| bytes.checked_add(64))
            .ok_or_else(batch_too_large)?;
    }
    Ok(bytes)
}

fn estimated_index_definition_work_bytes(definition: &IndexDefinition) -> Result<usize> {
    let table_bytes = estimated_string_work_bytes(&definition.table)?;
    let mut bytes = 256usize
        .checked_add(estimated_string_work_bytes(&definition.name)?)
        .and_then(|bytes| bytes.checked_add(table_bytes))
        .ok_or_else(batch_too_large)?;
    for column in &definition.columns {
        bytes = bytes
            .checked_add(estimated_string_work_bytes(column)?)
            .and_then(|bytes| bytes.checked_add(64))
            .ok_or_else(batch_too_large)?;
    }
    Ok(bytes)
}

fn estimated_string_work_bytes(value: &str) -> Result<usize> {
    value
        .len()
        .checked_mul(6)
        .and_then(|bytes| bytes.checked_add(64))
        .ok_or_else(batch_too_large)
}

fn replacement_next_tree_id(next_tree_id: TreeId, index_count: usize) -> Result<TreeId> {
    let tree_count = index_count
        .checked_add(1)
        .and_then(|count| TreeId::try_from(count).ok())
        .ok_or_else(|| limit_error("The catalog tree ID range is exhausted"))?;
    let next_tree_id = next_tree_id
        .checked_add(tree_count)
        .ok_or_else(|| limit_error("The catalog tree ID range is exhausted"))?;
    if next_tree_id > MAX_TREE_ID {
        return Err(limit_error("The catalog tree ID range is exhausted"));
    }
    Ok(next_tree_id)
}

fn snapshot_operation_count(
    row_count: usize,
    index_count: usize,
    unique_index_count: usize,
    replacing_existing: bool,
    catalog_rootless: bool,
) -> Result<usize> {
    let index_row_operations = index_count.checked_mul(3).ok_or_else(batch_too_large)?;
    let unique_row_operations = unique_index_count
        .checked_mul(2)
        .ok_or_else(batch_too_large)?;
    // Per row: validate/build the fresh table entry and publish its upsert; logically validate
    // every index in legacy order; visit and emit every possible candidate index entry; and probe
    // both the logical and candidate key sets for every unique index. NULL entries only make the
    // actual work smaller than this conservative bound.
    let per_row_operations = 2usize
        .checked_add(index_row_operations)
        .and_then(|operations| operations.checked_add(unique_row_operations))
        .ok_or_else(batch_too_large)?;
    let row_operations = row_count
        .checked_mul(per_row_operations)
        .ok_or_else(batch_too_large)?;
    // A non-empty replacement creates one fresh table root, opens one candidate-table cursor per
    // index, and may create one fresh root for every index. Counting the latter even when all
    // values turn out to be NULL keeps preflight independent of row contents.
    let build_setup_operations = if row_count == 0 {
        0
    } else {
        index_count
            .checked_mul(2)
            .and_then(|operations| operations.checked_add(1))
            .ok_or_else(batch_too_large)?
    };
    // One header and table record plus one record for each rebuilt index.
    let catalog_operations = 2usize
        .checked_add(index_count)
        .and_then(|operations| operations.checked_add(usize::from(catalog_rootless)))
        .ok_or_else(batch_too_large)?;
    // Replacements validate/reclaim every old child index, followed by the old table.
    let reclaim_operations = if replacing_existing {
        1usize
            .checked_add(index_count)
            .ok_or_else(batch_too_large)?
    } else {
        0
    };
    row_operations
        .checked_add(build_setup_operations)
        .and_then(|operations| operations.checked_add(catalog_operations))
        .and_then(|operations| operations.checked_add(reclaim_operations))
        .ok_or_else(batch_too_large)
}

fn preflight_snapshot_operations(
    row_count: usize,
    index_count: usize,
    unique_index_count: usize,
    replacing_existing: bool,
    catalog_rootless: bool,
) -> Result<()> {
    if snapshot_operation_count(
        row_count,
        index_count,
        unique_index_count,
        replacing_existing,
        catalog_rootless,
    )? > MAX_PAGED_BATCH_OPERATIONS
    {
        return Err(operation_limit());
    }
    Ok(())
}

fn add_column_operation_count(row_count: usize, rooted: bool) -> Result<usize> {
    if !rooted {
        // Catalog header and table-record publication only.
        return Ok(2);
    }
    // Each committed row is visited/transformed and upserted once. Fixed operations are candidate
    // cursor setup, fresh table-root creation, old-tree validation/reclaim, header upsert, and
    // table-record upsert.
    row_count
        .checked_mul(2)
        .and_then(|operations| operations.checked_add(5))
        .ok_or_else(batch_too_large)
}

fn preflight_add_column_operations(row_count: usize, rooted: bool) -> Result<()> {
    if add_column_operation_count(row_count, rooted)? > MAX_PAGED_BATCH_OPERATIONS {
        return Err(operation_limit());
    }
    Ok(())
}

fn operation_limit() -> EngineError {
    EngineError::new(
        "TRANSACTION_TOO_LARGE",
        format!(
            "A paged batch cannot require more than {MAX_PAGED_BATCH_OPERATIONS} row, index, and catalog operations"
        ),
    )
}

fn lookup_encoded_primary_key<D: PageDevice>(
    pager: &mut Pager<D>,
    table: &PagedTable,
    key: &[u8],
) -> Result<Option<Row>> {
    let Some(root) = table.root_page_id else {
        return Ok(None);
    };
    Btree::get(pager, root, table.tree_id, key)?
        .map(|value| validated_row(&table.schema, key, &value))
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
    while let Some((entry_key, value)) = cursor.next(pager)? {
        if !secondary_index_entry_matches_prefix(&entry_key, prefix) {
            break;
        }
        if !value.is_empty() {
            return Err(storage_corrupt(format!(
                "Secondary index `{}` contains a non-empty value",
                index.definition.name
            )));
        }
        primary_keys.push(secondary_index_primary_key(&entry_key, prefix)?.to_vec());
        if index.definition.unique && primary_keys.len() > 1 {
            return Err(storage_corrupt(format!(
                "Unique index `{}` contains duplicate values",
                index.definition.name
            )));
        }
    }
    Ok(primary_keys)
}

fn adjusted_count(current: usize, inserted: usize, deleted: usize, kind: &str) -> Result<usize> {
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

fn ensure_batch_bytes(bytes: usize) -> Result<()> {
    if bytes > MAX_PAGED_BATCH_BYTES {
        Err(batch_too_large())
    } else {
        Ok(())
    }
}

fn batch_too_large() -> EngineError {
    EngineError::new(
        "TRANSACTION_TOO_LARGE",
        format!("A paged batch cannot retain more than {MAX_PAGED_BATCH_BYTES} bytes"),
    )
}

fn unique_violation(index: &str) -> EngineError {
    EngineError::constraint_violation(format!("Index `{index}` would contain duplicate values"))
}

struct ImportCatalogBounds {
    table_count: u32,
    index_count: u32,
    next_tree_id: TreeId,
}

fn preflight_import_catalog_bounds(
    table_count: usize,
    index_count: usize,
) -> Result<ImportCatalogBounds> {
    if table_count > MAX_CATALOG_TABLES as usize {
        return Err(limit_error(format!(
            "An imported catalog cannot contain more than {MAX_CATALOG_TABLES} tables"
        )));
    }
    if index_count > MAX_CATALOG_INDEXES as usize {
        return Err(limit_error(format!(
            "An imported catalog cannot contain more than {MAX_CATALOG_INDEXES} indexes"
        )));
    }
    let table_count = u32::try_from(table_count)
        .map_err(|_| limit_error("The imported catalog contains too many tables"))?;
    let index_count = u32::try_from(index_count)
        .map_err(|_| limit_error("The imported catalog contains too many indexes"))?;
    let tree_count = u64::from(table_count)
        .checked_add(u64::from(index_count))
        .ok_or_else(|| limit_error("The imported catalog tree count overflowed"))?;
    let next_tree_id = FIRST_USER_TREE_ID
        .checked_add(tree_count)
        .filter(|next_tree_id| *next_tree_id <= MAX_TREE_ID)
        .ok_or_else(|| limit_error("The imported catalog tree ID range is exhausted"))?;
    Ok(ImportCatalogBounds {
        table_count,
        index_count,
        next_tree_id,
    })
}

fn preflight_import_metadata(
    source: &InMemoryStorage,
    bounds: &ImportCatalogBounds,
) -> Result<usize> {
    let table_count = bounds.table_count as usize;
    let index_count = bounds.index_count as usize;
    // These vectors coexist during publication: borrowed source plans, final owned catalog
    // records, and the encoded header. Charge every slot before any `with_capacity` call.
    let fixed_slots = table_count
        .checked_mul(std::mem::size_of::<TableInput<'static>>())
        .and_then(|bytes| {
            index_count
                .checked_mul(std::mem::size_of::<IndexInput<'static>>())
                .and_then(|index_bytes| bytes.checked_add(index_bytes))
        })
        .and_then(|bytes| {
            table_count
                .checked_mul(std::mem::size_of::<CatalogTableRecord>())
                .and_then(|record_bytes| bytes.checked_add(record_bytes))
        })
        .and_then(|bytes| {
            index_count
                .checked_mul(std::mem::size_of::<CatalogIndexRecord>())
                .and_then(|record_bytes| bytes.checked_add(record_bytes))
        })
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<(Vec<u8>, Vec<u8>)>() + 512))
        .ok_or_else(batch_too_large)?;
    ensure_batch_bytes(fixed_slots)?;

    // The final record vectors own one clone of every schema and index definition at once. Codec
    // scratch is instead sequential, so retain model heaps cumulatively but charge only the
    // largest per-record serialization peak below.
    let mut retained_bytes = fixed_slots;
    for name in source.table_names() {
        let schema = source.table_schema_ref(name)?;
        retained_bytes = retained_bytes
            .checked_add(estimated_retained_schema_bytes(schema)?)
            .ok_or_else(batch_too_large)?;
        ensure_batch_bytes(retained_bytes)?;
    }
    for name in source.index_names() {
        let definition = source.index_definition_ref(name).ok_or_else(|| {
            storage_corrupt(format!("In-memory index `{name}` has no definition"))
        })?;
        retained_bytes = retained_bytes
            .checked_add(estimated_retained_index_definition_bytes(definition)?)
            .ok_or_else(batch_too_large)?;
        ensure_batch_bytes(retained_bytes)?;
    }

    // Validate every eventual catalog key/value before candidate allocation. Each provisional is
    // dropped immediately; the retained baseline above represents the later final record vectors.
    let mut tree_id = FIRST_USER_TREE_ID;
    for name in source.table_names() {
        let schema = source.table_schema_ref(name)?;
        // Catalog encoding retains the caller's provisional model clone, a serde `Value` clone,
        // the growing canonical body, and the framed output. The shared estimator already charges
        // escaped strings at 6x; tripling it conservatively covers those concurrent models and
        // buffers before any clone is made.
        let codec_work = import_table_catalog_codec_work(schema)?;
        ensure_import_peak(retained_bytes, codec_work)?;
        let provisional = encode_catalog_table_record(&CatalogTableRecord {
            schema: schema.clone(),
            tree_id,
            root_page_id: None,
            row_count: 0,
        })?;
        let encoded_work = provisional
            .0
            .len()
            .checked_add(provisional.1.len())
            .and_then(|bytes| bytes.checked_add(128))
            .ok_or_else(batch_too_large)?;
        ensure_import_peak(retained_bytes, encoded_work)?;
        tree_id = advance_import_tree_id(tree_id)?;
    }
    for name in source.index_names() {
        let definition = source.index_definition_ref(name).ok_or_else(|| {
            storage_corrupt(format!("In-memory index `{name}` has no definition"))
        })?;
        let codec_work =
            import_catalog_codec_work(estimated_index_definition_work_bytes(definition)?)?;
        ensure_import_peak(retained_bytes, codec_work)?;
        let provisional = encode_catalog_index_record(&CatalogIndexRecord {
            definition: definition.clone(),
            tree_id,
            root_page_id: None,
            entry_count: 0,
        })?;
        let encoded_work = provisional
            .0
            .len()
            .checked_add(provisional.1.len())
            .and_then(|bytes| bytes.checked_add(128))
            .ok_or_else(batch_too_large)?;
        ensure_import_peak(retained_bytes, encoded_work)?;
        tree_id = advance_import_tree_id(tree_id)?;
    }
    debug_assert_eq!(tree_id, bounds.next_tree_id);
    Ok(retained_bytes)
}

fn estimated_retained_schema_bytes(schema: &TableSchema) -> Result<usize> {
    let mut bytes = schema
        .name
        .len()
        .checked_add(64)
        .ok_or_else(batch_too_large)?;
    bytes = bytes
        .checked_add(
            schema
                .primary_key
                .len()
                .checked_mul(std::mem::size_of::<String>())
                .ok_or_else(batch_too_large)?,
        )
        .and_then(|bytes| {
            bytes.checked_add(
                schema
                    .columns
                    .len()
                    .checked_mul(std::mem::size_of::<crate::ColumnDefinition>())?,
            )
        })
        .ok_or_else(batch_too_large)?;
    for name in &schema.primary_key {
        bytes = bytes
            .checked_add(name.len())
            .and_then(|bytes| bytes.checked_add(64))
            .ok_or_else(batch_too_large)?;
    }
    for column in &schema.columns {
        bytes = bytes
            .checked_add(column.name.len())
            .and_then(|bytes| bytes.checked_add(64))
            .ok_or_else(batch_too_large)?;
        if let Some(default) = &column.default {
            bytes = bytes
                .checked_add(crate::storage::estimated_value_bytes(default)?)
                .and_then(|bytes| bytes.checked_add(64))
                .ok_or_else(batch_too_large)?;
        }
    }
    Ok(bytes)
}

fn estimated_retained_index_definition_bytes(definition: &IndexDefinition) -> Result<usize> {
    let mut bytes = definition
        .name
        .len()
        .checked_add(definition.table.len())
        .and_then(|bytes| bytes.checked_add(128))
        .and_then(|bytes| {
            bytes.checked_add(
                definition
                    .columns
                    .len()
                    .checked_mul(std::mem::size_of::<String>())?,
            )
        })
        .ok_or_else(batch_too_large)?;
    for column in &definition.columns {
        bytes = bytes
            .checked_add(column.len())
            .and_then(|bytes| bytes.checked_add(64))
            .ok_or_else(batch_too_large)?;
    }
    Ok(bytes)
}

fn ensure_import_peak(retained_bytes: usize, transient_bytes: usize) -> Result<()> {
    let peak = retained_bytes
        .checked_add(transient_bytes)
        .ok_or_else(batch_too_large)?;
    ensure_batch_bytes(peak)
}

fn import_catalog_codec_work(model_work: usize) -> Result<usize> {
    model_work.checked_mul(3).ok_or_else(batch_too_large)
}

fn import_table_catalog_codec_work(schema: &TableSchema) -> Result<usize> {
    let mut work = import_catalog_codec_work(estimated_schema_work_bytes(schema)?)?;
    for column in &schema.columns {
        if let Some(default) = &column.default {
            // The model estimator is structural/raw for JSON defaults. Catalog serialization can
            // retain both the canonical body and the framed copy at their fully escaped sizes.
            let escaped_buffers = crate::storage::validate_json_value(default)?
                .checked_mul(2)
                .ok_or_else(batch_too_large)?;
            work = work
                .checked_add(escaped_buffers)
                .ok_or_else(batch_too_large)?;
        }
    }
    Ok(work)
}

fn import_row_encoding_work(encoded_model_work: usize, row_model_work: usize) -> Result<usize> {
    encoded_model_work
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(row_model_work))
        // The table builder retains the encoded primary key across row encoding and upsert.
        .and_then(|bytes| bytes.checked_add(crate::MAX_BTREE_KEY_BYTES + 64))
        .ok_or_else(batch_too_large)
}

fn import_table_operation_count(
    row_count: usize,
    index_count: usize,
    unique_index_count: usize,
) -> Result<usize> {
    // Per row: borrowed validation, primary-key encoding, exact candidate probe, and table
    // upsert. Each index adds logical validation, candidate encoding/visit, and upsert; a unique
    // index additionally encodes its prefix and probes the candidate tree.
    let per_row = index_count
        .checked_mul(3)
        .and_then(|operations| {
            unique_index_count
                .checked_mul(2)
                .and_then(|unique| operations.checked_add(unique))
        })
        .and_then(|operations| operations.checked_add(4))
        .ok_or_else(batch_too_large)?;
    let row_operations = row_count.checked_mul(per_row).ok_or_else(batch_too_large)?;
    let root_operations = if row_count == 0 {
        0
    } else {
        index_count.checked_add(1).ok_or_else(batch_too_large)?
    };
    row_operations
        .checked_add(root_operations)
        .ok_or_else(batch_too_large)
}

fn import_operation_count(tables: &[TableInput<'_>], index_count: usize) -> Result<usize> {
    // Catalog root creation, header upsert, and one table/index record upsert.
    let mut operations = 2usize
        .checked_add(tables.len())
        .and_then(|operations| operations.checked_add(index_count))
        .ok_or_else(batch_too_large)?;
    for table in tables {
        operations = operations
            .checked_add(import_table_operation_count(
                table.rows.len(),
                table.index_count,
                table.unique_index_count,
            )?)
            .ok_or_else(batch_too_large)?;
    }
    Ok(operations)
}

fn preflight_import_operations(tables: &[TableInput<'_>], index_count: usize) -> Result<()> {
    if import_operation_count(tables, index_count)? > MAX_PAGED_BATCH_OPERATIONS {
        return Err(operation_limit());
    }
    Ok(())
}

const IMPORT_CURSOR_WORK_BYTES: usize = 16 * 1024;

fn preflight_import_rows(
    metadata_bytes: usize,
    tables: &[TableInput<'_>],
    indexes: &[IndexInput<'_>],
) -> Result<()> {
    for table in tables {
        let schema_normalization_work = estimated_retained_schema_bytes(table.schema)?;
        for row in table.rows.values() {
            // Bound every clone before making it. The encode phase owns a serde row clone plus
            // canonical and framed buffers; the exact-key cursor phase may materialize an existing
            // one-MiB table value. They are sequential, so charge their maximum, not their sum.
            let row_model_work = crate::storage::estimated_row_bytes(row)?;
            let encoded_row_work =
                import_row_encoding_work(estimated_row_bytes(row)?, row_model_work)?;
            let normalization_work = row_model_work
                .checked_add(schema_normalization_work)
                .ok_or_else(batch_too_large)?;
            let primary_probe_work = MAX_PAGED_VALUE_BYTES
                .checked_add(crate::MAX_BTREE_KEY_BYTES)
                .and_then(|bytes| bytes.checked_add(IMPORT_CURSOR_WORK_BYTES))
                .ok_or_else(batch_too_large)?;
            ensure_import_peak(
                metadata_bytes,
                encoded_row_work
                    .max(normalization_work)
                    .max(primary_probe_work),
            )?;

            let normalized = normalize_row(table.schema, row.clone())?;
            if normalized != *row {
                return Err(storage_corrupt(format!(
                    "In-memory row in `{}` is not normalized to its schema",
                    table.schema.name
                )));
            }
            drop(normalized);
            validate_primary_storage_key_bound(table.schema, row)?;
            drop(encode_primary_key(table.schema, row)?);
            drop(encode_row(row)?);
        }
    }

    for index in indexes {
        let table = &tables[tables
            .binary_search_by(|candidate| {
                candidate.schema.name.as_str().cmp(&index.definition.table)
            })
            .expect("the import catalog preflight resolved every index table")];
        let index_work = estimated_index_definition_work_bytes(index.definition)?
            .checked_add(crate::MAX_BTREE_KEY_BYTES * 3)
            .and_then(|bytes| bytes.checked_add(IMPORT_CURSOR_WORK_BYTES))
            .ok_or_else(batch_too_large)?;
        ensure_import_peak(metadata_bytes, index_work)?;
        for row in table.rows.values() {
            drop(validated_index_key(table.schema, index.definition, row)?);
            drop(encode_secondary_index_entry_key(
                table.schema,
                index.definition,
                row,
            )?);
            if index.definition.unique {
                drop(encode_secondary_index_prefix(
                    table.schema,
                    index.definition,
                    row,
                )?);
            }
        }
    }
    Ok(())
}

fn advance_import_tree_id(tree_id: TreeId) -> Result<TreeId> {
    tree_id
        .checked_add(1)
        .filter(|next_tree_id| *next_tree_id <= MAX_TREE_ID)
        .ok_or_else(|| limit_error("The imported catalog tree ID range is exhausted"))
}

struct TableInput<'a> {
    schema: &'a TableSchema,
    rows: &'a BTreeMap<String, Row>,
    row_count: u64,
    tree_id: TreeId,
    index_count: usize,
    unique_index_count: usize,
}

struct IndexInput<'a> {
    definition: &'a IndexDefinition,
    tree_id: TreeId,
}

impl<D: PageDevice> StorageReader for PagedStorage<D> {
    fn ensure_readable(&self) -> Result<()> {
        self.ensure_ready()
    }

    fn visit_table(
        &self,
        table: &str,
        visitor: &mut dyn FnMut(&Row) -> Result<VisitControl>,
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
            let next = cursor.next(&mut self.pager.borrow_mut())?;
            let Some((key, value)) = next else {
                break;
            };
            let row = validated_row(&table.schema, &key, &value)?;
            if visitor(&row)? == VisitControl::Stop {
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
        .map(|value| validated_row(&table.schema, &encoded_key, &value))
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
        visitor: &mut dyn FnMut(&Row) -> Result<VisitControl>,
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
            let next = cursor.next(&mut self.pager.borrow_mut())?;
            let Some((entry_key, value)) = next else {
                break;
            };
            if !secondary_index_entry_matches_prefix(&entry_key, &prefix) {
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
                &entry_key,
            )?;
            if secondary_index_primary_key(&entry_key, &prefix)? != primary_key {
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
            let row = validated_row(&table_data.schema, primary_key, &row_value)?;
            if encode_secondary_index_entry_key(&table_data.schema, &index.definition, &row)?
                .as_deref()
                != Some(entry_key.as_slice())
            {
                return Err(storage_corrupt(format!(
                    "Secondary index `{}` entry does not match its table row",
                    index.definition.name
                )));
            }
            if visitor(&row)? == VisitControl::Stop {
                return Ok(Some(VisitOutcome::Stopped));
            }
        }
        Ok(Some(VisitOutcome::Complete))
    }

    fn table_schema(&self, table: &str) -> Result<TableSchema> {
        self.ensure_ready()?;
        self.tables
            .get(table)
            .map(|table| table.schema.clone())
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
    let mut cursor = Btree::cursor(pager, catalog_root_page_id, CATALOG_TREE_ID)?;
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

    let mut tree_ids = BTreeSet::from([CATALOG_TREE_ID]);
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

    let tables = table_records
        .into_iter()
        .map(|(name, record)| {
            let row_count = usize::try_from(record.row_count)
                .map_err(|_| storage_corrupt("A table row count cannot fit in memory"))?;
            Ok((
                name,
                PagedTable {
                    schema: record.schema,
                    tree_id: record.tree_id,
                    root_page_id: record.root_page_id,
                    row_count,
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let indexes = index_records
        .into_iter()
        .map(|(name, record)| {
            Ok((
                name,
                PagedIndex {
                    definition: record.definition,
                    tree_id: record.tree_id,
                    root_page_id: record.root_page_id,
                    entry_count: usize::try_from(record.entry_count).map_err(|_| {
                        storage_corrupt("An index entry count cannot fit in memory")
                    })?,
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
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
    if table.schema.columns.is_empty() {
        return Err(storage_corrupt(format!(
            "Index `{}` references a table without a typed catalog",
            definition.name
        )));
    }
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
    let mut count = 0_u64;
    let mut cursor = Btree::cursor(pager, root, record.tree_id)?;
    while let Some((key, value)) = cursor.next(pager)? {
        validated_row(&record.schema, &key, &value)?;
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
    let mut count = 0_u64;
    let mut unique_prefixes = HashSet::new();
    let mut cursor = Btree::cursor(pager, root, record.tree_id)?;
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
        let row = validated_row(&table.schema, primary_key, &row_value)?;
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
    let mut count = 0_u64;
    let mut cursor = Btree::cursor(pager, root, table.tree_id)?;
    while let Some((key, value)) = cursor.next(pager)? {
        let row = validated_row(&table.schema, &key, &value)?;
        if encode_secondary_index_entry_key(&table.schema, definition, &row)?.is_some() {
            count = count
                .checked_add(1)
                .ok_or_else(|| storage_corrupt("An expected index entry count overflowed"))?;
        }
    }
    Ok(count)
}

fn validated_row(schema: &TableSchema, key: &[u8], value: &[u8]) -> Result<Row> {
    let row = decode_row(value)?;
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
    if encode_primary_key(schema, &row).map_err(as_storage_corruption)? != key {
        return Err(storage_corrupt(format!(
            "Stored row in `{}` does not match its B-tree key",
            schema.name
        )));
    }
    Ok(row)
}

fn limit_error(message: impl Into<String>) -> EngineError {
    EngineError::new("STORAGE_LIMIT", message)
}

fn storage_corrupt(message: impl Into<String>) -> EngineError {
    EngineError::new("STORAGE_CORRUPT", message)
}

fn as_storage_corruption(error: EngineError) -> EngineError {
    storage_corrupt(error.message)
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    use serde_json::{Value, json};

    use super::*;
    use crate::{
        ColumnDefinition, ColumnType, Engine, MAX_BTREE_KEY_BYTES, MemoryPageDevice, PAGE_SIZE,
        Page, PageDevice, QueryResult, StorageDriver, SuperblockSlot,
    };

    #[derive(Clone)]
    struct CountingDevice {
        inner: Rc<RefCell<MemoryPageDevice>>,
        reads: Rc<Cell<usize>>,
    }

    impl CountingDevice {
        fn new() -> Self {
            Self {
                inner: Rc::new(RefCell::new(MemoryPageDevice::new(0).unwrap())),
                reads: Rc::new(Cell::new(0)),
            }
        }

        fn reset_reads(&self) {
            self.reads.set(0);
        }

        fn reads(&self) -> usize {
            self.reads.get()
        }
    }

    impl PageDevice for CountingDevice {
        fn page_count(&self) -> PageId {
            self.inner.borrow().page_count()
        }

        fn read_page(&mut self, id: PageId, destination: &mut [u8]) -> Result<()> {
            self.reads.set(self.reads.get() + 1);
            self.inner.borrow_mut().read_page(id, destination)
        }

        fn write_page(&mut self, id: PageId, source: &[u8]) -> Result<()> {
            self.inner.borrow_mut().write_page(id, source)
        }

        fn flush(&mut self) -> Result<()> {
            self.inner.borrow_mut().flush()
        }
    }

    #[derive(Default)]
    struct DurableState {
        working: Vec<[u8; PAGE_SIZE]>,
        durable: Vec<[u8; PAGE_SIZE]>,
        flushes: usize,
        fail_after_flush: Option<usize>,
    }

    #[derive(Clone, Default)]
    struct DurableDevice(Rc<RefCell<DurableState>>);

    impl DurableDevice {
        fn arm_after_flush(&self, flush: usize) {
            let mut state = self.0.borrow_mut();
            state.flushes = 0;
            state.fail_after_flush = Some(flush);
        }

        fn crash(&self) {
            let mut state = self.0.borrow_mut();
            state.working = state.durable.clone();
            state.flushes = 0;
            state.fail_after_flush = None;
        }
    }

    impl PageDevice for DurableDevice {
        fn page_count(&self) -> PageId {
            self.0.borrow().working.len() as PageId
        }

        fn read_page(&mut self, id: PageId, destination: &mut [u8]) -> Result<()> {
            let state = self.0.borrow();
            if destination.len() != PAGE_SIZE || id >= state.working.len() as PageId {
                return Err(EngineError::new("DURABLE_DEVICE", "invalid read"));
            }
            destination.copy_from_slice(&state.working[id as usize]);
            Ok(())
        }

        fn write_page(&mut self, id: PageId, source: &[u8]) -> Result<()> {
            let mut state = self.0.borrow_mut();
            if source.len() != PAGE_SIZE {
                return Err(EngineError::new("DURABLE_DEVICE", "invalid write"));
            }
            if id == state.working.len() as PageId {
                state.working.push([0; PAGE_SIZE]);
            } else if id > state.working.len() as PageId {
                return Err(EngineError::new("DURABLE_DEVICE", "non-dense write"));
            }
            state.working[id as usize].copy_from_slice(source);
            Ok(())
        }

        fn flush(&mut self) -> Result<()> {
            let mut state = self.0.borrow_mut();
            state.durable = state.working.clone();
            state.flushes += 1;
            if state.fail_after_flush == Some(state.flushes) {
                state.fail_after_flush = None;
                return Err(EngineError::new(
                    "INJECTED_IO",
                    "failure after durability barrier",
                ));
            }
            Ok(())
        }
    }

    fn row(value: Value) -> Row {
        value.as_object().unwrap().clone()
    }

    fn schema(name: &str, columns: &[(&str, ColumnType, bool)]) -> TableSchema {
        TableSchema {
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
        let mut storage = InMemoryStorage::default();
        storage
            .define_table(schema(
                "authors",
                &[
                    ("id", ColumnType::Integer, false),
                    ("name", ColumnType::Text, false),
                ],
            ))
            .unwrap();
        storage
            .define_table(schema(
                "posts",
                &[
                    ("id", ColumnType::Integer, false),
                    ("author_id", ColumnType::Integer, false),
                    ("state", ColumnType::Text, true),
                    ("rank", ColumnType::Integer, false),
                ],
            ))
            .unwrap();
        storage
            .replace_table(
                "authors",
                vec![
                    row(json!({"id": 1, "name": "Ada"})),
                    row(json!({"id": 2, "name": "Lin"})),
                ],
            )
            .unwrap();
        storage
            .replace_table(
                "posts",
                vec![
                    row(json!({"id": 10, "author_id": 1, "state": "draft", "rank": 2})),
                    row(json!({"id": 11, "author_id": 1, "state": "live", "rank": 1})),
                    row(json!({"id": 12, "author_id": 2, "state": null, "rank": 3})),
                ],
            )
            .unwrap();
        storage
            .define_index(IndexDefinition {
                name: "posts_state_rank".to_owned(),
                table: "posts".to_owned(),
                columns: vec!["state".to_owned(), "rank".to_owned()],
                unique: false,
            })
            .unwrap();
        storage
            .define_index(IndexDefinition {
                name: "posts_author".to_owned(),
                table: "posts".to_owned(),
                columns: vec!["author_id".to_owned()],
                unique: false,
            })
            .unwrap();
        storage
    }

    fn source_with_table_count(table_count: usize) -> InMemoryStorage {
        let mut storage = InMemoryStorage::default();
        for table in 0..table_count {
            storage
                .define_table(TableSchema {
                    name: format!("table_{table:05}"),
                    primary_key: vec!["id".to_owned()],
                    columns: vec![],
                })
                .unwrap();
        }
        storage
    }

    fn source_with_index_count(index_count: usize) -> InMemoryStorage {
        let mut storage = InMemoryStorage::default();
        storage
            .define_table(schema("items", &[("id", ColumnType::Integer, false)]))
            .unwrap();
        for index in 0..index_count {
            storage
                .define_index(IndexDefinition {
                    name: format!("items_{index:05}"),
                    table: "items".to_owned(),
                    columns: vec!["id".to_owned()],
                    unique: false,
                })
                .unwrap();
        }
        storage
    }

    #[test]
    fn sequence_aware_import_publishes_empty_catalog_once_and_reopens() {
        let source = InMemoryStorage::default();
        let imported = PagedStorage::from_in_memory_with_journal_sequence(
            MemoryPageDevice::new(0).unwrap(),
            &source,
            41,
        )
        .unwrap();
        assert_eq!(imported.revision(), 0);
        assert_eq!(imported.applied_journal_sequence(), 41);
        assert!(imported.tables.is_empty());
        assert!(imported.indexes.is_empty());

        let device = imported.into_device();
        let pager = Pager::open_or_create(device).unwrap();
        assert_eq!(pager.generation(), 2);
        assert_eq!(pager.database_revision(), 0);
        assert_eq!(pager.applied_journal_sequence(), 41);
        assert!(pager.catalog_root_page_id().is_some());

        let reopened = PagedStorage::open(pager.into_device()).unwrap();
        assert_eq!(reopened.revision(), 0);
        assert_eq!(reopened.applied_journal_sequence(), 41);
        assert_eq!(reopened.next_tree_id, FIRST_USER_TREE_ID);
        assert!(reopened.tables.is_empty());
        assert!(reopened.indexes.is_empty());

        let ordinary =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
        assert_eq!(ordinary.applied_journal_sequence(), 0);
    }

    #[test]
    fn paged_revision_boundary_publishes_max_and_rejects_the_next_candidate_before_page_work() {
        let mut source = source();
        source
            .set_revision(crate::revision::MAX_DATABASE_REVISION - 1)
            .unwrap();
        let device = CountingDevice::new();
        let control = device.clone();
        let mut paged = PagedStorage::from_in_memory(device, &source).unwrap();
        let outcome = paged
            .apply_batch(&ChangeBatch {
                changes: vec![Change::Upsert {
                    table: "posts".to_owned(),
                    row: row(json!({
                        "id": 10,
                        "author_id": 1,
                        "state": "published",
                        "rank": 2,
                    })),
                }],
                ..ChangeBatch::default()
            })
            .unwrap();
        assert_eq!(outcome.revision, crate::revision::MAX_DATABASE_REVISION);
        let page_count = control.page_count();
        let flush_count = control.inner.borrow().flush_count();
        let error = paged
            .apply_batch(&ChangeBatch {
                changes: vec![Change::Upsert {
                    table: "posts".to_owned(),
                    row: row(json!({
                        "id": 10,
                        "author_id": 1,
                        "state": "archived",
                        "rank": 2,
                    })),
                }],
                ..ChangeBatch::default()
            })
            .unwrap_err();
        assert_eq!(error.code, "REVISION_OVERFLOW");
        assert_eq!(control.page_count(), page_count);
        assert_eq!(control.inner.borrow().flush_count(), flush_count);
        assert!(!paged.pager.borrow().is_recovery_required());

        let reopened = PagedStorage::open(paged.into_device()).unwrap();
        assert_eq!(reopened.revision(), crate::revision::MAX_DATABASE_REVISION);
        assert_eq!(
            reopened
                .lookup_primary_key("posts", &row(json!({"id": 10})))
                .unwrap(),
            Some(row(json!({
                "id": 10,
                "author_id": 1,
                "state": "published",
                "rank": 2,
            })))
        );
    }

    #[test]
    fn paged_import_and_reopen_reject_revisions_above_the_safe_bound() {
        let mut unsupported = InMemoryStorage::default();
        unsupported.set_revision_unchecked_for_test(crate::revision::MAX_DATABASE_REVISION + 1);
        let device = CountingDevice::new();
        let control = device.clone();
        let error = match PagedStorage::from_in_memory(device, &unsupported) {
            Ok(_) => panic!("an unsupported source revision must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.code, "REVISION_OVERFLOW");
        assert_eq!(control.page_count(), crate::FIRST_DATA_PAGE_ID);
        assert!(PagedStorage::open(control).unwrap().tables.is_empty());

        let target =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source()).unwrap();
        let target_device = target.into_device();
        let error = match PagedStorage::from_in_memory(target_device.clone(), &unsupported) {
            Ok(_) => panic!("a nonempty target must win before source revision validation"),
            Err(error) => error,
        };
        assert_eq!(error.code, "DATABASE_NOT_EMPTY");
        assert_eq!(PagedStorage::open(target_device).unwrap().revision(), 2);

        let mut maximum = InMemoryStorage::default();
        maximum
            .set_revision(crate::revision::MAX_DATABASE_REVISION)
            .unwrap();
        let paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &maximum).unwrap();
        let slot = paged.pager.borrow().active_metadata().superblock.slot;
        let mut device = paged.into_device();
        let superblock_page_id = match slot {
            SuperblockSlot::A => 0,
            SuperblockSlot::B => 1,
        };
        let mut bytes = [0; PAGE_SIZE];
        device.read_page(superblock_page_id, &mut bytes).unwrap();
        let mut page = Page::decode(&bytes).unwrap();
        page.payload[32..40]
            .copy_from_slice(&(crate::revision::MAX_DATABASE_REVISION + 1).to_le_bytes());
        device
            .write_page(superblock_page_id, &page.encode().unwrap())
            .unwrap();
        device.flush().unwrap();
        let error = match PagedStorage::open(device) {
            Ok(_) => panic!("an unsupported persisted revision must fail closed"),
            Err(error) => error,
        };
        assert_eq!(error.code, "UNSUPPORTED_PAGE");
    }

    #[test]
    fn sequence_aware_import_assigns_monotonic_trees_and_preserves_watermark() {
        let source = source();
        let revision = source.revision();
        let imported = PagedStorage::from_in_memory_with_journal_sequence(
            MemoryPageDevice::new(0).unwrap(),
            &source,
            73,
        )
        .unwrap();
        assert_eq!(imported.revision(), revision);
        assert_eq!(imported.applied_journal_sequence(), 73);
        assert_eq!(imported.tables["authors"].tree_id, FIRST_USER_TREE_ID);
        assert_eq!(imported.tables["posts"].tree_id, FIRST_USER_TREE_ID + 1);
        assert_eq!(
            imported.indexes["posts_author"].tree_id,
            FIRST_USER_TREE_ID + 2
        );
        assert_eq!(
            imported.indexes["posts_state_rank"].tree_id,
            FIRST_USER_TREE_ID + 3
        );
        assert_eq!(imported.next_tree_id, FIRST_USER_TREE_ID + 4);

        let mut reopened = PagedStorage::open(imported.into_device()).unwrap();
        assert_eq!(reopened.revision(), revision);
        assert_eq!(reopened.applied_journal_sequence(), 73);
        assert_eq!(reopened.next_tree_id, FIRST_USER_TREE_ID + 4);
        reopened
            .apply_batch(&ChangeBatch {
                changes: vec![Change::Upsert {
                    table: "posts".to_owned(),
                    row: row(json!({
                        "id": 13,
                        "author_id": 2,
                        "state": "live",
                        "rank": 4,
                    })),
                }],
                ..ChangeBatch::default()
            })
            .unwrap();
        assert_eq!(reopened.applied_journal_sequence(), 73);
        let reopened = PagedStorage::open(reopened.into_device()).unwrap();
        assert_eq!(reopened.applied_journal_sequence(), 73);
    }

    #[test]
    fn import_operation_accounting_has_an_exact_accepted_boundary() {
        assert_eq!(import_table_operation_count(0, 2, 1).unwrap(), 0);
        assert_eq!(import_table_operation_count(1, 2, 1).unwrap(), 15);

        // One catalog root, one header, and one table record add three fixed operations. A
        // nonempty, unindexed table adds one root and four operations per row.
        let accepted_rows = (MAX_PAGED_BATCH_OPERATIONS - 4) / 4;
        let accepted = 3 + import_table_operation_count(accepted_rows, 0, 0).unwrap();
        assert_eq!(accepted, MAX_PAGED_BATCH_OPERATIONS);
        let rejected = 3 + import_table_operation_count(accepted_rows + 1, 0, 0).unwrap();
        assert!(rejected > MAX_PAGED_BATCH_OPERATIONS);
    }

    #[test]
    fn import_operation_limit_precedes_candidate_page_allocation() {
        let mut source = InMemoryStorage::default();
        let schema = schema("items", &[("id", ColumnType::Integer, false)]);
        let rows = (0..700)
            .map(|id| row(json!({"id": id})))
            .collect::<Vec<_>>();
        source.replace_table_snapshot(schema, rows).unwrap();
        for index in 0..512 {
            source
                .define_index(IndexDefinition {
                    name: format!("items_{index:04}"),
                    table: "items".to_owned(),
                    columns: vec!["id".to_owned()],
                    unique: false,
                })
                .unwrap();
        }

        let device = CountingDevice::new();
        let control = device.clone();
        let error = match PagedStorage::from_in_memory(device, &source) {
            Ok(_) => panic!("an over-budget import must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.code, "TRANSACTION_TOO_LARGE");
        assert_eq!(control.page_count(), crate::FIRST_DATA_PAGE_ID);
        assert!(PagedStorage::open(control).unwrap().tables.is_empty());
    }

    #[test]
    fn import_streams_many_primary_and_unique_keys_without_cumulative_sets() {
        let mut source = InMemoryStorage::default();
        let schema = schema(
            "items",
            &[
                ("id", ColumnType::Integer, false),
                ("tag", ColumnType::Text, false),
            ],
        );
        let rows = (0..2_048)
            .map(|id| row(json!({"id": id, "tag": format!("tag_{id:04}")})))
            .collect::<Vec<_>>();
        source.replace_table_snapshot(schema, rows).unwrap();
        source
            .define_index(IndexDefinition {
                name: "items_tag".to_owned(),
                table: "items".to_owned(),
                columns: vec!["tag".to_owned()],
                unique: true,
            })
            .unwrap();

        let imported =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
        assert_eq!(imported.table_row_count("items").unwrap(), 2_048);
        assert_eq!(imported.indexes["items_tag"].entry_count, 2_048);
        let reopened = PagedStorage::open(imported.into_device()).unwrap();
        assert_eq!(reopened.table_row_count("items").unwrap(), 2_048);
        assert_eq!(reopened.indexes["items_tag"].entry_count, 2_048);
    }

    #[test]
    fn import_candidate_probe_rejects_canonical_primary_key_aliases() {
        let mut source = InMemoryStorage::default();
        source
            .replace_table_snapshot(
                schema("measurements", &[("id", ColumnType::Float, false)]),
                vec![row(json!({"id": -0.0})), row(json!({"id": 0.0}))],
            )
            .unwrap();
        assert_eq!(source.table_rows("measurements").unwrap().len(), 2);

        let device = CountingDevice::new();
        let control = device.clone();
        let error = match PagedStorage::from_in_memory(device, &source) {
            Ok(_) => panic!("canonical primary-key aliases must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.code, "CONSTRAINT_VIOLATION");
        assert!(PagedStorage::open(control).unwrap().tables.is_empty());
    }

    #[test]
    fn import_metadata_models_and_transients_share_one_hard_byte_bound() {
        ensure_import_peak(MAX_PAGED_BATCH_BYTES - 1, 1).unwrap();
        assert_eq!(
            ensure_import_peak(MAX_PAGED_BATCH_BYTES, 1)
                .unwrap_err()
                .code,
            "TRANSACTION_TOO_LARGE"
        );
        assert_eq!(import_catalog_codec_work(7).unwrap(), 21);
        assert_eq!(
            import_catalog_codec_work(usize::MAX).unwrap_err().code,
            "TRANSACTION_TOO_LARGE"
        );
        assert_eq!(
            import_row_encoding_work(10, 20).unwrap(),
            40 + crate::MAX_BTREE_KEY_BYTES + 64
        );
        assert_eq!(
            import_row_encoding_work(usize::MAX, 0).unwrap_err().code,
            "TRANSACTION_TOO_LARGE"
        );

        let mut source = InMemoryStorage::default();
        let table_suffix = "t".repeat(850);
        let primary_key = vec![
            "a".repeat(900),
            "b".repeat(900),
            "c".repeat(900),
            "d".repeat(900),
        ];
        for table in 0..MAX_CATALOG_TABLES {
            source
                .define_table(TableSchema {
                    name: format!("table_{table:04}_{table_suffix}"),
                    primary_key: primary_key.clone(),
                    columns: Vec::new(),
                })
                .unwrap();
        }
        let device = CountingDevice::new();
        let control = device.clone();
        let error = match PagedStorage::from_in_memory(device, &source) {
            Ok(_) => panic!("cumulative catalog models must share the import byte bound"),
            Err(error) => error,
        };
        assert_eq!(error.code, "TRANSACTION_TOO_LARGE");
        assert_eq!(control.page_count(), crate::FIRST_DATA_PAGE_ID);
    }

    #[test]
    fn import_bounds_catalog_codec_models_before_provisional_clones() {
        let mut source = InMemoryStorage::default();
        source
            .define_table(TableSchema {
                name: "wide".to_owned(),
                primary_key: vec!["id".to_owned()],
                columns: vec![
                    ColumnDefinition {
                        name: "id".to_owned(),
                        data_type: ColumnType::Integer,
                        nullable: false,
                        default: None,
                    },
                    ColumnDefinition {
                        name: "metadata".to_owned(),
                        data_type: ColumnType::Json,
                        nullable: false,
                        // Compact on disk, but serde's caller clone, `Value` clone, and canonical
                        // buffers coexist and exceed the 16-MiB import transient contract.
                        default: Some(Value::Array(vec![Value::Null; 100_000])),
                    },
                ],
            })
            .unwrap();

        let device = CountingDevice::new();
        let control = device.clone();
        let error = match PagedStorage::from_in_memory(device, &source) {
            Ok(_) => panic!("catalog codec models must be bounded before provisional clones"),
            Err(error) => error,
        };
        assert_eq!(error.code, "TRANSACTION_TOO_LARGE");
        assert_eq!(control.page_count(), crate::FIRST_DATA_PAGE_ID);
    }

    #[test]
    fn import_bounds_escaped_catalog_defaults_against_retained_metadata() {
        let escaped_default = Value::String("\u{0001}".repeat(160_000));
        let escaped_bytes = crate::storage::validate_json_value(&escaped_default).unwrap();
        assert_eq!(escaped_bytes, 960_002);
        let escaped_schema = TableSchema {
            name: "wide".to_owned(),
            primary_key: vec!["id".to_owned()],
            columns: vec![
                ColumnDefinition {
                    name: "id".to_owned(),
                    data_type: ColumnType::Integer,
                    nullable: false,
                    default: None,
                },
                ColumnDefinition {
                    name: "metadata".to_owned(),
                    data_type: ColumnType::Json,
                    nullable: false,
                    default: Some(escaped_default),
                },
            ],
        };
        let model_only =
            import_catalog_codec_work(estimated_schema_work_bytes(&escaped_schema).unwrap())
                .unwrap();
        assert_eq!(
            import_table_catalog_codec_work(&escaped_schema).unwrap(),
            model_only + 2 * escaped_bytes
        );

        let mut source = InMemoryStorage::default();
        let suffix = "t".repeat(800);
        let primary_key = vec![
            "a".repeat(900),
            "b".repeat(900),
            "c".repeat(900),
            "d".repeat(900),
        ];
        for table in 0..3_000 {
            source
                .define_table(TableSchema {
                    name: format!("table_{table:04}_{suffix}"),
                    primary_key: primary_key.clone(),
                    columns: Vec::new(),
                })
                .unwrap();
        }
        source.define_table(escaped_schema).unwrap();

        let device = CountingDevice::new();
        let control = device.clone();
        let error = match PagedStorage::from_in_memory(device, &source) {
            Ok(_) => panic!("escaped catalog buffers must share the retained metadata bound"),
            Err(error) => error,
        };
        assert_eq!(error.code, "TRANSACTION_TOO_LARGE");
        assert_eq!(control.page_count(), crate::FIRST_DATA_PAGE_ID);
    }

    #[test]
    fn rootless_recovery_watermark_is_neither_empty_nor_an_import_target() {
        let mut pager = Pager::open_or_create(CountingDevice::new()).unwrap();
        pager.begin_write().unwrap().commit(0, 17, None).unwrap();
        assert_eq!(pager.active_metadata().superblock.live_data_page_count, 0);
        assert_eq!(pager.applied_journal_sequence(), 17);
        let device = pager.into_device();

        let oversized = source_with_table_count(MAX_CATALOG_TABLES as usize + 1);
        let error = match PagedStorage::from_in_memory(device.clone(), &oversized) {
            Ok(_) => panic!("a rootless recovery marker must reject import before source bounds"),
            Err(error) => error,
        };
        assert_eq!(error.code, "DATABASE_NOT_EMPTY");
        assert_eq!(device.page_count(), crate::FIRST_DATA_PAGE_ID);

        let error = match PagedStorage::open(device) {
            Ok(_) => panic!("a rootless recovery marker must fail closed on reopen"),
            Err(error) => error,
        };
        assert_eq!(error.code, "STORAGE_CORRUPT");
    }

    #[test]
    fn import_bounds_fail_before_candidate_pages_and_nonempty_target_wins() {
        let maximum = preflight_import_catalog_bounds(
            MAX_CATALOG_TABLES as usize,
            MAX_CATALOG_INDEXES as usize,
        )
        .unwrap();
        assert_eq!(maximum.table_count, MAX_CATALOG_TABLES);
        assert_eq!(maximum.index_count, MAX_CATALOG_INDEXES);
        assert_eq!(
            maximum.next_tree_id,
            FIRST_USER_TREE_ID + u64::from(MAX_CATALOG_TABLES + MAX_CATALOG_INDEXES)
        );
        assert!(maximum.next_tree_id <= MAX_TREE_ID);
        assert_eq!(
            advance_import_tree_id(MAX_TREE_ID - 1).unwrap(),
            MAX_TREE_ID
        );
        assert_eq!(
            advance_import_tree_id(MAX_TREE_ID).unwrap_err().code,
            "STORAGE_LIMIT"
        );

        let too_many_tables = source_with_table_count(MAX_CATALOG_TABLES as usize + 1);
        let device = CountingDevice::new();
        let control = device.clone();
        let error = match PagedStorage::from_in_memory_with_journal_sequence(
            device,
            &too_many_tables,
            99,
        ) {
            Ok(_) => panic!("an oversized table catalog must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.code, "STORAGE_LIMIT");
        assert_eq!(control.page_count(), crate::FIRST_DATA_PAGE_ID);
        let rootless = PagedStorage::open(control).unwrap();
        assert_eq!(rootless.applied_journal_sequence(), 0);
        assert!(rootless.tables.is_empty());

        let too_many_indexes = source_with_index_count(MAX_CATALOG_INDEXES as usize + 1);
        let device = CountingDevice::new();
        let control = device.clone();
        let error = match PagedStorage::from_in_memory(device, &too_many_indexes) {
            Ok(_) => panic!("an oversized index catalog must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.code, "STORAGE_LIMIT");
        assert_eq!(control.page_count(), crate::FIRST_DATA_PAGE_ID);
        assert!(PagedStorage::open(control).unwrap().tables.is_empty());

        let target =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source()).unwrap();
        let target_device = target.into_device();
        let error = match PagedStorage::from_in_memory_with_journal_sequence(
            target_device.clone(),
            &too_many_tables,
            99,
        ) {
            Ok(_) => panic!("a nonempty target must be rejected before source preflight"),
            Err(error) => error,
        };
        assert_eq!(error.code, "DATABASE_NOT_EMPTY");
        let reopened = PagedStorage::open(target_device).unwrap();
        assert_eq!(reopened.applied_journal_sequence(), 0);
        assert_eq!(reopened.table_row_count("posts").unwrap(), 3);
    }

    #[test]
    fn interrupted_sequence_import_reopens_at_the_old_or_new_watermark() {
        let source = source();
        for (failing_flush, expected_new) in [(1, false), (2, false), (3, true)] {
            let device = DurableDevice::default();
            let control = device.clone();
            let device = Pager::open_or_create(device).unwrap().into_device();
            control.arm_after_flush(failing_flush);

            let error =
                match PagedStorage::from_in_memory_with_journal_sequence(device, &source, 101) {
                    Ok(_) => panic!("an injected import failure must not return success"),
                    Err(error) => error,
                };
            assert_eq!(
                error.code,
                if expected_new {
                    "RECOVERY_REQUIRED"
                } else {
                    "INJECTED_IO"
                }
            );

            control.crash();
            let reopened = PagedStorage::open(control.clone()).unwrap();
            assert_eq!(
                reopened.applied_journal_sequence(),
                if expected_new { 101 } else { 0 }
            );
            assert_eq!(reopened.table_schema("posts").is_ok(), expected_new);
            assert_eq!(
                reopened.revision(),
                if expected_new { source.revision() } else { 0 }
            );
        }
    }

    #[test]
    fn table_definitions_are_atomic_same_revision_and_reopenable() {
        let device = MemoryPageDevice::new(0).unwrap();
        let mut paged = PagedStorage::open(device).unwrap();
        assert_eq!(paged.revision(), 0);

        // Even an empty initializer publishes the canonical catalog root at the same revision.
        assert!(paged.define_tables_with_publication(vec![]).unwrap());
        let device = paged.into_device();
        let pager = Pager::open_or_create(device).unwrap();
        assert_eq!(pager.database_revision(), 0);
        assert!(pager.catalog_root_page_id().is_some());

        let mut paged = PagedStorage::open(pager.into_device()).unwrap();
        assert!(!paged.define_tables_with_publication(vec![]).unwrap());
        let accounts = schema(
            "accounts",
            &[
                ("id", ColumnType::Integer, false),
                ("name", ColumnType::Text, false),
            ],
        );
        let notes = schema(
            "notes",
            &[
                ("id", ColumnType::Integer, false),
                ("body", ColumnType::Text, true),
            ],
        );
        paged
            .define_tables(vec![notes.clone(), accounts.clone()])
            .unwrap();
        assert_eq!(paged.revision(), 0);
        assert_eq!(paged.table_schema("accounts").unwrap(), accounts);
        assert_eq!(paged.table_schema("notes").unwrap(), notes);

        paged.define_table(accounts.clone()).unwrap();
        assert_eq!(paged.revision(), 0);
        let conflicting = TableSchema {
            primary_key: vec!["name".to_owned()],
            ..accounts.clone()
        };
        assert_eq!(
            paged
                .define_tables(vec![
                    schema("would_have_existed", &[("id", ColumnType::Integer, false)]),
                    conflicting,
                ])
                .unwrap_err()
                .code,
            "INVALID_SCHEMA"
        );
        assert_eq!(
            paged.table_schema("would_have_existed").unwrap_err().code,
            "TABLE_NOT_FOUND"
        );

        let reopened = PagedStorage::open(paged.into_device()).unwrap();
        assert_eq!(reopened.revision(), 0);
        assert_eq!(reopened.table_schema("accounts").unwrap(), accounts);
        assert_eq!(reopened.table_schema("notes").unwrap(), notes);
    }

    #[test]
    fn interrupted_table_definition_reopens_at_the_old_or_new_catalog() {
        for (failing_flush, expected_new) in [(1, false), (2, false), (3, true)] {
            let device = DurableDevice::default();
            let control = device.clone();
            let mut paged = PagedStorage::open(device).unwrap();
            control.arm_after_flush(failing_flush);

            let error = paged
                .define_table(schema("accounts", &[("id", ColumnType::Integer, false)]))
                .unwrap_err();
            if expected_new {
                assert_eq!(error.code, "RECOVERY_REQUIRED");
                assert_eq!(
                    paged.table_schema("accounts").unwrap_err().code,
                    "RECOVERY_REQUIRED"
                );
            } else {
                assert_eq!(error.code, "INJECTED_IO");
                assert_eq!(
                    paged.table_schema("accounts").unwrap_err().code,
                    "TABLE_NOT_FOUND"
                );
            }

            let device = paged.into_device();
            control.crash();
            let reopened = PagedStorage::open(device).unwrap();
            assert_eq!(reopened.revision(), 0);
            assert_eq!(reopened.table_schema("accounts").is_ok(), expected_new);
        }
    }

    #[test]
    fn index_definitions_use_monotonic_tree_ids_and_keep_empty_roots_absent() {
        let mut paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source()).unwrap();
        let first_index_tree_id = paged.next_tree_id;
        let revision = paged.revision();
        paged
            .create_index_and_advance(IndexDefinition {
                name: "posts_rank".to_owned(),
                table: "posts".to_owned(),
                columns: vec!["rank".to_owned()],
                unique: false,
            })
            .unwrap();
        assert_eq!(paged.revision(), revision + 1);
        assert_eq!(paged.indexes["posts_rank"].tree_id, first_index_tree_id);
        assert_eq!(paged.indexes["posts_rank"].entry_count, 3);

        paged
            .define_table(schema(
                "empty",
                &[
                    ("id", ColumnType::Integer, false),
                    ("value", ColumnType::Text, true),
                ],
            ))
            .unwrap();
        let empty_index_tree_id = paged.next_tree_id;
        paged
            .create_index_and_advance(IndexDefinition {
                name: "empty_value".to_owned(),
                table: "empty".to_owned(),
                columns: vec!["value".to_owned()],
                unique: true,
            })
            .unwrap();
        assert_eq!(paged.indexes["empty_value"].tree_id, empty_index_tree_id);
        assert_eq!(paged.indexes["empty_value"].root_page_id, None);
        assert_eq!(paged.indexes["empty_value"].entry_count, 0);

        let reopened = PagedStorage::open(paged.into_device()).unwrap();
        assert_eq!(reopened.indexes["posts_rank"].tree_id, first_index_tree_id);
        assert_eq!(reopened.indexes["empty_value"].tree_id, empty_index_tree_id);
        assert_eq!(reopened.indexes["empty_value"].root_page_id, None);
    }

    #[test]
    fn index_build_streams_a_table_while_candidate_pages_exceed_the_cache() {
        let mut source = InMemoryStorage::default();
        source
            .define_table(schema(
                "items",
                &[
                    ("id", ColumnType::Integer, false),
                    ("bucket", ColumnType::Integer, false),
                ],
            ))
            .unwrap();
        source
            .replace_table(
                "items",
                (0..2_000)
                    .map(|id| row(json!({"id": id, "bucket": id % 17})))
                    .collect(),
            )
            .unwrap();
        let device = CountingDevice::new();
        let control = device.clone();
        let imported = PagedStorage::from_in_memory(device, &source).unwrap();
        let device = imported.into_device();
        let mut pager = Pager::with_cache_capacity(device, PAGE_SIZE).unwrap();
        let revision = pager.database_revision();
        let catalog_root = pager.catalog_root_page_id().unwrap();
        let (next_tree_id, tables, indexes) =
            load_and_validate_catalog(&mut pager, catalog_root).unwrap();
        let mut paged = PagedStorage {
            pager: RefCell::new(pager),
            revision,
            next_tree_id,
            tables,
            indexes,
            recovery_required: false,
        };
        control.reset_reads();

        paged
            .create_index_and_advance(IndexDefinition {
                name: "items_bucket".to_owned(),
                table: "items".to_owned(),
                columns: vec!["bucket".to_owned()],
                unique: false,
            })
            .unwrap();
        assert_eq!(paged.indexes["items_bucket"].entry_count, 2_000);
        assert!(control.reads() > 1);
        let mut matches = 0usize;
        paged
            .visit_index(
                "items",
                &["bucket".to_owned()],
                &row(json!({"bucket": 3})),
                &mut |_| {
                    matches += 1;
                    Ok(VisitControl::Continue)
                },
            )
            .unwrap();
        assert_eq!(matches, 118);

        let reopened = PagedStorage::open(paged.into_device()).unwrap();
        assert_eq!(reopened.indexes["items_bucket"].entry_count, 2_000);
    }

    #[test]
    fn add_column_streams_a_table_larger_than_the_transient_work_budget() {
        const ROWS: usize = 520;
        const PAYLOAD_BYTES: usize = 32 * 1024;
        let cumulative_payload_bytes = ROWS * PAYLOAD_BYTES;
        assert!(cumulative_payload_bytes > MAX_PAGED_BATCH_BYTES);
        let schema = schema(
            "large_items",
            &[
                ("id", ColumnType::Integer, false),
                ("payload", ColumnType::Text, false),
            ],
        );
        let mut source = InMemoryStorage::default();
        source.define_table(schema).unwrap();
        source
            .replace_table(
                "large_items",
                (0..ROWS)
                    .map(|id| {
                        row(json!({
                            "id": id,
                            "payload": format!("{id:04}{}", "x".repeat(PAYLOAD_BYTES - 4)),
                        }))
                    })
                    .collect(),
            )
            .unwrap();

        let device = CountingDevice::new();
        let control = device.clone();
        let imported = PagedStorage::from_in_memory(device, &source).unwrap();
        let device = imported.into_device();
        let mut pager = Pager::with_cache_capacity(device, PAGE_SIZE).unwrap();
        let revision = pager.database_revision();
        let catalog_root = pager.catalog_root_page_id().unwrap();
        let (next_tree_id, tables, indexes) =
            load_and_validate_catalog(&mut pager, catalog_root).unwrap();
        let mut paged = PagedStorage {
            pager: RefCell::new(pager),
            revision,
            next_tree_id,
            tables,
            indexes,
            recovery_required: false,
        };
        control.reset_reads();
        paged
            .add_column_and_advance(
                "large_items",
                &ColumnDefinition {
                    name: "active".to_owned(),
                    data_type: ColumnType::Boolean,
                    nullable: false,
                    default: Some(json!(false)),
                },
            )
            .unwrap();
        assert_eq!(paged.table_row_count("large_items").unwrap(), ROWS);
        assert!(control.reads() > 1);
        for id in [0, ROWS - 1] {
            let row = paged
                .lookup_primary_key("large_items", &row(json!({"id": id})))
                .unwrap()
                .unwrap();
            assert_eq!(row["active"], false);
            assert_eq!(row["payload"].as_str().unwrap().len(), PAYLOAD_BYTES);
        }
    }

    #[test]
    fn interrupted_index_build_reopens_at_the_old_or_new_catalog() {
        for (failing_flush, expected_new) in [(1, false), (2, false), (3, true)] {
            let device = DurableDevice::default();
            let control = device.clone();
            let mut paged = PagedStorage::from_in_memory(device, &source()).unwrap();
            let old_revision = paged.revision();
            control.arm_after_flush(failing_flush);
            let error = paged
                .create_index_and_advance(IndexDefinition {
                    name: "posts_rank".to_owned(),
                    table: "posts".to_owned(),
                    columns: vec!["rank".to_owned()],
                    unique: false,
                })
                .unwrap_err();
            if expected_new {
                assert_eq!(error.code, "RECOVERY_REQUIRED");
                assert_eq!(
                    paged.indexes_for_table("posts").unwrap_err().code,
                    "RECOVERY_REQUIRED"
                );
                for if_not_exists in [false, true] {
                    let error = crate::statement::plan_create_index(
                        &paged,
                        &IndexDefinition {
                            // This name exists in the last confirmed catalog. The requested table
                            // is deliberately invalid: duplicate-name ordering must be retained,
                            // but poisoned storage must fail before either duplicate result.
                            name: "posts_author".to_owned(),
                            table: "missing".to_owned(),
                            columns: vec![],
                            unique: true,
                        },
                        if_not_exists,
                    )
                    .unwrap_err();
                    assert_eq!(error.code, "RECOVERY_REQUIRED");
                }
            } else {
                assert_eq!(error.code, "INJECTED_IO");
                assert!(paged.index_definition("posts_rank").is_none());
                assert_eq!(paged.revision(), old_revision);
            }

            let device = paged.into_device();
            control.crash();
            let reopened = PagedStorage::open(device).unwrap();
            assert_eq!(
                reopened.index_definition("posts_rank").is_some(),
                expected_new
            );
            assert_eq!(reopened.revision(), old_revision + u64::from(expected_new));
        }
    }

    #[test]
    fn snapshot_replacement_allocates_fresh_trees_rebuilds_indexes_and_handles_empty_rows() {
        let mut source = source();
        source
            .define_index(IndexDefinition {
                name: "posts_rank_unique".to_owned(),
                table: "posts".to_owned(),
                columns: vec!["rank".to_owned()],
                unique: true,
            })
            .unwrap();
        let schema = source.table_schema("posts").unwrap();
        let mut paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
        let old_table_tree = paged.tables["posts"].tree_id;
        let old_index_trees = paged
            .indexes
            .values()
            .filter(|index| index.definition.table == "posts")
            .map(|index| (index.definition.name.clone(), index.tree_id))
            .collect::<BTreeMap<_, _>>();
        let first_tree_id = paged.next_tree_id;
        let revision = paged.revision();

        let outcome = paged
            .replace_table_snapshot(
                schema.clone(),
                vec![
                    row(json!({"id": 20, "author_id": 1, "state": "live", "rank": 8})),
                    row(json!({"id": 21, "author_id": 1, "state": null, "rank": 9})),
                ],
            )
            .unwrap();
        assert_eq!(outcome.revision, revision + 1);
        assert_eq!(outcome.tables, vec!["posts"]);
        assert_eq!(paged.tables["posts"].tree_id, first_tree_id);
        assert_ne!(paged.tables["posts"].tree_id, old_table_tree);
        for (offset, (name, index)) in paged
            .indexes
            .iter()
            .filter(|(_, index)| index.definition.table == "posts")
            .enumerate()
        {
            assert_eq!(index.tree_id, first_tree_id + 1 + offset as u64);
            assert_ne!(index.tree_id, old_index_trees[name]);
        }
        assert_eq!(paged.indexes["posts_state_rank"].entry_count, 1);
        assert_eq!(paged.indexes["posts_author"].entry_count, 2);
        assert_eq!(paged.indexes["posts_rank_unique"].entry_count, 2);
        let after_nonempty_next_tree_id = paged.next_tree_id;

        paged.replace_table_snapshot(schema, Vec::new()).unwrap();
        assert_eq!(paged.revision(), revision + 2);
        assert_eq!(paged.tables["posts"].tree_id, after_nonempty_next_tree_id);
        assert_eq!(paged.tables["posts"].root_page_id, None);
        assert_eq!(paged.tables["posts"].row_count, 0);
        for index in paged
            .indexes
            .values()
            .filter(|index| index.definition.table == "posts")
        {
            assert_eq!(index.root_page_id, None);
            assert_eq!(index.entry_count, 0);
        }

        let reopened = PagedStorage::open(paged.into_device()).unwrap();
        assert_eq!(reopened.revision(), revision + 2);
        assert_eq!(reopened.table_row_count("posts").unwrap(), 0);
        assert_eq!(reopened.next_tree_id, after_nonempty_next_tree_id + 4);
    }

    #[test]
    fn snapshot_replacement_matches_shared_primary_and_secondary_key_limit_errors() {
        let schema = schema(
            "items",
            &[
                ("id", ColumnType::Text, false),
                ("tag", ColumnType::Text, false),
            ],
        );
        let mut source = InMemoryStorage::default();
        source.define_table(schema.clone()).unwrap();
        source
            .define_index(IndexDefinition {
                name: "items_tag".to_owned(),
                table: "items".to_owned(),
                columns: vec!["tag".to_owned()],
                unique: false,
            })
            .unwrap();

        for rows in [
            vec![row(json!({
                "id": "x".repeat(MAX_BTREE_KEY_BYTES),
                "tag": "short",
            }))],
            vec![row(json!({
                "id": "short",
                "tag": "x".repeat(MAX_BTREE_KEY_BYTES),
            }))],
        ] {
            let expected = source
                .clone()
                .replace_table_snapshot(schema.clone(), rows.clone())
                .unwrap_err();
            let mut paged =
                PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
            let revision = paged.revision();
            let actual = paged
                .replace_table_snapshot(schema.clone(), rows)
                .unwrap_err();
            assert_eq!(actual, expected);
            assert_eq!(actual.code, "INVALID_CHANGE");
            assert_eq!(paged.revision(), revision);
            assert_eq!(paged.table_row_count("items").unwrap(), 0);
        }
    }

    #[test]
    fn snapshot_replacement_matches_primary_key_limit_without_indexes() {
        let schema = schema("items", &[("id", ColumnType::Text, false)]);
        let mut source = InMemoryStorage::default();
        source.define_table(schema.clone()).unwrap();
        let rows = vec![row(json!({"id": "x".repeat(MAX_BTREE_KEY_BYTES)}))];
        let expected = source
            .clone()
            .replace_table_snapshot(schema.clone(), rows.clone())
            .unwrap_err();
        assert_eq!(expected.code, "INVALID_CHANGE");

        let mut paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
        let revision = paged.revision();
        let actual = paged.replace_table_snapshot(schema, rows).unwrap_err();
        assert_eq!(actual, expected);
        assert_eq!(paged.revision(), revision);
        assert_eq!(paged.table_row_count("items").unwrap(), 0);
    }

    #[test]
    fn duplicate_unique_snapshot_aborts_with_old_catalog_rows_and_tree_ids_intact() {
        let mut source = InMemoryStorage::default();
        let schema = schema(
            "accounts",
            &[
                ("id", ColumnType::Integer, false),
                ("email", ColumnType::Text, true),
            ],
        );
        source.define_table(schema.clone()).unwrap();
        source
            .replace_table(
                "accounts",
                vec![
                    row(json!({"id": 1, "email": "ada@example.com"})),
                    row(json!({"id": 2, "email": null})),
                ],
            )
            .unwrap();
        source
            .define_index(IndexDefinition {
                name: "accounts_email".to_owned(),
                table: "accounts".to_owned(),
                columns: vec!["email".to_owned()],
                unique: true,
            })
            .unwrap();
        let mut paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
        let revision = paged.revision();
        let next_tree_id = paged.next_tree_id;
        let table_tree_id = paged.tables["accounts"].tree_id;
        let index_tree_id = paged.indexes["accounts_email"].tree_id;
        let before = paged.scan_table("accounts").unwrap();

        let error = paged
            .replace_table_snapshot(
                schema,
                vec![
                    row(json!({"id": 3, "email": "same@example.com"})),
                    row(json!({"id": 4, "email": "same@example.com"})),
                ],
            )
            .unwrap_err();
        assert_eq!(error.code, "CONSTRAINT_VIOLATION");
        assert_eq!(paged.revision(), revision);
        assert_eq!(paged.next_tree_id, next_tree_id);
        assert_eq!(paged.tables["accounts"].tree_id, table_tree_id);
        assert_eq!(paged.indexes["accounts_email"].tree_id, index_tree_id);
        assert_eq!(paged.scan_table("accounts").unwrap(), before);

        let reopened = PagedStorage::open(paged.into_device()).unwrap();
        assert_eq!(reopened.revision(), revision);
        assert_eq!(reopened.scan_table("accounts").unwrap(), before);
    }

    #[test]
    fn duplicate_primary_key_precedes_oversized_secondary_key_like_in_memory() {
        let schema = schema(
            "items",
            &[
                ("id", ColumnType::Integer, false),
                ("tag", ColumnType::Text, false),
            ],
        );
        let mut source = InMemoryStorage::default();
        source.define_table(schema.clone()).unwrap();
        source
            .define_index(IndexDefinition {
                name: "items_tag".to_owned(),
                table: "items".to_owned(),
                columns: vec!["tag".to_owned()],
                unique: false,
            })
            .unwrap();
        let rows = vec![
            row(json!({"id": 1, "tag": "x".repeat(MAX_BTREE_KEY_BYTES)})),
            row(json!({"id": 1, "tag": "short"})),
        ];
        let expected = source
            .clone()
            .replace_table_snapshot(schema.clone(), rows.clone())
            .unwrap_err();
        assert_eq!(expected.code, "INVALID_CHANGE");
        assert!(expected.message.contains("duplicate primary key"));

        let mut paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
        let revision = paged.revision();
        let next_tree_id = paged.next_tree_id;
        let actual = paged.replace_table_snapshot(schema, rows).unwrap_err();
        assert_eq!(actual, expected);
        assert_eq!(paged.revision(), revision);
        assert_eq!(paged.next_tree_id, next_tree_id);
        assert_eq!(paged.table_row_count("items").unwrap(), 0);
    }

    #[test]
    fn earlier_unique_index_violation_precedes_a_later_oversized_index_key() {
        let schema = schema(
            "items",
            &[
                ("id", ColumnType::Integer, false),
                ("early", ColumnType::Text, false),
                ("late", ColumnType::Text, false),
            ],
        );
        let mut source = InMemoryStorage::default();
        source.define_table(schema.clone()).unwrap();
        source
            .define_index(IndexDefinition {
                name: "a_early_unique".to_owned(),
                table: "items".to_owned(),
                columns: vec!["early".to_owned()],
                unique: true,
            })
            .unwrap();
        source
            .define_index(IndexDefinition {
                name: "z_late".to_owned(),
                table: "items".to_owned(),
                columns: vec!["late".to_owned()],
                unique: false,
            })
            .unwrap();
        let rows = vec![
            row(json!({"id": 2, "early": "same", "late": "short"})),
            row(json!({
                "id": 1,
                "early": "same",
                "late": "x".repeat(MAX_BTREE_KEY_BYTES),
            })),
        ];
        let expected = source
            .clone()
            .replace_table_snapshot(schema.clone(), rows.clone())
            .unwrap_err();
        assert_eq!(expected.code, "CONSTRAINT_VIOLATION");
        assert!(expected.message.contains("a_early_unique"));

        let mut paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
        let revision = paged.revision();
        let actual = paged.replace_table_snapshot(schema, rows).unwrap_err();
        assert_eq!(actual, expected);
        assert_eq!(paged.revision(), revision);
        assert_eq!(paged.table_row_count("items").unwrap(), 0);
    }

    #[test]
    fn interrupted_snapshot_replacement_reopens_at_the_complete_old_or_new_generation() {
        for (failing_flush, expected_new) in [(1, false), (2, false), (3, true)] {
            let device = DurableDevice::default();
            let control = device.clone();
            let source = source();
            let schema = source.table_schema("posts").unwrap();
            let mut paged = PagedStorage::from_in_memory(device, &source).unwrap();
            let revision = paged.revision();
            control.arm_after_flush(failing_flush);
            let error = paged
                .replace_table_snapshot(
                    schema,
                    vec![row(json!({
                        "id": 99,
                        "author_id": 1,
                        "state": "new",
                        "rank": 9,
                    }))],
                )
                .unwrap_err();
            if expected_new {
                assert_eq!(error.code, "RECOVERY_REQUIRED");
                assert_eq!(
                    paged.ensure_readable().unwrap_err().code,
                    "RECOVERY_REQUIRED"
                );
                assert_eq!(
                    paged
                        .replace_table_snapshot(source.table_schema("posts").unwrap(), Vec::new(),)
                        .unwrap_err()
                        .code,
                    "RECOVERY_REQUIRED"
                );
            } else {
                assert_eq!(error.code, "INJECTED_IO");
                assert_eq!(paged.revision(), revision);
                assert_eq!(paged.table_row_count("posts").unwrap(), 3);
            }

            let device = paged.into_device();
            control.crash();
            let reopened = PagedStorage::open(device).unwrap();
            assert_eq!(reopened.revision(), revision + u64::from(expected_new));
            assert_eq!(
                reopened.table_row_count("posts").unwrap(),
                if expected_new { 1 } else { 3 }
            );
            assert_eq!(
                reopened
                    .lookup_primary_key("posts", &row(json!({"id": 99})))
                    .unwrap()
                    .is_some(),
                expected_new
            );
            assert!(reopened.table_schema("authors").is_ok());
            assert!(reopened.index_definition("posts_author").is_some());
            assert!(reopened.index_definition("posts_state_rank").is_some());
        }
    }

    #[test]
    fn interrupted_add_column_reopens_at_the_complete_old_or_new_schema() {
        for (failing_flush, expected_new) in [(1, false), (2, false), (3, true)] {
            let device = DurableDevice::default();
            let control = device.clone();
            let source = source();
            let mut paged = PagedStorage::from_in_memory(device, &source).unwrap();
            let revision = paged.revision();
            let old_table_tree = paged.tables["posts"].tree_id;
            let allocated_tree = paged.next_tree_id;
            let old_next_tree_id = paged.next_tree_id;
            let old_indexes = paged
                .indexes
                .iter()
                .filter(|(_, index)| index.definition.table == "posts")
                .map(|(name, index)| {
                    (
                        name.clone(),
                        (index.tree_id, index.root_page_id, index.entry_count),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            control.arm_after_flush(failing_flush);
            let error = paged
                .add_column_and_advance(
                    "posts",
                    &ColumnDefinition {
                        name: "priority".to_owned(),
                        data_type: ColumnType::Integer,
                        nullable: false,
                        default: Some(json!(3)),
                    },
                )
                .unwrap_err();
            if expected_new {
                assert_eq!(error.code, "RECOVERY_REQUIRED");
                assert_eq!(
                    paged.ensure_readable().unwrap_err().code,
                    "RECOVERY_REQUIRED"
                );
                // Even an existing-name IF NOT EXISTS path must not expose stale metadata after
                // an ambiguous publication.
                assert_eq!(
                    crate::statement::plan_add_column(
                        &paged,
                        "posts",
                        &ColumnDefinition {
                            name: "id".to_owned(),
                            data_type: ColumnType::Json,
                            nullable: true,
                            default: None,
                        },
                        true,
                    )
                    .unwrap_err()
                    .code,
                    "RECOVERY_REQUIRED"
                );
            } else {
                assert_eq!(error.code, "INJECTED_IO");
                assert_eq!(paged.revision(), revision);
                assert!(
                    paged.tables["posts"]
                        .schema
                        .columns
                        .iter()
                        .all(|column| column.name != "priority")
                );
            }

            let device = paged.into_device();
            control.crash();
            let reopened = PagedStorage::open(device).unwrap();
            assert_eq!(reopened.revision(), revision + u64::from(expected_new));
            assert_eq!(
                reopened.tables["posts"].tree_id,
                if expected_new {
                    allocated_tree
                } else {
                    old_table_tree
                }
            );
            assert_eq!(
                reopened.next_tree_id,
                old_next_tree_id + u64::from(expected_new)
            );
            for (name, expected) in &old_indexes {
                let index = &reopened.indexes[name];
                assert_eq!(
                    (index.tree_id, index.root_page_id, index.entry_count),
                    *expected
                );
            }
            let row = reopened
                .lookup_primary_key("posts", &row(json!({"id": 10})))
                .unwrap()
                .unwrap();
            assert_eq!(row.get("priority"), expected_new.then_some(&json!(3)));
        }
    }

    #[test]
    fn corrupt_old_table_after_index_reclaims_aborts_the_complete_replacement() {
        let device = CountingDevice::new();
        let control = device.clone();
        let source = source();
        let schema = source.table_schema("posts").unwrap();
        let imported = PagedStorage::from_in_memory(device, &source).unwrap();
        let table_root = imported.tables["posts"].root_page_id.unwrap();
        let unrelated_root = imported.tables["authors"].root_page_id.unwrap();
        let original = *control.inner.borrow().page(table_root).unwrap();
        let old_index_roots = imported
            .indexes
            .values()
            .filter(|index| index.definition.table == "posts")
            .map(|index| index.root_page_id.unwrap())
            .collect::<Vec<_>>();
        let device = imported.into_device();
        let mut pager = Pager::with_cache_capacity(device, PAGE_SIZE).unwrap();
        let revision = pager.database_revision();
        let catalog_root = pager.catalog_root_page_id().unwrap();
        let (next_tree_id, tables, indexes) =
            load_and_validate_catalog(&mut pager, catalog_root).unwrap();
        // Evict the target table root from the one-page cache before damaging its device bytes.
        pager.read_page(unrelated_root).unwrap();
        let mut paged = PagedStorage {
            pager: RefCell::new(pager),
            revision,
            next_tree_id,
            tables,
            indexes,
            recovery_required: false,
        };
        let mut corrupt = original;
        corrupt[PAGE_SIZE - 1] ^= 1;
        control
            .inner
            .borrow_mut()
            .write_page(table_root, &corrupt)
            .unwrap();

        let error = paged
            .replace_table_snapshot(
                schema,
                vec![row(json!({
                    "id": 99,
                    "author_id": 1,
                    "state": "new",
                    "rank": 9,
                }))],
            )
            .unwrap_err();
        assert_eq!(error.code, "INVALID_PAGE");
        assert_eq!(paged.revision(), revision);
        assert_eq!(paged.table_row_count("posts").unwrap(), 3);
        for root in old_index_roots {
            assert!(
                paged
                    .pager
                    .borrow()
                    .active_metadata()
                    .allocation_bitmap
                    .is_allocated(root)
                    .unwrap()
            );
        }

        control
            .inner
            .borrow_mut()
            .write_page(table_root, &original)
            .unwrap();
        let reopened = PagedStorage::open(paged.into_device()).unwrap();
        assert_eq!(reopened.revision(), revision);
        assert_eq!(reopened.table_row_count("posts").unwrap(), 3);
        assert!(reopened.index_definition("posts_author").is_some());
        assert!(reopened.index_definition("posts_state_rank").is_some());
    }

    #[test]
    fn corrupt_old_table_aborts_add_column_without_publishing_or_freeing() {
        let device = CountingDevice::new();
        let control = device.clone();
        let imported = PagedStorage::from_in_memory(device, &source()).unwrap();
        let table_root = imported.tables["posts"].root_page_id.unwrap();
        let unrelated_root = imported.tables["authors"].root_page_id.unwrap();
        let original = *control.inner.borrow().page(table_root).unwrap();
        let device = imported.into_device();
        let mut pager = Pager::with_cache_capacity(device, PAGE_SIZE).unwrap();
        let revision = pager.database_revision();
        let catalog_root = pager.catalog_root_page_id().unwrap();
        let (next_tree_id, tables, indexes) =
            load_and_validate_catalog(&mut pager, catalog_root).unwrap();
        let old_table = tables["posts"].clone();
        let old_index_roots = indexes
            .values()
            .filter(|index| index.definition.table == "posts")
            .map(|index| index.root_page_id.unwrap())
            .collect::<Vec<_>>();
        // Evict the target root from the one-page cache before damaging the committed bytes.
        pager.read_page(unrelated_root).unwrap();
        let mut paged = PagedStorage {
            pager: RefCell::new(pager),
            revision,
            next_tree_id,
            tables,
            indexes,
            recovery_required: false,
        };
        let mut corrupt = original;
        corrupt[PAGE_SIZE - 1] ^= 1;
        control
            .inner
            .borrow_mut()
            .write_page(table_root, &corrupt)
            .unwrap();

        let error = paged
            .add_column_and_advance(
                "posts",
                &ColumnDefinition {
                    name: "priority".to_owned(),
                    data_type: ColumnType::Integer,
                    nullable: false,
                    default: Some(json!(3)),
                },
            )
            .unwrap_err();
        assert_eq!(error.code, "INVALID_PAGE");
        assert_eq!(paged.revision(), revision);
        assert_eq!(paged.next_tree_id, next_tree_id);
        assert_eq!(paged.tables["posts"].tree_id, old_table.tree_id);
        assert_eq!(paged.tables["posts"].root_page_id, old_table.root_page_id);
        for root in old_index_roots {
            assert!(
                paged
                    .pager
                    .borrow()
                    .active_metadata()
                    .allocation_bitmap
                    .is_allocated(root)
                    .unwrap()
            );
        }

        control
            .inner
            .borrow_mut()
            .write_page(table_root, &original)
            .unwrap();
        let reopened = PagedStorage::open(paged.into_device()).unwrap();
        assert_eq!(reopened.revision(), revision);
        assert_eq!(reopened.tables["posts"].tree_id, old_table.tree_id);
        assert!(
            reopened.tables["posts"]
                .schema
                .columns
                .iter()
                .all(|column| column.name != "priority")
        );
    }

    #[test]
    fn conflicting_oversized_schema_preserves_invalid_schema_error_order() {
        let mut source = InMemoryStorage::default();
        let existing = schema("wide", &[("id", ColumnType::Integer, false)]);
        source.define_table(existing).unwrap();
        let mut conflicting = schema("wide", &[("id", ColumnType::Integer, false)]);
        conflicting
            .columns
            .extend((1..256).map(|number| ColumnDefinition {
                name: format!("column_{number}"),
                data_type: ColumnType::Text,
                nullable: false,
                default: Some(json!("x".repeat(5_000))),
            }));
        let expected = source
            .clone()
            .replace_table_snapshot(conflicting.clone(), Vec::new())
            .unwrap_err();
        assert_eq!(expected.code, "INVALID_SCHEMA");

        let mut paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
        let revision = paged.revision();
        let actual = paged
            .replace_table_snapshot(conflicting, Vec::new())
            .unwrap_err();
        assert_eq!(actual, expected);
        assert_eq!(paged.revision(), revision);
    }

    #[test]
    fn snapshot_metadata_work_is_bounded_before_plan_clones() {
        let mut source = InMemoryStorage::default();
        let mut wide = schema("wide", &[("id", ColumnType::Integer, false)]);
        wide.columns.extend((1..128).map(|number| ColumnDefinition {
            name: format!("column_{number}_{}", "x".repeat(880)),
            data_type: ColumnType::Text,
            nullable: true,
            default: None,
        }));
        source.define_table(wide.clone()).unwrap();
        let indexed_columns = wide
            .columns
            .iter()
            .skip(1)
            .map(|column| column.name.clone())
            .collect::<Vec<_>>();
        for number in 0..24 {
            source
                .define_index(IndexDefinition {
                    name: format!("wide_{number}"),
                    table: "wide".to_owned(),
                    columns: indexed_columns.clone(),
                    unique: false,
                })
                .unwrap();
        }
        let mut paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
        let revision = paged.revision();
        let next_tree_id = paged.next_tree_id;
        let error = paged.replace_table_snapshot(wide, Vec::new()).unwrap_err();
        assert_eq!(error.code, "TRANSACTION_TOO_LARGE");
        assert_eq!(paged.revision(), revision);
        assert_eq!(paged.next_tree_id, next_tree_id);
        assert_eq!(paged.table_row_count("wide").unwrap(), 0);
    }

    #[test]
    fn add_column_bounds_container_defaults_before_catalog_schema_clones() {
        let mut source = InMemoryStorage::default();
        source
            .define_table(schema("items", &[("id", ColumnType::Integer, false)]))
            .unwrap();
        let mut paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
        let revision = paged.revision();
        let old_table = paged.tables["items"].clone();
        let old_next_tree_id = paged.next_tree_id;
        // This is physically encodable (well below the 1 MiB catalog value cap), but its
        // concurrently retained schema, record clone, and serde Value model exceed the 16 MiB
        // transient-work contract. Null-heavy arrays expose container-node accounting that a
        // string-only multiplier would miss.
        let column = ColumnDefinition {
            name: "metadata".to_owned(),
            data_type: ColumnType::Json,
            nullable: false,
            default: Some(Value::Array(vec![Value::Null; 150_000])),
        };
        let error = paged.add_column_and_advance("items", &column).unwrap_err();
        assert_eq!(error.code, "TRANSACTION_TOO_LARGE");
        assert_eq!(paged.revision(), revision);
        assert_eq!(paged.next_tree_id, old_next_tree_id);
        assert_eq!(paged.tables["items"].tree_id, old_table.tree_id);
        assert_eq!(paged.tables["items"].root_page_id, None);
        assert!(
            paged.tables["items"]
                .schema
                .columns
                .iter()
                .all(|column| column.name != "metadata")
        );
    }

    #[test]
    fn destructive_schema_changes_reclaim_pages_and_preserve_the_tree_id_high_water_mark() {
        let mut paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source()).unwrap();
        let next_tree_id = paged.next_tree_id;
        let dropped_roots = [
            paged.indexes["posts_author"].root_page_id.unwrap(),
            paged.indexes["posts_state_rank"].root_page_id.unwrap(),
            paged.tables["posts"].root_page_id.unwrap(),
        ];
        let initial_live_pages = paged
            .pager
            .borrow()
            .active_metadata()
            .superblock
            .live_data_page_count;

        let revision = paged.revision();
        paged.drop_index_and_advance("posts_author").unwrap();
        assert_eq!(paged.revision(), revision + 1);
        assert_eq!(paged.next_tree_id, next_tree_id);
        assert!(paged.index_definition("posts_author").is_none());
        let after_index_pages = paged
            .pager
            .borrow()
            .active_metadata()
            .superblock
            .live_data_page_count;
        assert!(after_index_pages < initial_live_pages);

        paged.drop_table_and_advance("posts").unwrap();
        assert_eq!(paged.revision(), revision + 2);
        assert_eq!(paged.next_tree_id, next_tree_id);
        assert_eq!(
            paged.table_schema("posts").unwrap_err().code,
            "TABLE_NOT_FOUND"
        );
        assert!(paged.index_definition("posts_state_rank").is_none());
        assert!(paged.table_schema("authors").is_ok());
        let after_table_pages = paged
            .pager
            .borrow()
            .active_metadata()
            .superblock
            .live_data_page_count;
        assert!(after_table_pages < after_index_pages);

        // The next object uses the monotonic high-water mark, while its pages reuse physical
        // slots reclaimed by the dropped table and indexes.
        paged
            .define_table(schema(
                "replacement",
                &[
                    ("id", ColumnType::Integer, false),
                    ("value", ColumnType::Text, false),
                ],
            ))
            .unwrap();
        assert_eq!(paged.tables["replacement"].tree_id, next_tree_id);
        paged
            .apply_batch(&ChangeBatch {
                changes: vec![Change::Upsert {
                    table: "replacement".to_owned(),
                    row: row(json!({"id": 1, "value": "reused"})),
                }],
                ..ChangeBatch::default()
            })
            .unwrap();
        assert!(dropped_roots.iter().any(|root| {
            paged
                .pager
                .borrow()
                .active_metadata()
                .allocation_bitmap
                .is_allocated(*root)
                .unwrap()
        }));

        let reopened = PagedStorage::open(paged.into_device()).unwrap();
        assert_eq!(reopened.next_tree_id, next_tree_id + 1);
        assert_eq!(
            reopened.table_schema("posts").unwrap_err().code,
            "TABLE_NOT_FOUND"
        );
        assert_eq!(
            reopened
                .lookup_primary_key("replacement", &row(json!({"id": 1})))
                .unwrap()
                .unwrap()["value"],
            json!("reused")
        );
    }

    #[test]
    fn interrupted_schema_drops_reopen_at_the_complete_old_or_new_catalog() {
        for drop_table in [false, true] {
            for (failing_flush, expected_new) in [(1, false), (2, false), (3, true)] {
                let device = DurableDevice::default();
                let control = device.clone();
                let mut paged = PagedStorage::from_in_memory(device, &source()).unwrap();
                let revision = paged.revision();
                control.arm_after_flush(failing_flush);
                let error = if drop_table {
                    paged.drop_table_and_advance("posts").unwrap_err()
                } else {
                    paged.drop_index_and_advance("posts_author").unwrap_err()
                };
                if expected_new {
                    assert_eq!(error.code, "RECOVERY_REQUIRED");
                    assert_eq!(
                        paged.ensure_readable().unwrap_err().code,
                        "RECOVERY_REQUIRED"
                    );
                    assert_eq!(
                        crate::statement::plan_drop_index(&paged, "missing", true)
                            .unwrap_err()
                            .code,
                        "RECOVERY_REQUIRED"
                    );
                    assert_eq!(
                        crate::statement::plan_drop_table(&paged, "missing", true)
                            .unwrap_err()
                            .code,
                        "RECOVERY_REQUIRED"
                    );
                } else {
                    assert_eq!(error.code, "INJECTED_IO");
                    assert_eq!(paged.revision(), revision);
                    assert!(paged.table_schema("posts").is_ok());
                    assert!(paged.index_definition("posts_author").is_some());
                }

                let device = paged.into_device();
                control.crash();
                let reopened = PagedStorage::open(device).unwrap();
                assert_eq!(reopened.revision(), revision + u64::from(expected_new));
                if drop_table {
                    assert_eq!(reopened.table_schema("posts").is_err(), expected_new);
                    assert_eq!(
                        reopened.index_definition("posts_author").is_none(),
                        expected_new
                    );
                    assert_eq!(
                        reopened.index_definition("posts_state_rank").is_none(),
                        expected_new
                    );
                } else {
                    assert!(reopened.table_schema("posts").is_ok());
                    assert_eq!(
                        reopened.index_definition("posts_author").is_none(),
                        expected_new
                    );
                    assert!(reopened.index_definition("posts_state_rank").is_some());
                }
            }
        }
    }

    #[test]
    fn corrupt_dropped_tree_aborts_before_catalog_publication() {
        let device = CountingDevice::new();
        let control = device.clone();
        let imported = PagedStorage::from_in_memory(device, &source()).unwrap();
        let authors_root = imported.tables["authors"].root_page_id.unwrap();
        let posts_root = imported.tables["posts"].root_page_id.unwrap();
        let original = *control.inner.borrow().page(authors_root).unwrap();
        let device = imported.into_device();
        let mut pager = Pager::with_cache_capacity(device, PAGE_SIZE).unwrap();
        let revision = pager.database_revision();
        let catalog_root = pager.catalog_root_page_id().unwrap();
        let (next_tree_id, tables, indexes) =
            load_and_validate_catalog(&mut pager, catalog_root).unwrap();
        // Ensure the target root is not resident in the one-page cache when its device bytes are
        // corrupted after opening.
        pager.read_page(posts_root).unwrap();
        let mut paged = PagedStorage {
            pager: RefCell::new(pager),
            revision,
            next_tree_id,
            tables,
            indexes,
            recovery_required: false,
        };
        let mut corrupt = original;
        corrupt[PAGE_SIZE - 1] ^= 1;
        control
            .inner
            .borrow_mut()
            .write_page(authors_root, &corrupt)
            .unwrap();

        let error = paged.drop_table_and_advance("authors").unwrap_err();
        assert_eq!(error.code, "INVALID_PAGE");
        assert_eq!(paged.revision(), revision);
        assert!(paged.table_schema("authors").is_ok());
        assert!(paged.table_schema("posts").is_ok());

        // Restore the deliberately damaged committed page and reopen. The failed candidate did
        // not publish either its frees or its catalog deletion.
        control
            .inner
            .borrow_mut()
            .write_page(authors_root, &original)
            .unwrap();
        let reopened = PagedStorage::open(paged.into_device()).unwrap();
        assert_eq!(reopened.revision(), revision);
        assert!(reopened.table_schema("authors").is_ok());
        assert!(reopened.table_schema("posts").is_ok());
    }

    #[test]
    fn add_column_rewrites_only_the_table_and_preserves_every_index_tree() {
        let source = source();
        let mut paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
        let revision = paged.revision();
        let old_table = paged.tables["posts"].clone();
        let old_next_tree_id = paged.next_tree_id;
        let old_indexes = paged
            .indexes
            .iter()
            .filter(|(_, index)| index.definition.table == "posts")
            .map(|(name, index)| {
                (
                    name.clone(),
                    (index.tree_id, index.root_page_id, index.entry_count),
                )
            })
            .collect::<BTreeMap<_, _>>();

        assert_eq!(
            paged
                .add_column_and_advance(
                    "posts",
                    &ColumnDefinition {
                        name: "priority".to_owned(),
                        data_type: ColumnType::Integer,
                        nullable: false,
                        default: Some(json!(3)),
                    },
                )
                .unwrap(),
            revision + 1
        );
        let table = &paged.tables["posts"];
        assert_eq!(table.tree_id, old_next_tree_id);
        assert_ne!(table.root_page_id, old_table.root_page_id);
        assert_eq!(table.row_count, old_table.row_count);
        assert_eq!(paged.next_tree_id, old_next_tree_id + 1);
        for (name, expected) in &old_indexes {
            let index = &paged.indexes[name];
            assert_eq!(
                (index.tree_id, index.root_page_id, index.entry_count),
                *expected
            );
        }
        assert!(
            !paged
                .pager
                .borrow()
                .active_metadata()
                .allocation_bitmap
                .is_allocated(old_table.root_page_id.unwrap())
                .unwrap()
        );

        let mut indexed_rows = Vec::new();
        assert_eq!(
            paged
                .visit_index(
                    "posts",
                    &["author_id".to_owned()],
                    &row(json!({"author_id": 1})),
                    &mut |row| {
                        indexed_rows.push(row.clone());
                        Ok(VisitControl::Continue)
                    },
                )
                .unwrap(),
            Some(VisitOutcome::Complete)
        );
        indexed_rows.sort_by_key(|row| row["id"].as_i64().unwrap());
        assert_eq!(indexed_rows.len(), 2);
        assert!(indexed_rows.iter().all(|row| row["priority"] == 3));

        let reopened = PagedStorage::open(paged.into_device()).unwrap();
        assert_eq!(reopened.revision(), revision + 1);
        assert_eq!(reopened.next_tree_id, old_next_tree_id + 1);
        assert_eq!(reopened.tables["posts"].tree_id, old_next_tree_id);
        for (name, expected) in old_indexes {
            let index = &reopened.indexes[&name];
            assert_eq!(
                (index.tree_id, index.root_page_id, index.entry_count),
                expected
            );
        }
        assert_eq!(
            reopened
                .lookup_primary_key("posts", &row(json!({"id": 12})))
                .unwrap()
                .unwrap()["priority"],
            3
        );
    }

    #[test]
    fn empty_add_column_is_metadata_only_even_at_the_tree_id_ceiling() {
        let mut source = InMemoryStorage::default();
        source
            .define_table(schema("empty", &[("id", ColumnType::Integer, false)]))
            .unwrap();
        let mut paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
        let revision = paged.revision();
        let old_table = paged.tables["empty"].clone();
        paged.next_tree_id = MAX_TREE_ID;
        paged
            .add_column_and_advance(
                "empty",
                &ColumnDefinition {
                    name: "required".to_owned(),
                    data_type: ColumnType::Text,
                    nullable: false,
                    default: None,
                },
            )
            .unwrap();
        assert_eq!(paged.revision(), revision + 1);
        assert_eq!(paged.next_tree_id, MAX_TREE_ID);
        assert_eq!(paged.tables["empty"].tree_id, old_table.tree_id);
        assert_eq!(paged.tables["empty"].root_page_id, None);
        assert_eq!(paged.tables["empty"].row_count, 0);

        let error = paged
            .apply_batch(&ChangeBatch {
                changes: vec![Change::Upsert {
                    table: "empty".to_owned(),
                    row: row(json!({"id": 1})),
                }],
                ..ChangeBatch::default()
            })
            .unwrap_err();
        assert_eq!(error.code, "CONSTRAINT_VIOLATION");
        assert_eq!(paged.revision(), revision + 1);

        let reopened = PagedStorage::open(paged.into_device()).unwrap();
        assert_eq!(reopened.next_tree_id, MAX_TREE_ID);
        assert_eq!(reopened.tables["empty"].tree_id, old_table.tree_id);
        assert!(
            reopened.tables["empty"]
                .schema
                .columns
                .iter()
                .any(|column| column.name == "required")
        );
    }

    #[test]
    fn rooted_add_column_requires_one_representable_fresh_tree_id() {
        let mut paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source()).unwrap();
        let revision = paged.revision();
        let old_table = paged.tables["posts"].clone();
        paged.next_tree_id = MAX_TREE_ID;
        let constraint = paged
            .add_column_and_advance(
                "posts",
                &ColumnDefinition {
                    name: "required".to_owned(),
                    data_type: ColumnType::Text,
                    nullable: false,
                    default: None,
                },
            )
            .unwrap_err();
        assert_eq!(constraint.code, "CONSTRAINT_VIOLATION");
        let error = paged
            .add_column_and_advance(
                "posts",
                &ColumnDefinition {
                    name: "priority".to_owned(),
                    data_type: ColumnType::Integer,
                    nullable: false,
                    default: Some(json!(3)),
                },
            )
            .unwrap_err();
        assert_eq!(error.code, "STORAGE_LIMIT");
        assert_eq!(paged.revision(), revision);
        assert_eq!(paged.tables["posts"].tree_id, old_table.tree_id);
        assert_eq!(paged.tables["posts"].root_page_id, old_table.root_page_id);
    }

    #[test]
    fn add_column_revision_exhaustion_precedes_row_rewrite_and_page_work() {
        let schema = schema(
            "large",
            &[
                ("id", ColumnType::Integer, false),
                ("payload", ColumnType::Text, false),
            ],
        );
        let empty = row(json!({"id": 1, "payload": ""}));
        let overhead = serde_json::to_vec(&empty).unwrap().len();
        let payload_bytes = crate::storage::MAX_LOGICAL_ROW_BYTES - overhead;
        let existing = row(json!({"id": 1, "payload": "x".repeat(payload_bytes)}));
        let mut source = InMemoryStorage::default();
        source.define_table(schema).unwrap();
        source.replace_table("large", vec![existing]).unwrap();
        let column = ColumnDefinition {
            name: "extra".to_owned(),
            data_type: ColumnType::Text,
            nullable: false,
            default: Some(json!("y")),
        };
        let mut expected = source.clone();
        let expected_error = expected.add_column("large", column.clone()).unwrap_err();
        assert_eq!(expected_error.code, "INVALID_CHANGE");
        source
            .set_revision(crate::revision::MAX_DATABASE_REVISION)
            .unwrap();

        let device = CountingDevice::new();
        let control = device.clone();
        let mut paged = PagedStorage::from_in_memory(device, &source).unwrap();
        let old_tree = paged.tables["large"].clone();
        let old_next_tree_id = paged.next_tree_id;
        let page_count = control.page_count();
        let flush_count = control.inner.borrow().flush_count();
        let actual = paged.add_column_and_advance("large", &column).unwrap_err();
        assert_eq!(actual.code, "REVISION_OVERFLOW");
        assert_eq!(paged.revision(), crate::revision::MAX_DATABASE_REVISION);
        assert_eq!(paged.next_tree_id, old_next_tree_id);
        assert_eq!(paged.tables["large"].tree_id, old_tree.tree_id);
        assert_eq!(paged.tables["large"].root_page_id, old_tree.root_page_id);
        assert_eq!(control.page_count(), page_count);
        assert_eq!(control.inner.borrow().flush_count(), flush_count);
        let reopened = PagedStorage::open(paged.into_device()).unwrap();
        assert_eq!(reopened.revision(), crate::revision::MAX_DATABASE_REVISION);
    }

    #[test]
    fn add_column_operation_budget_counts_stream_rewrite_and_fixed_publication() {
        assert_eq!(add_column_operation_count(0, false).unwrap(), 2);
        assert_eq!(add_column_operation_count(499_997, true).unwrap(), 999_999);
        preflight_add_column_operations(499_997, true).unwrap();
        assert_eq!(
            add_column_operation_count(499_998, true).unwrap(),
            1_000_001
        );
        assert_eq!(
            preflight_add_column_operations(499_998, true)
                .unwrap_err()
                .code,
            "TRANSACTION_TOO_LARGE"
        );
    }

    #[test]
    fn repeated_add_column_reuses_reclaimed_pages_without_a_live_page_leak() {
        let mut paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source()).unwrap();
        let initial_revision = paged.revision();
        let initial_next_tree_id = paged.next_tree_id;
        let mut physical_pages_after_first = None;
        for number in 0..8 {
            let old_root = paged.tables["posts"].root_page_id.unwrap();
            paged
                .add_column_and_advance(
                    "posts",
                    &ColumnDefinition {
                        name: format!("added_{number}"),
                        data_type: ColumnType::Boolean,
                        nullable: false,
                        default: Some(json!(false)),
                    },
                )
                .unwrap();
            assert!(
                !paged
                    .pager
                    .borrow()
                    .active_metadata()
                    .allocation_bitmap
                    .is_allocated(old_root)
                    .unwrap()
            );
            let page_count = paged.pager.borrow().physical_page_count();
            if let Some(first) = physical_pages_after_first {
                assert!(page_count <= first + 2);
            } else {
                physical_pages_after_first = Some(page_count);
            }
        }
        assert_eq!(paged.revision(), initial_revision + 8);
        assert_eq!(paged.next_tree_id, initial_next_tree_id + 8);
        assert_eq!(paged.tables["posts"].row_count, 3);
        let reopened = PagedStorage::open(paged.into_device()).unwrap();
        assert_eq!(reopened.revision(), initial_revision + 8);
        assert_eq!(reopened.tables["posts"].schema.columns.len(), 12);
    }

    #[test]
    fn gapped_catalog_retains_its_monotonic_tree_id_allocator() {
        let mut source = InMemoryStorage::default();
        for name in ["alpha", "beta", "gamma"] {
            source
                .define_table(schema(name, &[("id", ColumnType::Integer, false)]))
                .unwrap();
        }
        let paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
        let device = paged.into_device();
        let mut pager = Pager::open_or_create(device).unwrap();
        let catalog_root = pager.catalog_root_page_id().unwrap();
        let revision = pager.database_revision();

        // Simulate a future DROP by removing one catalog record while leaving next_tree_id at its
        // monotonic high-water mark. The dropped tree itself is empty in this fixture.
        let (_, tables, indexes) = load_and_validate_catalog(&mut pager, catalog_root).unwrap();
        let dropped = tables["beta"].tree_id;
        let previous_next_tree_id = FIRST_USER_TREE_ID + (tables.len() + indexes.len()) as u64;
        let (table_key, _) = encode_catalog_table_record(&CatalogTableRecord {
            schema: tables["beta"].schema.clone(),
            tree_id: dropped,
            root_page_id: tables["beta"].root_page_id,
            row_count: tables["beta"].row_count as u64,
        })
        .unwrap();
        let mut transaction = pager.begin_write().unwrap();
        let (catalog_root, removed) =
            Btree::delete(&mut transaction, catalog_root, CATALOG_TREE_ID, &table_key).unwrap();
        assert!(removed);
        let mut catalog_root = catalog_root.unwrap();
        let (header_key, header_value) = encode_catalog_header_record(&CatalogHeader {
            next_tree_id: previous_next_tree_id,
            table_count: (tables.len() - 1) as u32,
            index_count: indexes.len() as u32,
        })
        .unwrap();
        catalog_root = Btree::upsert(
            &mut transaction,
            catalog_root,
            CATALOG_TREE_ID,
            &header_key,
            &header_value,
        )
        .unwrap();
        transaction.commit(revision, 0, Some(catalog_root)).unwrap();

        let mut reopened = PagedStorage::open(pager.into_device()).unwrap();
        assert_eq!(reopened.next_tree_id, previous_next_tree_id);
        reopened
            .define_table(schema("replacement", &[("id", ColumnType::Integer, false)]))
            .unwrap();
        assert_eq!(
            reopened.tables["replacement"].tree_id,
            previous_next_tree_id
        );
        assert_ne!(reopened.tables["replacement"].tree_id, dropped);
        assert_eq!(reopened.next_tree_id, previous_next_tree_id + 1);

        let reopened = PagedStorage::open(reopened.into_device()).unwrap();
        assert_eq!(
            reopened.tables["replacement"].tree_id,
            previous_next_tree_id
        );
        assert_eq!(reopened.next_tree_id, previous_next_tree_id + 1);
    }

    fn query_pair(source: &InMemoryStorage, sql: &str) -> (QueryResult, QueryResult) {
        let expected = Engine::new(source.clone()).query_sql(sql, &[]).unwrap();
        let paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), source).unwrap();
        let actual = Engine::new(paged).query_sql(sql, &[]).unwrap();
        (expected, actual)
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
            assert_eq!(actual, expected, "{sql}");
        }

        let paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
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
        let paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source()).unwrap();
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
                        ids.push(row["id"].as_i64().unwrap());
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
        let paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source()).unwrap();
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
    fn page_native_batch_is_atomic_indexed_revisioned_and_reopenable() {
        let source = source();
        let mut paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
        let before_revision = paged.revision();
        let outcome = paged
            .apply_batch(&ChangeBatch {
                changes: vec![
                    Change::Upsert {
                        table: "posts".to_owned(),
                        row: row(json!({"id": 11, "author_id": 2, "state": "archived", "rank": 7})),
                    },
                    Change::Delete {
                        table: "posts".to_owned(),
                        key: row(json!({"id": 10})),
                    },
                    Change::Upsert {
                        table: "posts".to_owned(),
                        row: row(json!({"id": 13, "author_id": 1, "state": "live", "rank": 4})),
                    },
                    Change::Upsert {
                        table: "authors".to_owned(),
                        row: row(json!({"id": 1, "name": "Ada Lovelace"})),
                    },
                    Change::Upsert {
                        table: "posts".to_owned(),
                        row: row(json!({"id": 14, "author_id": 1, "state": "draft", "rank": 8})),
                    },
                    Change::Delete {
                        table: "posts".to_owned(),
                        key: row(json!({"id": 14})),
                    },
                ],
                ..ChangeBatch::default()
            })
            .unwrap();
        assert_eq!(outcome.revision, before_revision + 1);
        assert_eq!(outcome.tables, vec!["authors", "posts"]);
        assert_eq!(paged.table_row_count("posts").unwrap(), 3);
        assert_eq!(
            paged.scan_table("posts").unwrap(),
            vec![
                row(json!({"id": 11, "author_id": 2, "state": "archived", "rank": 7})),
                row(json!({"id": 12, "author_id": 2, "state": null, "rank": 3})),
                row(json!({"id": 13, "author_id": 1, "state": "live", "rank": 4})),
            ]
        );
        let mut ids = Vec::new();
        paged
            .visit_index(
                "posts",
                &["author_id".to_owned()],
                &row(json!({"author_id": 2})),
                &mut |row| {
                    ids.push(row["id"].as_i64().unwrap());
                    Ok(VisitControl::Continue)
                },
            )
            .unwrap();
        assert_eq!(ids, vec![11, 12]);

        let mut reopened = PagedStorage::open(paged.into_device()).unwrap();
        assert_eq!(reopened.revision(), before_revision + 1);
        assert_eq!(
            reopened
                .lookup_primary_key("authors", &row(json!({"id": 1})))
                .unwrap()
                .unwrap()["name"],
            "Ada Lovelace"
        );
        let no_op = reopened
            .apply_batch(&ChangeBatch {
                changes: vec![],
                ..ChangeBatch::default()
            })
            .unwrap();
        assert_eq!(no_op.revision, before_revision + 1);
        assert!(no_op.tables.is_empty());
        let syntactic_change = reopened
            .apply_batch(&ChangeBatch {
                changes: vec![Change::Delete {
                    table: "posts".to_owned(),
                    key: row(json!({"id": 999})),
                }],
                ..ChangeBatch::default()
            })
            .unwrap();
        assert_eq!(syntactic_change.revision, before_revision + 2);
        assert_eq!(syntactic_change.tables, vec!["posts"]);
    }

    #[test]
    fn unique_indexes_validate_complete_final_composite_state_and_allow_nulls_and_swaps() {
        let mut source = InMemoryStorage::default();
        source
            .define_table(schema(
                "accounts",
                &[
                    ("id", ColumnType::Integer, false),
                    ("tenant", ColumnType::Integer, false),
                    ("email", ColumnType::Text, true),
                ],
            ))
            .unwrap();
        source
            .replace_table(
                "accounts",
                vec![
                    row(json!({"id": 1, "tenant": 7, "email": "a@example.com"})),
                    row(json!({"id": 2, "tenant": 7, "email": "b@example.com"})),
                    row(json!({"id": 3, "tenant": 7, "email": null})),
                    row(json!({"id": 4, "tenant": 7, "email": null})),
                ],
            )
            .unwrap();
        source
            .define_index(IndexDefinition {
                name: "accounts_tenant_email".to_owned(),
                table: "accounts".to_owned(),
                columns: vec!["tenant".to_owned(), "email".to_owned()],
                unique: true,
            })
            .unwrap();
        let mut paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
        paged
            .apply_batch(&ChangeBatch {
                changes: vec![
                    Change::Upsert {
                        table: "accounts".to_owned(),
                        row: row(json!({"id": 1, "tenant": 7, "email": "b@example.com"})),
                    },
                    Change::Upsert {
                        table: "accounts".to_owned(),
                        row: row(json!({"id": 2, "tenant": 7, "email": "a@example.com"})),
                    },
                ],
                ..ChangeBatch::default()
            })
            .unwrap();
        let revision = paged.revision();
        let error = paged
            .apply_batch(&ChangeBatch {
                changes: vec![
                    Change::Upsert {
                        table: "accounts".to_owned(),
                        row: row(json!({"id": 3, "tenant": 7, "email": "same@example.com"})),
                    },
                    Change::Upsert {
                        table: "accounts".to_owned(),
                        row: row(json!({"id": 4, "tenant": 7, "email": "same@example.com"})),
                    },
                ],
                ..ChangeBatch::default()
            })
            .unwrap_err();
        assert_eq!(error.code, "CONSTRAINT_VIOLATION");
        assert_eq!(paged.revision(), revision);
        assert_eq!(
            paged
                .lookup_primary_key("accounts", &row(json!({"id": 3})))
                .unwrap()
                .unwrap()["email"],
            Value::Null
        );
    }

    #[test]
    fn invalid_late_table_keeps_every_table_and_revision_unchanged() {
        let mut paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source()).unwrap();
        let revision = paged.revision();
        let before = paged
            .lookup_primary_key("authors", &row(json!({"id": 1})))
            .unwrap();
        let error = paged
            .apply_batch(&ChangeBatch {
                changes: vec![
                    Change::Upsert {
                        table: "authors".to_owned(),
                        row: row(json!({"id": 1, "name": "Changed"})),
                    },
                    Change::Delete {
                        table: "missing".to_owned(),
                        key: row(json!({"id": 1})),
                    },
                ],
                ..ChangeBatch::default()
            })
            .unwrap_err();
        assert_eq!(error.code, "TABLE_NOT_FOUND");
        assert_eq!(paged.revision(), revision);
        assert_eq!(
            paged
                .lookup_primary_key("authors", &row(json!({"id": 1})))
                .unwrap(),
            before
        );
    }

    #[test]
    fn one_row_batch_uses_exact_lookup_instead_of_scanning_the_table() {
        let mut source = InMemoryStorage::default();
        source
            .define_table(schema(
                "large",
                &[
                    ("id", ColumnType::Integer, false),
                    ("payload", ColumnType::Text, false),
                ],
            ))
            .unwrap();
        source
            .replace_table(
                "large",
                (0..2_000)
                    .map(|id| row(json!({"id": id, "payload": "x".repeat(700)})))
                    .collect(),
            )
            .unwrap();

        let device = CountingDevice::new();
        let control = device.clone();
        let imported = PagedStorage::from_in_memory(device, &source).unwrap();
        let device = imported.into_device();
        let mut pager = Pager::with_cache_capacity(device, PAGE_SIZE).unwrap();
        let revision = pager.database_revision();
        let catalog_root = pager.catalog_root_page_id().unwrap();
        let (next_tree_id, tables, indexes) =
            load_and_validate_catalog(&mut pager, catalog_root).unwrap();
        let mut paged = PagedStorage {
            pager: RefCell::new(pager),
            revision,
            next_tree_id,
            tables,
            indexes,
            recovery_required: false,
        };
        control.reset_reads();

        paged
            .apply_batch(&ChangeBatch {
                changes: vec![Change::Upsert {
                    table: "large".to_owned(),
                    row: row(json!({"id": 1_000, "payload": "updated"})),
                }],
                ..ChangeBatch::default()
            })
            .unwrap();
        assert!(
            control.reads() < 30,
            "one exact mutation read {} pages from a multi-hundred-page table",
            control.reads()
        );
    }

    #[test]
    fn interrupted_publication_reopens_at_exactly_the_old_or_new_generation() {
        for (failing_flush, expected_new) in [(1, false), (3, true)] {
            let device = DurableDevice::default();
            let control = device.clone();
            let mut paged = PagedStorage::from_in_memory(device, &source()).unwrap();
            let revision = paged.revision();
            control.arm_after_flush(failing_flush);
            let error = paged
                .apply_batch(&ChangeBatch {
                    changes: vec![Change::Upsert {
                        table: "authors".to_owned(),
                        row: row(json!({"id": 1, "name": "Grace"})),
                    }],
                    ..ChangeBatch::default()
                })
                .unwrap_err();
            if expected_new {
                assert_eq!(error.code, "RECOVERY_REQUIRED");
                for result in [
                    paged.table_row_count("authors").map(|_| ()),
                    paged.table_schema("authors").map(|_| ()),
                    paged
                        .lookup_primary_key("authors", &row(json!({"id": 1})))
                        .map(|_| ()),
                    paged.indexes_for_table("posts").map(|_| ()),
                    paged
                        .visit_table("authors", &mut |_| Ok(VisitControl::Continue))
                        .map(|_| ()),
                    paged
                        .visit_index(
                            "posts",
                            &["author_id".to_owned()],
                            &row(json!({"author_id": 1})),
                            &mut |_| Ok(VisitControl::Continue),
                        )
                        .map(|_| ()),
                ] {
                    assert_eq!(result.unwrap_err().code, "RECOVERY_REQUIRED");
                }
                assert_eq!(
                    paged
                        .apply_batch(&ChangeBatch {
                            changes: vec![Change::Delete {
                                table: "authors".to_owned(),
                                key: row(json!({"id": 2})),
                            }],
                            ..ChangeBatch::default()
                        })
                        .unwrap_err()
                        .code,
                    "RECOVERY_REQUIRED"
                );
            } else {
                assert_eq!(error.code, "INJECTED_IO");
                assert_eq!(paged.revision(), revision);
                assert_eq!(
                    paged
                        .lookup_primary_key("authors", &row(json!({"id": 1})))
                        .unwrap()
                        .unwrap()["name"],
                    "Ada"
                );
            }

            let device = paged.into_device();
            control.crash();
            let reopened = PagedStorage::open(device).unwrap();
            assert_eq!(
                reopened
                    .lookup_primary_key("authors", &row(json!({"id": 1})))
                    .unwrap()
                    .unwrap()["name"],
                if expected_new { "Grace" } else { "Ada" }
            );
            assert_eq!(reopened.revision(), revision + u64::from(expected_new));
        }
    }

    #[test]
    fn page_native_batch_enforces_operation_and_memory_limits_before_mutation() {
        let mut paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source()).unwrap();
        let revision = paged.revision();
        let operation_error = paged
            .apply_batch(&ChangeBatch {
                changes: (0..=crate::storage::MAX_BATCH_CHANGES)
                    .map(|id| Change::Delete {
                        table: "posts".to_owned(),
                        key: row(json!({"id": id})),
                    })
                    .collect(),
                ..ChangeBatch::default()
            })
            .unwrap_err();
        assert_eq!(operation_error.code, "TRANSACTION_TOO_LARGE");
        assert_eq!(paged.revision(), revision);

        let memory_error = paged
            .apply_batch(&ChangeBatch {
                changes: (0..20_000)
                    .map(|id| Change::Upsert {
                        table: "posts".to_owned(),
                        row: row(json!({
                            "id": 99 + id,
                            "author_id": 1,
                            "state": "z".repeat(800),
                            "rank": 1,
                        })),
                    })
                    .collect(),
                ..ChangeBatch::default()
            })
            .unwrap_err();
        assert_eq!(memory_error.code, "TRANSACTION_TOO_LARGE");
        assert_eq!(paged.revision(), revision);
    }

    #[test]
    fn operation_budget_counts_index_delete_and_upsert_plus_catalog_publication() {
        let table = PagedTable {
            schema: schema(
                "indexed",
                &[
                    ("id", ColumnType::Integer, false),
                    ("value", ColumnType::Text, false),
                ],
            ),
            tree_id: FIRST_USER_TREE_ID,
            root_page_id: None,
            row_count: 0,
        };
        let tables = BTreeMap::from([("indexed".to_owned(), table)]);
        let indexes = (0..9)
            .map(|number| {
                let name = format!("index_{number}");
                (
                    name.clone(),
                    PagedIndex {
                        definition: IndexDefinition {
                            name,
                            table: "indexed".to_owned(),
                            columns: vec!["value".to_owned()],
                            unique: false,
                        },
                        tree_id: FIRST_USER_TREE_ID + 1 + number,
                        root_page_id: None,
                        entry_count: 0,
                    },
                )
            })
            .collect();
        let at_limit = (MAX_PAGED_BATCH_OPERATIONS - 10) / 19;
        assert_eq!(at_limit, 52_631);
        let changes = (0..=at_limit)
            .map(|id| Change::Delete {
                table: "indexed".to_owned(),
                key: row(json!({"id": id})),
            })
            .collect::<Vec<_>>();
        preflight_batch(
            &changes[..at_limit],
            &tables,
            &indexes,
            preflight_change_batch,
        )
        .unwrap();
        let error =
            preflight_batch(&changes, &tables, &indexes, preflight_change_batch).unwrap_err();
        assert_eq!(error.code, "TRANSACTION_TOO_LARGE");
    }

    #[test]
    fn snapshot_operation_budget_counts_unique_probes_tree_setup_and_reclaims() {
        // One row with two indexes (one unique): two table operations, six logical/candidate index
        // operations, two logical/candidate unique probes, one table-root create, two cursor
        // seeks, two index-root creates, four catalog writes, and three old-tree reclaim calls.
        assert_eq!(snapshot_operation_count(1, 2, 1, true, false).unwrap(), 22);
        assert_eq!(snapshot_operation_count(0, 2, 1, true, false).unwrap(), 7);
        assert_eq!(snapshot_operation_count(1, 0, 0, false, true).unwrap(), 6);

        // With no indexes an existing replacement costs two operations per row plus the table
        // create, two catalog writes, and old-table reclaim. Exercise the exact limit without
        // allocating or materializing half a million rows.
        let at_limit = (MAX_PAGED_BATCH_OPERATIONS - 4) / 2;
        assert_eq!(
            snapshot_operation_count(at_limit, 0, 0, true, false).unwrap(),
            MAX_PAGED_BATCH_OPERATIONS
        );
        preflight_snapshot_operations(at_limit, 0, 0, true, false).unwrap();
        assert_eq!(
            preflight_snapshot_operations(at_limit + 1, 0, 0, true, false)
                .unwrap_err()
                .code,
            "TRANSACTION_TOO_LARGE"
        );
    }

    #[test]
    fn snapshot_row_order_slot_budget_is_checked_before_exact_capacity_allocation() {
        let rows = MAX_PAGED_BATCH_BYTES
            .div_ceil(std::mem::size_of::<(String, usize)>())
            .checked_add(1)
            .unwrap();
        let slots = rows
            .checked_mul(std::mem::size_of::<(String, usize)>())
            .unwrap();
        assert!(slots > MAX_PAGED_BATCH_BYTES);
        assert_eq!(
            ensure_batch_bytes(slots).unwrap_err().code,
            "TRANSACTION_TOO_LARGE"
        );
    }

    #[test]
    fn snapshot_tree_id_preflight_reserves_a_representable_high_water_mark() {
        assert_eq!(
            replacement_next_tree_id(MAX_TREE_ID - 1, 0).unwrap(),
            MAX_TREE_ID
        );
        assert_eq!(
            replacement_next_tree_id(MAX_TREE_ID, 0).unwrap_err().code,
            "STORAGE_LIMIT"
        );
        assert_eq!(
            replacement_next_tree_id(MAX_TREE_ID - 1, 1)
                .unwrap_err()
                .code,
            "STORAGE_LIMIT"
        );
    }

    #[test]
    fn typed_delete_keys_fail_before_publication_on_type_or_shape_errors() {
        let mut paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source()).unwrap();
        let revision = paged.revision();
        let wrong_type = paged
            .apply_batch(&ChangeBatch {
                changes: vec![Change::Delete {
                    table: "posts".to_owned(),
                    key: row(json!({"id": "11"})),
                }],
                ..ChangeBatch::default()
            })
            .unwrap_err();
        assert_eq!(wrong_type.code, "TYPE_MISMATCH");
        assert_eq!(paged.revision(), revision);
        let missing = paged
            .apply_batch(&ChangeBatch {
                changes: vec![Change::Delete {
                    table: "posts".to_owned(),
                    key: Row::new(),
                }],
                ..ChangeBatch::default()
            })
            .unwrap_err();
        assert_eq!(missing.code, "INVALID_CHANGE");
        assert_eq!(paged.revision(), revision);
    }

    #[test]
    fn validators_reject_nonzero_counts_without_tree_roots() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let table = CatalogTableRecord {
            schema: schema("missing_rows", &[("id", ColumnType::Integer, false)]),
            tree_id: FIRST_USER_TREE_ID,
            root_page_id: None,
            row_count: 1,
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
        let mut source = InMemoryStorage::default();
        source
            .define_table(schema(
                "empty",
                &[
                    ("id", ColumnType::Integer, false),
                    ("label", ColumnType::Text, true),
                ],
            ))
            .unwrap();
        source
            .define_index(IndexDefinition {
                name: "empty_label".to_owned(),
                table: "empty".to_owned(),
                columns: vec!["label".to_owned()],
                unique: false,
            })
            .unwrap();
        let paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
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
        let source = source();
        let paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
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
        let root = Btree::upsert(&mut transaction, root, CATALOG_TREE_ID, &key, &value).unwrap();
        transaction.commit(revision, 0, Some(root)).unwrap();
        let error = match PagedStorage::open(pager.into_device()) {
            Ok(_) => panic!("a mismatched catalog count must fail closed"),
            Err(error) => error,
        };
        assert_eq!(error.code, "STORAGE_CORRUPT");
    }

    #[test]
    fn rootless_live_pages_are_neither_an_empty_database_nor_an_import_target() {
        let mut pager = Pager::open_or_create(MemoryPageDevice::new(0).unwrap()).unwrap();
        let mut transaction = pager.begin_write().unwrap();
        Btree::create(&mut transaction, 99).unwrap();
        transaction.commit(0, 0, None).unwrap();
        assert_eq!(pager.active_metadata().superblock.live_data_page_count, 1);
        let device = pager.into_device();

        let error = match PagedStorage::open(device.clone()) {
            Ok(_) => panic!("rootless live pages must not open as an empty database"),
            Err(error) => error,
        };
        assert_eq!(error.code, "STORAGE_CORRUPT");

        let error = match PagedStorage::from_in_memory(device, &source()) {
            Ok(_) => panic!("rootless live pages must not be overwritten by import"),
            Err(error) => error,
        };
        assert_eq!(error.code, "DATABASE_NOT_EMPTY");
    }

    #[test]
    fn reopening_rejects_an_index_missing_eligible_table_rows() {
        let source = source();
        let paged =
            PagedStorage::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
        let index = paged.indexes.get("posts_author").unwrap().clone();
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
        .unwrap();
        transaction.commit(revision, 0, Some(catalog_root)).unwrap();

        let error = match PagedStorage::open(pager.into_device()) {
            Ok(_) => panic!("an incomplete secondary index must fail closed"),
            Err(error) => error,
        };
        assert_eq!(error.code, "STORAGE_CORRUPT");
    }
}
