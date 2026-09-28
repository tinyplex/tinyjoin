use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use crate::{
    Btree, ColumnType, EngineError, ExecuteResult, PageDevice, PageId, Pager,
    PagerWriteTransaction, QueryResult, Result, Row, RowChange, StorageReader, TreeId,
    VisitControl, VisitOutcome,
    btree::BatchChange,
    hash::{EMPTY_HASH, combine, identify},
    paged_codec::{
        CATALOG_TREE_ID, CatalogHeader, CatalogIndexRecord, IndexEntry, IndexEntryLayout,
        MAX_CATALOG_INDEXES, MAX_CATALOG_TABLES, MAX_TREE_ID, RecordLayout,
        encode_catalog_header_record, encode_catalog_index_key, encode_catalog_index_record,
        encode_catalog_table_key, encode_primary_key, encode_record_index_entry,
        encode_record_index_prefix, encode_row, encode_secondary_index_entry_key,
        encode_secondary_index_prefix, index_column_positions, leading_key_component,
        secondary_index_entry_matches_prefix, secondary_index_primary_key,
        secondary_index_primary_key_for_definition,
    },
    paged_storage::{
        EntryVisitor, PagedIndex, PagedTable, adjusted_count, batch_too_large,
        dangling_index_entry, ensure_batch_bytes, limit_error, ranged_index_primary_key,
        storage_corrupt, unique_violation,
    },
    row::{HeldRow, RowRef},
    statement::{PlannedDml, PreviousRow, Statement, WriteStatement, change_table},
    storage::{
        KeyOrder, KeyRange, estimated_record_bytes, estimated_row_bytes, preflight_row_write_set,
        schema_with_added_column, validate_schema,
    },
};

/// The catalog root written by one script, and the fingerprint of the data it describes.
struct CatalogPublication {
    root_page_id: PageId,
    database_hash: u64,
}

pub(crate) struct ScriptPublication {
    pub(crate) committed: bool,
    pub(crate) revision: u64,
    pub(crate) next_tree_id: TreeId,
    pub(crate) tables: Rc<BTreeMap<String, PagedTable>>,
    pub(crate) indexes: Rc<BTreeMap<String, PagedIndex>>,
    pub(crate) results: Vec<ExecuteResult>,
}

/// A row a write changes: the row its key held, and the record of the row it holds after, where
/// `None` is no row.
#[derive(Clone)]
pub(crate) struct ChangedRow {
    pub(crate) old: Option<HeldRow>,
    pub(crate) next: Option<Vec<u8>>,
}

/// A row a statement changes, and whether one of its changes writes a row, which makes a second
/// write of its key a conflict.
struct PlannedChange {
    row: ChangedRow,
    written: bool,
}

/// One table's changed rows by encoded primary key: a vector while their keys arrive in ascending
/// order, as a scan in key order plans them, and a map from the first that does not.
enum TableRows {
    Sorted(Vec<(Vec<u8>, PlannedChange)>),
    Map(BTreeMap<Vec<u8>, PlannedChange>),
}

impl TableRows {
    /// The change already planned for `key`, if any.
    #[allow(clippy::ptr_arg)]
    fn get_mut(&mut self, key: &Vec<u8>) -> Option<&mut PlannedChange> {
        if let Self::Sorted(rows) = self
            && rows.last().is_some_and(|(last, _)| last > key)
        {
            let mut map = BTreeMap::new();
            for (key, change) in std::mem::take(rows) {
                map.insert(key, change);
            }
            *self = Self::Map(map);
        }
        match self {
            Self::Sorted(rows) => match rows.last_mut() {
                Some((last, change)) if last == key => Some(change),
                _ => None,
            },
            Self::Map(rows) => rows.get_mut(key),
        }
    }

    /// Plans the first change of `key`, for which [`Self::get_mut`] found none.
    fn insert(&mut self, key: Vec<u8>, change: PlannedChange) {
        match self {
            Self::Sorted(rows) => rows.push((key, change)),
            Self::Map(rows) => {
                rows.insert(key, change);
            }
        }
    }

    /// The rows in key order.
    fn changes(&self) -> Vec<(&[u8], &ChangedRow)> {
        let mut changes = Vec::new();
        match self {
            Self::Sorted(rows) => {
                for (key, change) in rows {
                    changes.push((key.as_slice(), &change.row));
                }
            }
            Self::Map(rows) => {
                for (key, change) in rows {
                    changes.push((key.as_slice(), &change.row));
                }
            }
        }
        changes
    }
}

/// A planned change's row: a map, or a key a delete names, or the encoded key and record of a
/// row planned straight into its stored entry.
enum PlannedRow {
    Map(Row),
    Record(Vec<u8>, Vec<u8>),
}

/// The rows a write changes in one table, each with its encoded primary key, in key order.
pub(crate) type TableChanges<'a> = (&'a str, Vec<(&'a [u8], &'a ChangedRow)>);

/// The estimated bytes of a row a writer holds, which [`estimated_row_bytes`] finds for it as a
/// map, reading a stored entry in place.
pub(crate) fn held_row_bytes(table: &PagedTable, row: &HeldRow) -> Result<usize> {
    match row {
        HeldRow::Map(row) => estimated_row_bytes(row),
        HeldRow::Stored(entry) => {
            estimated_record_bytes(&table.record(entry.key(), entry.value())?)
        }
    }
}

struct PagedScriptCandidate<'a, D: PageDevice> {
    transaction: RefCell<PagerWriteTransaction<'a, D>>,
    catalog_root: Option<PageId>,
    base_revision: u64,
    next_tree_id: TreeId,
    // Shared with the storage until this candidate changes them.
    tables: Rc<BTreeMap<String, PagedTable>>,
    indexes: Rc<BTreeMap<String, PagedIndex>>,
    // The catalog as this candidate found it.
    base_tables: Rc<BTreeMap<String, PagedTable>>,
    base_indexes: Rc<BTreeMap<String, PagedIndex>>,
    mutated: bool,
    operations: Cell<usize>,
    result_bytes: usize,
}

pub(crate) fn execute<D: PageDevice>(
    pager: &mut Pager<D>,
    base_revision: u64,
    next_tree_id: TreeId,
    tables: Rc<BTreeMap<String, PagedTable>>,
    indexes: Rc<BTreeMap<String, PagedIndex>>,
    statements: Vec<Statement>,
) -> Result<ScriptPublication> {
    if statements.is_empty() {
        return Ok(ScriptPublication {
            committed: false,
            revision: base_revision,
            next_tree_id,
            tables,
            indexes,
            results: vec![],
        });
    }

    let mut candidate = begin_candidate(pager, base_revision, next_tree_id, tables, indexes)?;
    let execution = candidate.execute_all(statements);
    finish_candidate(candidate, execution)
}

/// Commits the rows a transaction staged. Staging validated them, statement by statement, against
/// the limits and unique indexes a script's writes are checked against, and found the rows they
/// replace; the transaction's base revision is this one, so both still hold.
pub(crate) fn execute_changed_rows<D: PageDevice>(
    pager: &mut Pager<D>,
    base_revision: u64,
    next_tree_id: TreeId,
    tables: Rc<BTreeMap<String, PagedTable>>,
    indexes: Rc<BTreeMap<String, PagedIndex>>,
    changes: &[TableChanges<'_>],
) -> Result<ScriptPublication> {
    let mut candidate = begin_candidate(pager, base_revision, next_tree_id, tables, indexes)?;
    let execution = (|| {
        for (table, rows) in changes {
            let index_count = candidate
                .indexes
                .values()
                .filter(|index| index.definition.table == *table)
                .count();
            candidate.charge_operations(
                rows.len()
                    .saturating_mul(index_count.saturating_mul(2).saturating_add(1)),
            )?;
        }
        candidate.apply_row_changes(changes)?;
        candidate.mutated = true;
        Ok(vec![])
    })();
    finish_candidate(candidate, execution)
}

fn begin_candidate<'a, D: PageDevice>(
    pager: &'a mut Pager<D>,
    base_revision: u64,
    next_tree_id: TreeId,
    tables: Rc<BTreeMap<String, PagedTable>>,
    indexes: Rc<BTreeMap<String, PagedIndex>>,
) -> Result<PagedScriptCandidate<'a, D>> {
    let catalog_root = pager.catalog_root_page_id();
    let transaction = pager.begin_write()?;
    Ok(PagedScriptCandidate {
        transaction: RefCell::new(transaction),
        catalog_root,
        base_revision,
        next_tree_id,
        base_tables: Rc::clone(&tables),
        base_indexes: Rc::clone(&indexes),
        tables,
        indexes,
        mutated: false,
        operations: Cell::new(0),
        // Structured exec responses retain the canonical bridge's three-byte
        // logical envelope and u32 result-count budget.
        result_bytes: 7,
    })
}

fn finish_candidate<D: PageDevice>(
    mut candidate: PagedScriptCandidate<'_, D>,
    execution: Result<Vec<ExecuteResult>>,
) -> Result<ScriptPublication> {
    match execution {
        Ok(results) if candidate.mutated => candidate.commit(results),
        Ok(results) => {
            let revision = candidate.base_revision;
            let next_tree_id = candidate.next_tree_id;
            let tables = std::mem::take(&mut candidate.tables);
            let indexes = std::mem::take(&mut candidate.indexes);
            candidate.transaction.into_inner().abort();
            Ok(ScriptPublication {
                committed: false,
                revision,
                next_tree_id,
                tables,
                indexes,
                results,
            })
        }
        Err(error) => {
            candidate.transaction.into_inner().abort();
            Err(error)
        }
    }
}

impl<D: PageDevice> PagedScriptCandidate<'_, D> {
    fn execute_all(&mut self, statements: Vec<Statement>) -> Result<Vec<ExecuteResult>> {
        let mut results = Vec::with_capacity(statements.len());
        for statement in statements {
            let result = self.execute_statement(statement)?;
            retain_result(&mut self.result_bytes, &result)?;
            results.push(result);
        }
        Ok(results)
    }

    fn execute_statement(&mut self, statement: Statement) -> Result<ExecuteResult> {
        match statement {
            Statement::Select(plan) => execute_query_result(crate::query::execute(self, &plan)?),
            Statement::Aggregate(plan) => {
                execute_query_result(crate::aggregate::execute(self, &plan)?)
            }
            Statement::Join(plan) => execute_query_result(crate::join::execute(self, &plan)?),
            Statement::Write(statement) => self.execute_write(&statement),
        }
    }

    fn execute_write(&mut self, statement: &WriteStatement) -> Result<ExecuteResult> {
        // Only a row mutation reports keys; DDL changes a table without naming rows.
        let mut keys = BTreeMap::new();
        let outcome = match statement {
            WriteStatement::CreateTable {
                schema,
                if_not_exists,
            } => {
                let outcome = crate::statement::plan_create_table(self, schema, *if_not_exists)?;
                if outcome.mutated {
                    validate_schema(schema)?;
                    if self.tables.len() == MAX_CATALOG_TABLES as usize {
                        return Err(limit_error(format!(
                            "A catalog cannot contain more than {MAX_CATALOG_TABLES} tables"
                        )));
                    }
                    let tree_id = self.allocate_tree_id()?;
                    let table = PagedTable::new(schema.clone(), tree_id, None, 0, EMPTY_HASH)?;
                    Rc::make_mut(&mut self.tables).insert(schema.name.clone(), table);
                }
                outcome
            }
            WriteStatement::CreateIndex {
                definition,
                if_not_exists,
            } => {
                let outcome =
                    crate::statement::plan_create_index(self, definition, *if_not_exists)?;
                if outcome.mutated {
                    self.create_index(definition)?;
                }
                outcome
            }
            WriteStatement::DropTable { table, if_exists } => {
                let outcome = crate::statement::plan_drop_table(self, table, *if_exists)?;
                if outcome.mutated {
                    self.drop_table(table)?;
                }
                outcome
            }
            WriteStatement::DropIndex { name, if_exists } => {
                let outcome = crate::statement::plan_drop_index(self, name, *if_exists)?;
                if outcome.mutated {
                    self.drop_index(name)?;
                }
                outcome
            }
            WriteStatement::AddColumn {
                table,
                column,
                if_not_exists,
            } => {
                let outcome =
                    crate::statement::plan_add_column(self, table, column, *if_not_exists)?;
                if outcome.mutated {
                    self.add_column(table, column)?;
                }
                outcome
            }
            WriteStatement::Insert { .. }
            | WriteStatement::Update { .. }
            | WriteStatement::Delete { .. } => {
                let PlannedDml {
                    outcome,
                    changes,
                    previous,
                } = crate::statement::plan_dml(self, statement)?;
                if outcome.mutated {
                    keys = crate::statement::changed_keys(self, &changes, &previous)?;
                    self.apply_changes(changes, previous)?;
                }
                outcome
            }
        };

        let fields = crate::statement::write_result_fields(self, statement)?;
        if outcome.mutated {
            self.mutated = true;
        }
        Ok(ExecuteResult {
            command: outcome.command.to_owned(),
            revision: self.base_revision,
            row_count: outcome.row_count,
            fields,
            rows: outcome.rows,
            tables: outcome.tables,
            keys,
        })
    }

    fn allocate_tree_id(&mut self) -> Result<TreeId> {
        if self.next_tree_id >= MAX_TREE_ID {
            return Err(limit_error("The catalog tree ID range is exhausted"));
        }
        let tree_id = self.next_tree_id;
        self.next_tree_id += 1;
        Ok(tree_id)
    }

    fn create_index(&mut self, definition: &crate::IndexDefinition) -> Result<()> {
        if self.indexes.len() == MAX_CATALOG_INDEXES as usize {
            return Err(limit_error(format!(
                "A catalog cannot contain more than {MAX_CATALOG_INDEXES} indexes"
            )));
        }
        let table = self
            .tables
            .get(&definition.table)
            .cloned()
            .ok_or_else(|| EngineError::table_not_found(&definition.table))?;
        let tree_id = self.allocate_tree_id()?;
        let mut index = IndexBuild {
            definition,
            tree_id,
            root_page_id: None,
            entry_count: 0,
        };
        if let Some(table_root) = table.root_page_id {
            let positions = index_column_positions(&table.schema, definition)?;
            let mut transaction = self.transaction.borrow_mut();
            let mut rows =
                Btree::cursor_in_transaction(&mut transaction, table_root, table.tree_id)?;
            let mut entries = Vec::new();
            let mut bytes = 0usize;
            while let Some((primary_key, value)) = rows.next_in_transaction(&mut transaction)? {
                self.charge_operations(1)?;
                let record = table.record(&primary_key, &value)?;
                let Some(entry) = encode_record_index_entry(&positions, &record)? else {
                    continue;
                };
                bytes += entry.0.len() + 48;
                entries.push(entry);
                if bytes >= INDEX_BUILD_CHUNK_BYTES {
                    index.write(&mut transaction, &mut entries)?;
                    bytes = 0;
                }
            }
            index.write(&mut transaction, &mut entries)?;
        }
        let IndexBuild {
            root_page_id,
            entry_count,
            ..
        } = index;
        Rc::make_mut(&mut self.indexes).insert(
            definition.name.clone(),
            PagedIndex {
                definition: definition.clone(),
                tree_id,
                root_page_id,
                entry_count,
            },
        );
        Ok(())
    }

    fn drop_index(&mut self, name: &str) -> Result<()> {
        let index = Rc::make_mut(&mut self.indexes)
            .remove(name)
            .ok_or_else(|| {
                EngineError::new("INDEX_NOT_FOUND", format!("Index `{name}` is not defined"))
            })?;
        if let Some(root) = index.root_page_id {
            Btree::reclaim(&mut self.transaction.borrow_mut(), root, index.tree_id)?;
        }
        self.charge_operations(1)
    }

    fn drop_table(&mut self, name: &str) -> Result<()> {
        let index_names = self
            .indexes
            .iter()
            .filter(|(_, index)| index.definition.table == name)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        for index_name in index_names {
            self.drop_index(&index_name)?;
        }
        let table = Rc::make_mut(&mut self.tables)
            .remove(name)
            .ok_or_else(|| EngineError::table_not_found(name))?;
        if let Some(root) = table.root_page_id {
            Btree::reclaim(&mut self.transaction.borrow_mut(), root, table.tree_id)?;
        }
        self.charge_operations(1)
    }

    fn add_column(&mut self, table_name: &str, column: &crate::ColumnDefinition) -> Result<()> {
        let table = self
            .tables
            .get(table_name)
            .ok_or_else(|| EngineError::table_not_found(table_name))?;
        let schema = schema_with_added_column(&table.schema, column)?;
        // A row record omits trailing columns that equal their defaults, so every stored row
        // already encodes the new column's default and none is rewritten. The table's fingerprint
        // still changes, because the database fingerprint covers each table's columns.
        Rc::make_mut(&mut self.tables)
            .get_mut(table_name)
            .expect("the altered table was resolved above")
            .set_schema(schema)?;
        self.charge_operations(1)
    }

    /// Applies a statement's planned changes, which change one table. Planning normalized every
    /// row they write, and read the row each key held wherever it could, which `previous` reports.
    fn apply_changes(
        &mut self,
        input_changes: Vec<RowChange>,
        previous: Vec<PreviousRow>,
    ) -> Result<()> {
        let Some(first) = input_changes.first() else {
            return Ok(());
        };
        let table_name = change_table(first).clone();
        let definitions = self
            .indexes
            .values()
            .map(|index| &index.definition)
            .collect::<Vec<_>>();
        preflight_row_write_set(
            &input_changes,
            &|name| {
                let table = self.tables.get(name)?;
                Some((&*table.schema, Some(&**table.layout())))
            },
            &definitions,
            Default::default(),
        )?;
        let index_count = definitions
            .iter()
            .filter(|definition| definition.table == table_name)
            .count();
        for change in &input_changes {
            if *change_table(change) != table_name {
                return Err(EngineError::new(
                    "INTERNAL_ERROR",
                    "A statement's changes name more than one table",
                ));
            }
            self.charge_operations(index_count.saturating_mul(2).saturating_add(1))?;
        }

        let table = self
            .tables
            .get(&table_name)
            .ok_or_else(|| EngineError::table_not_found(&table_name))?;
        let mut retained_bytes = 0usize;
        let mut changes = TableRows::Sorted(vec![]);
        let mut previous = previous.into_iter();
        for change in input_changes {
            let held = previous.next().unwrap_or(PreviousRow::Unread);
            let (row, is_delete) = match change {
                RowChange::Upsert { row, .. } => (PlannedRow::Map(row), false),
                RowChange::Delete { key, .. } => (PlannedRow::Map(key), true),
                RowChange::Put { key, record, .. } => (PlannedRow::Record(key, record), false),
            };
            // A stored row planning held is the row the change's key holds, so its entry's key is
            // the change's encoded key.
            let key = match (&row, &held) {
                (PlannedRow::Record(key, _), _) => key.clone(),
                (PlannedRow::Map(row), PreviousRow::Read(Some(HeldRow::Stored(entry)))) => {
                    debug_assert_eq!(entry.key(), encode_primary_key(&table.schema, row)?);
                    entry.key().to_vec()
                }
                (PlannedRow::Map(row), _) => encode_primary_key(&table.schema, row)?,
            };
            let existing = changes.get_mut(&key);
            if !is_delete && existing.as_ref().is_some_and(|existing| existing.written) {
                return Err(EngineError::constraint_violation(format!(
                    "SQL statement would write canonical primary key in `{table_name}` more than once"
                )));
            }
            // The row is retained as its record, charged as the map it was planned as.
            let (next, next_bytes) = match row {
                _ if is_delete => (None, 0),
                PlannedRow::Map(row) => (
                    Some(encode_row(&table.schema, &row)?),
                    estimated_row_bytes(&row)?,
                ),
                PlannedRow::Record(_, record) => {
                    let bytes = estimated_record_bytes(&table.record(&key, &record)?)?;
                    (Some(record), bytes)
                }
            };
            if let Some(existing) = existing {
                let previous_bytes = match &existing.row.next {
                    Some(record) => estimated_record_bytes(&table.record(&key, record)?)?,
                    None => 0,
                };
                retained_bytes = retained_bytes
                    .checked_sub(previous_bytes)
                    .and_then(|bytes| bytes.checked_add(next_bytes))
                    .ok_or_else(batch_too_large)?;
                ensure_batch_bytes(retained_bytes)?;
                existing.row.next = next;
                existing.written |= !is_delete;
                continue;
            }
            let old = match held {
                PreviousRow::Read(row) => {
                    // Charged as the lookup it replaces.
                    self.charge_operations(1)?;
                    row
                }
                PreviousRow::Unread => self
                    .lookup_record(&table_name, &key)?
                    .map(|value| Ok::<_, EngineError>(table.record(&key, &value)?.to_entry()))
                    .transpose()?
                    .map(HeldRow::Stored),
            };
            let old_bytes = old
                .as_ref()
                .map_or(Ok(0), |old| held_row_bytes(table, old))?;
            retained_bytes = retained_bytes
                .checked_add(key.len())
                .and_then(|bytes| bytes.checked_add(old_bytes))
                .and_then(|bytes| bytes.checked_add(next_bytes))
                .and_then(|bytes| bytes.checked_add(96))
                .ok_or_else(batch_too_large)?;
            ensure_batch_bytes(retained_bytes)?;
            changes.insert(
                key,
                PlannedChange {
                    row: ChangedRow { old, next },
                    written: !is_delete,
                },
            );
        }
        let changes = [(table_name.as_str(), changes.changes())];
        self.validate_changed_unique_indexes(&changes, retained_bytes)?;
        self.apply_row_changes(&changes)
    }

    fn validate_changed_unique_indexes(
        &self,
        changes: &[TableChanges<'_>],
        mut retained_bytes: usize,
    ) -> Result<()> {
        for (table_name, table_changes) in changes {
            let table = &self.tables[*table_name];
            for index in self
                .indexes
                .values()
                .filter(|index| index.definition.table == *table_name && index.definition.unique)
            {
                let positions = index_column_positions(&table.schema, &index.definition)?;
                let mut changed_prefixes = BTreeMap::<Vec<u8>, &[u8]>::new();
                for (primary_key, change) in table_changes {
                    let Some(record) = &change.next else {
                        continue;
                    };
                    let Some(prefix) = encode_record_index_prefix(
                        &positions,
                        &table.record(primary_key, record)?,
                    )?
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
                    for existing_primary_key in self.index_primary_keys(index, &prefix)? {
                        retained_bytes = retained_bytes
                            .checked_add(existing_primary_key.len())
                            .ok_or_else(batch_too_large)?;
                        ensure_batch_bytes(retained_bytes)?;
                        if existing_primary_key == *primary_key {
                            continue;
                        }
                        let existing = table_changes
                            .binary_search_by(|(key, _)| (*key).cmp(&existing_primary_key))
                            .map(|position| table_changes[position].1);
                        let existing_moves = match existing {
                            Ok(existing) => match &existing.next {
                                None => true,
                                Some(record) => {
                                    let record = table.record(&existing_primary_key, record)?;
                                    encode_record_index_prefix(&positions, &record)?.as_deref()
                                        != Some(prefix.as_slice())
                                }
                            },
                            Err(_) => false,
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

    /// Writes each table's changed rows, and its indexes' entries for them. Each table's changes
    /// are in key order, which is the table's tree order.
    fn apply_row_changes(&mut self, changes: &[TableChanges<'_>]) -> Result<()> {
        for (table_name, table_changes) in changes {
            let table = Rc::make_mut(&mut self.tables)
                .get_mut(*table_name)
                .ok_or_else(|| EngineError::table_not_found(table_name))?;
            let mut transaction = self.transaction.borrow_mut();
            let batch = table_changes
                .iter()
                .map(|(key, change)| BatchChange {
                    key,
                    value: change.next.as_deref(),
                })
                .collect::<Vec<_>>();
            let inserted = table_changes
                .iter()
                .filter(|(_, change)| change.old.is_none() && change.next.is_some())
                .count();
            let removed = table_changes
                .iter()
                .filter(|(_, change)| change.old.is_some() && change.next.is_none())
                .count();
            let applied =
                Btree::apply(&mut transaction, table.root_page_id, table.tree_id, &batch)?;
            if (applied.inserted, applied.removed) != (inserted, removed) {
                return Err(storage_corrupt(format!(
                    "Table `{table_name}` does not hold the rows its changes replace"
                )));
            }
            table.root_page_id = applied.root_page_id;
            if let Some(hash) = applied.hash {
                table.hash = hash;
            }
            table.row_count = adjusted_count(table.row_count, inserted, removed, "table row")?;

            for index in Rc::make_mut(&mut self.indexes)
                .values_mut()
                .filter(|index| index.definition.table == *table_name)
            {
                // Entries end with their row's primary key, so no two changes share one.
                let positions = index_column_positions(&table.schema, &index.definition)?;
                let mut entries = Vec::new();
                for (key, change) in table_changes {
                    let old_key = match &change.old {
                        None => None,
                        Some(HeldRow::Map(row)) => {
                            encode_secondary_index_entry_key(&table.schema, &index.definition, row)?
                        }
                        Some(HeldRow::Stored(entry)) => encode_record_index_entry(
                            &positions,
                            &table.record(entry.key(), entry.value())?,
                        )?
                        .map(|(key, _)| key),
                    };
                    let next_key = match &change.next {
                        Some(record) => {
                            encode_record_index_entry(&positions, &table.record(key, record)?)?
                                .map(|(key, _)| key)
                        }
                        None => None,
                    };
                    if old_key == next_key {
                        continue;
                    }
                    entries.extend(old_key.map(|key| (key, false)));
                    entries.extend(next_key.map(|key| (key, true)));
                }
                if entries.is_empty() {
                    continue;
                }
                let mut batch = entries
                    .iter()
                    .map(|(key, insert)| BatchChange {
                        key,
                        value: insert.then_some(&[][..]),
                    })
                    .collect::<Vec<_>>();
                BatchChange::sort(&mut batch);
                let inserted = entries.iter().filter(|(_, insert)| *insert).count();
                let removed = entries.len() - inserted;
                let applied =
                    Btree::apply(&mut transaction, index.root_page_id, index.tree_id, &batch)?;
                if applied.removed != removed {
                    return Err(storage_corrupt(format!(
                        "Index `{}` is missing an entry for a candidate row",
                        index.definition.name
                    )));
                }
                if applied.inserted != inserted {
                    return Err(storage_corrupt(format!(
                        "Index `{}` already holds an entry for a candidate row",
                        index.definition.name
                    )));
                }
                index.root_page_id = applied.root_page_id;
                index.entry_count =
                    adjusted_count(index.entry_count, inserted, removed, "index entry")?;
            }
        }
        Ok(())
    }

    /// The record the encoded primary key `key` holds in `table_name`, if any.
    fn lookup_record(&self, table_name: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let table = self
            .tables
            .get(table_name)
            .ok_or_else(|| EngineError::table_not_found(table_name))?;
        let Some(root) = table.root_page_id else {
            return Ok(None);
        };
        self.charge_operations(1)?;
        Btree::get_in_transaction(&mut self.transaction.borrow_mut(), root, table.tree_id, key)
    }

    fn index_primary_keys(&self, index: &PagedIndex, prefix: &[u8]) -> Result<Vec<Vec<u8>>> {
        let Some(root) = index.root_page_id else {
            return Ok(vec![]);
        };
        let mut transaction = self.transaction.borrow_mut();
        let mut cursor =
            Btree::cursor_from_in_transaction(&mut transaction, root, index.tree_id, prefix)?;
        let mut primary_keys = Vec::new();
        while let Some((entry_key, value)) = cursor.next_in_transaction(&mut transaction)? {
            self.charge_operations(1)?;
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

    /// Calls `each` with every entry of an index tree whose leading component, of type `leading`,
    /// lies within `range`, in index order, until it stops.
    fn walk_index_range(
        &self,
        root: PageId,
        tree_id: TreeId,
        leading: ColumnType,
        range: &KeyRange,
        each: &mut EntryVisitor<'_>,
    ) -> Result<VisitOutcome> {
        let mut cursor = Btree::cursor_from_in_transaction(
            &mut self.transaction.borrow_mut(),
            root,
            tree_id,
            range.start(),
        )?;
        loop {
            let next = cursor.next_entry_in_transaction(&mut self.transaction.borrow_mut())?;
            let Some((entry, value)) = next else {
                return Ok(VisitOutcome::Complete);
            };
            if !range.contains(leading_key_component(entry, leading)?) {
                return Ok(VisitOutcome::Complete);
            }
            if each(entry, &value)? == VisitControl::Stop {
                return Ok(VisitOutcome::Stopped);
            }
        }
    }

    fn charge_operations(&self, count: usize) -> Result<()> {
        crate::sql_script::charge_operations(&self.operations, count)
    }

    fn commit(mut self, mut results: Vec<ExecuteResult>) -> Result<ScriptPublication> {
        let revision = crate::revision::next_database_revision(self.base_revision)?;
        let catalog = self.write_catalog()?;
        for result in &mut results {
            result.revision = revision;
        }
        let transaction = self.transaction.into_inner();
        transaction.commit(revision, catalog.database_hash, Some(catalog.root_page_id))?;
        Ok(ScriptPublication {
            committed: true,
            revision,
            next_tree_id: self.next_tree_id,
            tables: self.tables,
            indexes: self.indexes,
            results,
        })
    }

    fn write_catalog(&mut self) -> Result<CatalogPublication> {
        self.charge_operations(
            1 + self.base_tables.len()
                + self.base_indexes.len()
                + self.tables.len()
                + self.indexes.len(),
        )?;
        // Records of dropped tables and indexes are deleted. Every other record is written only
        // when it changed, and the header, which is cheap to encode, is left to the batch, which
        // keeps a value it already holds.
        let mut changes = Vec::new();
        for name in self.base_indexes.keys() {
            if !self.indexes.contains_key(name) {
                changes.push((encode_catalog_index_key(name)?, None));
            }
        }
        for name in self.base_tables.keys() {
            if !self.tables.contains_key(name) {
                changes.push((encode_catalog_table_key(name)?, None));
            }
        }
        let dropped = changes.len();
        let (key, value) = encode_catalog_header_record(&CatalogHeader {
            next_tree_id: self.next_tree_id,
            table_count: u32::try_from(self.tables.len())
                .map_err(|_| limit_error("The catalog contains too many tables"))?,
            index_count: u32::try_from(self.indexes.len())
                .map_err(|_| limit_error("The catalog contains too many indexes"))?,
        })?;
        changes.push((key, Some(value)));
        let mut database_hash = EMPTY_HASH;
        for (name, table) in self.tables.iter() {
            if self
                .base_tables
                .get(name)
                .is_none_or(|base| !same_table_record(base, table))
            {
                let (key, value) = table.catalog_record()?;
                changes.push((key, Some(value)));
            }
            // Binding each table's fingerprint to its name keeps two tables from cancelling each
            // other out, and makes exchanging the contents of two tables a visible change.
            // A table's columns are part of its identity: packed rows do not name their columns,
            // so two tables whose columns differ are different data even when their row bytes
            // match.
            database_hash = combine(
                database_hash,
                identify(
                    table.schema.name.as_bytes(),
                    combine(table.hash, table.columns_fingerprint()),
                ),
            );
        }
        for (name, index) in self.indexes.iter() {
            if self
                .base_indexes
                .get(name)
                .is_none_or(|base| !same_index_record(base, index))
            {
                let (key, value) = encode_catalog_index_record(&CatalogIndexRecord {
                    definition: index.definition.clone(),
                    tree_id: index.tree_id,
                    root_page_id: index.root_page_id,
                    entry_count: index.entry_count as u64,
                })?;
                changes.push((key, Some(value)));
            }
        }
        let mut batch = changes
            .iter()
            .map(|(key, value)| BatchChange {
                key,
                value: value.as_deref(),
            })
            .collect::<Vec<_>>();
        BatchChange::sort(&mut batch);
        let applied = Btree::apply(
            &mut self.transaction.borrow_mut(),
            self.catalog_root,
            CATALOG_TREE_ID,
            &batch,
        )?;
        if applied.removed != dropped {
            return Err(storage_corrupt(
                "The catalog is missing the record of a dropped table or index",
            ));
        }
        Ok(CatalogPublication {
            root_page_id: applied
                .root_page_id
                .ok_or_else(|| storage_corrupt("The catalog has no header"))?,
            database_hash,
        })
    }
}

/// Whether two versions of a table have the same catalog record.
fn same_table_record(left: &PagedTable, right: &PagedTable) -> bool {
    left.tree_id == right.tree_id
        && left.root_page_id == right.root_page_id
        && left.row_count == right.row_count
        && left.hash == right.hash
        && left.schema == right.schema
}

/// Whether two versions of an index have the same catalog record.
fn same_index_record(left: &PagedIndex, right: &PagedIndex) -> bool {
    left.tree_id == right.tree_id
        && left.root_page_id == right.root_page_id
        && left.entry_count == right.entry_count
        && left.definition == right.definition
}

/// How many bytes of entries an index build sorts in memory before writing them to its tree.
#[cfg(not(test))]
const INDEX_BUILD_CHUNK_BYTES: usize = 16 * 1024 * 1024;
/// Small enough that tests build indexes over a few hundred rows in several chunks.
#[cfg(test)]
const INDEX_BUILD_CHUNK_BYTES: usize = 16 * 1024;

/// A new index, built from its table's rows a sorted chunk of entries at a time.
struct IndexBuild<'a> {
    definition: &'a crate::IndexDefinition,
    tree_id: TreeId,
    root_page_id: Option<PageId>,
    entry_count: usize,
}

impl IndexBuild<'_> {
    /// Sorts a chunk of entries, each with the length of its indexed tuple, checks that a unique
    /// index holds no tuple twice, and writes the chunk to the tree in one batch.
    fn write<D: PageDevice>(
        &mut self,
        transaction: &mut PagerWriteTransaction<'_, D>,
        entries: &mut Vec<(Vec<u8>, usize)>,
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        // Each entry ends with its row's primary key, so no two are equal, and the pairs sort as
        // their entries do.
        entries.sort_unstable();
        if self.definition.unique {
            // Entries for one tuple sort together, so a repeated tuple has its neighbor's prefix.
            for pair in entries.windows(2) {
                if secondary_index_entry_matches_prefix(&pair[1].0, &pair[0].0[..pair[0].1]) {
                    return Err(unique_violation(&self.definition.name));
                }
            }
            if let Some(root) = self.root_page_id {
                for (key, tuple) in entries.iter() {
                    let prefix = &key[..*tuple];
                    let mut existing =
                        Btree::cursor_from_in_transaction(transaction, root, self.tree_id, prefix)?;
                    if let Some((existing_key, _)) = existing.next_in_transaction(transaction)?
                        && secondary_index_entry_matches_prefix(&existing_key, prefix)
                    {
                        return Err(unique_violation(&self.definition.name));
                    }
                }
            }
        }
        let batch = entries
            .iter()
            .map(|(key, _)| BatchChange {
                key,
                value: Some(&[]),
            })
            .collect::<Vec<_>>();
        let applied = Btree::apply(transaction, self.root_page_id, self.tree_id, &batch)?;
        if applied.inserted != entries.len() {
            return Err(storage_corrupt(format!(
                "Rows of one table share an entry in new index `{}`",
                self.definition.name
            )));
        }
        self.root_page_id = applied.root_page_id;
        self.entry_count = self
            .entry_count
            .checked_add(entries.len())
            .ok_or_else(batch_too_large)?;
        entries.clear();
        Ok(())
    }
}

/// Retains a conservative upper bound for the complete structured exec response.
///
/// The canonical bridge budget uses at most 36 fixed logical bytes per result, eight bytes per
/// field name, four bytes per table name, and a tagged JSON estimate for each row. The
/// storage-owned row estimate is strictly larger: it starts at 32 bytes, charges at least 64 bytes
/// per map entry, doubles every key and value estimate, and the extra 64 below covers the row
/// count/framing. Consequently this also bounds the bridge's 16 MiB byte cap and
/// one-million-node cap early enough to reject an oversized result before the page generation
/// commits; the bridge materializes the already-bounded JavaScript result afterward.
pub(crate) fn retain_result(result_bytes: &mut usize, result: &ExecuteResult) -> Result<()> {
    let mut bytes = result
        .command
        .len()
        .checked_add(256)
        .ok_or_else(batch_too_large)?;
    for field in &result.fields {
        bytes = bytes
            .checked_add(field.name.len())
            .and_then(|bytes| bytes.checked_add(32))
            .ok_or_else(batch_too_large)?;
    }
    for row in &result.rows {
        bytes = bytes
            .checked_add(estimated_row_bytes(row)?)
            .and_then(|bytes| bytes.checked_add(64))
            .ok_or_else(batch_too_large)?;
    }
    for table in &result.tables {
        bytes = bytes
            .checked_add(table.len())
            .and_then(|bytes| bytes.checked_add(32))
            .ok_or_else(batch_too_large)?;
    }
    *result_bytes = result_bytes
        .checked_add(bytes)
        .ok_or_else(batch_too_large)?;
    ensure_batch_bytes(*result_bytes)
}

fn execute_query_result(result: QueryResult) -> Result<ExecuteResult> {
    Ok(ExecuteResult {
        command: "SELECT".to_owned(),
        revision: result.revision,
        row_count: result.rows.len(),
        fields: result.fields,
        rows: result.rows,
        tables: vec![],
        keys: BTreeMap::new(),
    })
}

impl<D: PageDevice> StorageReader for PagedScriptCandidate<'_, D> {
    fn charge_work(&self, operations: usize) -> Result<()> {
        self.charge_operations(operations)
    }

    fn visit_table(
        &self,
        table: &str,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        let table = self
            .tables
            .get(table)
            .ok_or_else(|| EngineError::table_not_found(table))?;
        let Some(root) = table.root_page_id else {
            return Ok(VisitOutcome::Complete);
        };
        let mut cursor =
            Btree::cursor_in_transaction(&mut self.transaction.borrow_mut(), root, table.tree_id)?;
        loop {
            let next = cursor.next_entry_in_transaction(&mut self.transaction.borrow_mut())?;
            let Some((key, value)) = next else {
                break;
            };
            self.charge_operations(1)?;
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
        let table = self
            .tables
            .get(table)
            .ok_or_else(|| EngineError::table_not_found(table))?;
        let Some(root) = table.root_page_id else {
            return Ok(VisitOutcome::Complete);
        };
        let key_type = table.leading_key_type();
        let mut cursor = match order {
            KeyOrder::Ascending => Btree::cursor_from_in_transaction(
                &mut self.transaction.borrow_mut(),
                root,
                table.tree_id,
                range.start(),
            )?,
            KeyOrder::Descending => Btree::cursor_before_in_transaction(
                &mut self.transaction.borrow_mut(),
                root,
                table.tree_id,
                range.end().as_deref(),
            )?,
        };
        loop {
            let next = cursor.next_entry_in_transaction(&mut self.transaction.borrow_mut())?;
            let Some((key, value)) = next else {
                break;
            };
            if !range.contains(leading_key_component(key, key_type)?) {
                break;
            }
            self.charge_operations(1)?;
            if visitor(&RowRef::record(table.record(key, &value)?))? == VisitControl::Stop {
                return Ok(VisitOutcome::Stopped);
            }
        }
        Ok(VisitOutcome::Complete)
    }

    fn table_row_count(&self, table: &str) -> Result<usize> {
        self.tables
            .get(table)
            .map(|table| table.row_count)
            .ok_or_else(|| EngineError::table_not_found(table))
    }

    fn lookup_primary_key(&self, table: &str, key: &Row) -> Result<Option<Row>> {
        let table_data = self
            .tables
            .get(table)
            .ok_or_else(|| EngineError::table_not_found(table))?;
        let key = encode_primary_key(&table_data.schema, key)?;
        self.lookup_record(table, &key)?
            .map(|value| table_data.record(&key, &value)?.to_row())
            .transpose()
    }

    fn visit_primary_key(
        &self,
        table: &str,
        key: &Row,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        let table_data = self
            .tables
            .get(table)
            .ok_or_else(|| EngineError::table_not_found(table))?;
        let key = encode_primary_key(&table_data.schema, key)?;
        match self.lookup_record(table, &key)? {
            Some(value)
                if visitor(&RowRef::record(table_data.record(&key, &value)?))?
                    == VisitControl::Stop =>
            {
                Ok(VisitOutcome::Stopped)
            }
            _ => Ok(VisitOutcome::Complete),
        }
    }

    fn record_layout(&self, table: &str) -> Option<Rc<RecordLayout>> {
        self.tables
            .get(table)
            .map(|table| Rc::clone(table.layout()))
    }

    fn holds_encoded_key(&self, table: &str, key: &[u8]) -> Result<bool> {
        Ok(self.lookup_record(table, key)?.is_some())
    }

    fn index_definition(&self, name: &str) -> Option<crate::IndexDefinition> {
        self.indexes.get(name).map(|index| index.definition.clone())
    }

    fn indexes_for_table(&self, table: &str) -> Result<Vec<crate::IndexDefinition>> {
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
        let mut cursor = Btree::cursor_from_in_transaction(
            &mut self.transaction.borrow_mut(),
            index_root,
            index.tree_id,
            &prefix,
        )?;
        loop {
            let next = cursor.next_entry_in_transaction(&mut self.transaction.borrow_mut())?;
            let Some((entry_key, value)) = next else {
                break;
            };
            self.charge_operations(1)?;
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
            let row_value = Btree::get_in_transaction(
                &mut self.transaction.borrow_mut(),
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
            self.charge_operations(1)?;
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
        // A range is read through the index only while it holds at most `limit` entries, which a
        // first walk counts without copying any, charged as collecting them would be.
        let mut entries = 0usize;
        self.walk_index_range(index_root, index.tree_id, types[0], range, &mut |_, _| {
            entries += 1;
            if entries > limit {
                return Ok(VisitControl::Stop);
            }
            self.charge_operations(1)?;
            Ok(VisitControl::Continue)
        })?;
        if entries > limit {
            return Ok(None);
        }
        // Rows are visited in primary-key order, as a table visit would find them.
        let mut primary_keys = BTreeSet::new();
        self.walk_index_range(
            index_root,
            index.tree_id,
            types[0],
            range,
            &mut |entry, value| {
                primary_keys.insert(ranged_index_primary_key(
                    &index.definition,
                    entry,
                    value,
                    &types,
                )?);
                Ok(VisitControl::Continue)
            },
        )?;
        for primary_key in &primary_keys {
            let value = Btree::get_in_transaction(
                &mut self.transaction.borrow_mut(),
                table_root,
                table_data.tree_id,
                primary_key,
            )?
            .ok_or_else(|| dangling_index_entry(&index.definition))?;
            self.charge_operations(1)?;
            if visitor(&RowRef::record(table_data.record(primary_key, &value)?))?
                == VisitControl::Stop
            {
                return Ok(Some(VisitOutcome::Stopped));
            }
        }
        Ok(Some(VisitOutcome::Complete))
    }

    fn visit_index_entries(
        &self,
        table: &str,
        columns: &[String],
        range: &KeyRange,
        layout: &IndexEntryLayout,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<Option<VisitOutcome>> {
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
        let leading = table_data.column_types(&columns[..1])?[0];
        self.walk_index_range(
            index_root,
            index.tree_id,
            leading,
            range,
            &mut |entry, _| {
                self.charge_operations(1)?;
                visitor(&RowRef::index(IndexEntry::new(entry, layout)))
            },
        )
        .map(Some)
    }

    fn table_schema(&self, table: &str) -> Result<Rc<crate::TableDefinition>> {
        self.tables
            .get(table)
            .map(|table| Rc::clone(&table.schema))
            .ok_or_else(|| EngineError::table_not_found(table))
    }

    fn revision(&self) -> u64 {
        self.base_revision
    }
}
