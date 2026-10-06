use serde_json::Value;
#[cfg(test)]
use std::collections::BTreeMap;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    rc::Rc,
};

#[cfg(test)]
use crate::RowChange;
use crate::hash::KeySet;
use crate::name_map::NameMap;
#[cfg(test)]
use crate::paged_codec::{
    CatalogHeader, encode_catalog_header_record, encode_catalog_index_record,
    encode_secondary_index_entry_key,
};
#[cfg(test)]
use crate::storage::preflight_row_write_set;
use crate::{
    ApplyOutcome, Btree, ColumnType, EngineError, ExecuteResult, IndexDefinition, PageDevice,
    PageId, Pager, Result, Row, StorageReader, TableDefinition, TreeId, VisitControl, VisitOutcome,
    btree::{BtreeCursor, BtreeReadView, get_from, open_cursor},
    paged_codec::{
        CATALOG_TREE_ID, CatalogIndexRecord, CatalogKey, CatalogTableRecord, FIRST_USER_TREE_ID,
        IndexEntry, IndexEntryLayout, PrimaryKey, RecordLayout, StoredEntry, StoredRecord,
        decode_catalog_header_record, decode_catalog_index_record, decode_catalog_key,
        decode_catalog_table_record, encode_catalog_schema,
        encode_catalog_table_record_with_schema, encode_primary_key, encode_record_index_entry,
        encode_record_index_prefix, encode_secondary_index_prefix, index_column_positions,
        index_entry_primary_key, leading_key_component, secondary_index_entry_matches_prefix,
        secondary_index_primary_key, secondary_index_primary_key_for_definition,
        stored_record_passes,
    },
    query::Filter,
    row::{HeldRow, RowRef, ValueRef},
    sql_script::charge_operations,
    storage::{
        KeyOrder, KeyRange, RowWriteUsage, normalize_row, preflight_record_write,
        preflight_row_write,
    },
};
/// A callback for each key and value of a B-tree entry, which says whether to go on.
pub(crate) type EntryVisitor<'a> = dyn FnMut(&[u8], &[u8]) -> Result<VisitControl> + 'a;

/// Reads the rows and index entries of a catalog's tables through B-tree pages: those committed,
/// or those a script's candidate sees, which charges each entry it reads to the script's `work`.
pub(crate) struct TreeReader<'a> {
    pub(crate) pages: &'a RefCell<dyn BtreeReadView + 'a>,
    pub(crate) tables: &'a NameMap<PagedTable>,
    pub(crate) indexes: &'a NameMap<PagedIndex>,
    pub(crate) work: Option<&'a Cell<usize>>,
}

impl TreeReader<'_> {
    fn charge(&self) -> Result<()> {
        match self.work {
            Some(work) => charge_operations(work, 1),
            None => Ok(()),
        }
    }

    fn table(&self, name: &str) -> Result<&PagedTable> {
        self.tables
            .get(name)
            .ok_or_else(|| EngineError::table_not_found(name))
    }

    /// The table, and its index on exactly `columns`, if it has one.
    fn index(&self, table: &str, columns: &[String]) -> Result<(&PagedTable, Option<&PagedIndex>)> {
        let index = self
            .indexes
            .values()
            .find(|index| index.definition.table == table && index.definition.columns == columns);
        Ok((self.table(table)?, index))
    }

    /// The value the encoded key `key` holds in a tree, charged as one read.
    pub(crate) fn get(&self, root: PageId, tree_id: TreeId, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.charge()?;
        get_from(&mut *self.pages.borrow_mut(), root, tree_id, key)
    }

    /// A cursor over a tree from `bound`, or moving backward from before it.
    fn cursor(
        &self,
        root: PageId,
        tree_id: TreeId,
        bound: Option<&[u8]>,
        backward: bool,
    ) -> Result<BtreeCursor> {
        open_cursor(
            &mut *self.pages.borrow_mut(),
            root,
            tree_id,
            bound,
            backward,
            false,
        )
    }

    pub(crate) fn visit_table(
        &self,
        table: &str,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.visit_rows_where(table, None, KeyOrder::Ascending, &[], None, &mut |_| Ok(()), visitor)
    }

    pub(crate) fn visit_table_range(
        &self,
        table: &str,
        range: &KeyRange,
        order: KeyOrder,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.visit_rows_where(table, Some(range), order, &[], None, &mut |_| Ok(()), visitor)
    }

    /// Visits the rows of `table` in `order`, those within `range` when one is given, except those
    /// whose encoded keys `except` holds, in ascending order; and of them, those `filter` accepts,
    /// or all without one. Each row read is charged to the reader's work and to `scanned`. Only a
    /// leaf that could hold one of `except` looks for it, once, so that a scan passing over the
    /// rows a transaction replaced compares no row with their keys; and a filter's record tests
    /// reject a row from its bytes, before anything is decoded or presented for it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn visit_rows_where(
        &self,
        table: &str,
        range: Option<&KeyRange>,
        order: KeyOrder,
        mut except: &[&[u8]],
        filter: Option<&Filter<'_>>,
        scanned: &mut dyn FnMut(usize) -> Result<()>,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        let table = self.table(table)?;
        let Some(root) = table.root_page_id else {
            return Ok(VisitOutcome::Complete);
        };
        let start = range.map_or(&[][..], KeyRange::start);
        let mut cursor = match order {
            KeyOrder::Ascending => self.cursor(root, table.tree_id, Some(start), false)?,
            KeyOrder::Descending => self.cursor(
                root,
                table.tree_id,
                range.and_then(KeyRange::end).as_deref(),
                true,
            )?,
        };
        let key_type = table.leading_key_type();
        let work = self.work;
        // No scan reads a table backward and passes rows over.
        let backward = order == KeyOrder::Descending;
        debug_assert!(!backward || except.is_empty());
        // Rows are read from the cursor's copy of each leaf, so the pages are borrowed only to move
        // between leaves, or to read a value that overflows, and the visitor can read them too.
        let mut read = |page_id: PageId| self.pages.borrow_mut().read_btree_page(page_id);
        // The filter's tests of stored columns, which each leaf's cells are tested with in one pass
        // that reads their bytes and calls nothing. A test of a primary-key column, and a filter
        // its tests do not decide, leave the filter to judge each row the pass accepts.
        let tests = filter.map_or(&[][..], Filter::record_tests);
        let mut stored_tests = Vec::with_capacity(tests.len());
        let mut judged = !filter.is_none_or(Filter::decided_by_tests);
        for test in tests {
            match table.layout().stored_test(&table.schema, test) {
                Some(test) => stored_tests.push(test),
                None => judged = true,
            }
        }
        // The entries of a leaf the pass accepts, each flagged when the filter must still judge it:
        // one whose value overflows the leaf, or whose record the pass could not read, which
        // presenting the row reports.
        const JUDGE: u32 = 1 << 31;
        let mut accepted: Vec<u32> = Vec::new();
        let mut positions = Vec::new();
        while cursor.next_leaf_from(&mut *self.pages.borrow_mut())? {
            let Some((leaf, start)) = cursor.leaf() else {
                break;
            };
            let count = leaf.len();
            if except.is_empty() {
                positions.clear();
            } else {
                cursor.leaf_positions(&mut except, &mut positions)?;
            }
            // Forward, the entries from `start` on; backward, those before it, last first.
            let remaining = if backward { start } else { count - start };
            accepted.clear();
            accepted.reserve(remaining);
            let (mut skipped, mut examined, mut ended) = (0, 0, false);
            let mut skip = positions.first().copied().unwrap_or(usize::MAX);
            let mut next = start;
            for _ in 0..remaining {
                let position = if backward {
                    next -= 1;
                    next
                } else {
                    next += 1;
                    next - 1
                };
                if position == skip {
                    skipped += 1;
                    skip = positions.get(skipped).copied().unwrap_or(usize::MAX);
                    continue;
                }
                let passes = match leaf.inline_leaf_cell(position) {
                    Some((key, value)) => {
                        if let Some(range) = range
                            && !range.contains(leading_key_component(key, key_type)?)
                        {
                            ended = true;
                            break;
                        }
                        stored_record_passes(value, &stored_tests)
                    }
                    None => None,
                };
                examined += 1;
                match passes {
                    Some(false) => {}
                    Some(true) => accepted.push(position as u32),
                    None => accepted.push(position as u32 | JUDGE),
                }
            }
            // Every row read is charged, passed over or not, before any of the leaf's rows is
            // presented, so that a scan over its budget presents none past it.
            if let Some(work) = work {
                charge_operations(work, examined + skipped)?;
            }
            scanned(examined)?;
            for &entry in &accepted {
                let position = (entry & !JUDGE) as usize;
                let cell;
                let (key, value): (&[u8], &[u8]) = match leaf.inline_leaf_cell(position) {
                    Some(cell) => cell,
                    None => {
                        cell = cursor.cell_at(position, &mut read)?;
                        if let Some(range) = range
                            && !range.contains(leading_key_component(cell.0, key_type)?)
                        {
                            return Ok(VisitOutcome::Complete);
                        }
                        (cell.0, &cell.1)
                    }
                };
                let row = RowRef::record(table.record(key, value)?);
                if (judged || entry & JUDGE != 0)
                    && let Some(filter) = filter
                    && !filter.matches(&row)?
                {
                    continue;
                }
                if visitor(&row)? == VisitControl::Stop {
                    return Ok(VisitOutcome::Stopped);
                }
            }
            if ended {
                return Ok(VisitOutcome::Complete);
            }
            cursor.skip_to(if backward { 0 } else { count });
        }
        Ok(VisitOutcome::Complete)
    }

    pub(crate) fn lookup_primary_key(&self, table: &str, key: &Row) -> Result<Option<Row>> {
        let table = self.table(table)?;
        let key = encode_primary_key(&table.schema, key)?;
        let Some(root) = table.root_page_id else {
            return Ok(None);
        };
        self.get(root, table.tree_id, &key)?
            .map(|value| table.record(&key, &value)?.to_row())
            .transpose()
    }

    /// Visits the row of the encoded primary key `key` in `table`, if it holds one.
    pub(crate) fn visit_encoded_key(
        &self,
        table: &str,
        key: &[u8],
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        let table = self.table(table)?;
        let Some(root) = table.root_page_id else {
            return Ok(VisitOutcome::Complete);
        };
        match self.get(root, table.tree_id, key)? {
            Some(value)
                if visitor(&RowRef::record(table.record(key, &value)?))? == VisitControl::Stop =>
            {
                Ok(VisitOutcome::Stopped)
            }
            _ => Ok(VisitOutcome::Complete),
        }
    }

    pub(crate) fn visit_key(
        &self,
        table: &str,
        key: PrimaryKey<'_>,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        let key = key.encode(&self.table(table)?.schema)?;
        self.visit_encoded_key(table, &key, visitor)
    }

    pub(crate) fn visit_index(
        &self,
        table: &str,
        columns: &[String],
        key: &Row,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<Option<VisitOutcome>> {
        let (table_data, Some(index)) = self.index(table, columns)? else {
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
        let table_root = indexed_table_root(table_data, index)?;
        let mut cursor = self.cursor(index_root, index.tree_id, Some(&prefix), false)?;
        loop {
            let next = cursor.next_from(&mut *self.pages.borrow_mut())?;
            let Some((entry_key, value)) = next else {
                break;
            };
            self.charge()?;
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
            let row_value = self
                .get(table_root, table_data.tree_id, primary_key)?
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

    pub(crate) fn visit_index_range(
        &self,
        table: &str,
        columns: &[String],
        range: &KeyRange,
        limit: usize,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<Option<VisitOutcome>> {
        let (table_data, Some(index)) = self.index(table, columns)? else {
            return Ok(None);
        };
        let Some(index_root) = index.root_page_id else {
            return Ok(Some(VisitOutcome::Complete));
        };
        let table_root = indexed_table_root(table_data, index)?;
        let types = table_data.column_types(columns)?;
        // A range is read through the index only while it holds at most `limit` entries, which a
        // first walk counts without copying any, charged as collecting them would be.
        let mut entries = 0usize;
        self.walk_index_range(index_root, index.tree_id, types[0], range, &mut |_, _| {
            entries += 1;
            if entries > limit {
                return Ok(VisitControl::Stop);
            }
            self.charge()?;
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
            let value = self
                .get(table_root, table_data.tree_id, primary_key)?
                .ok_or_else(|| dangling_index_entry(&index.definition))?;
            if visitor(&RowRef::record(table_data.record(primary_key, &value)?))?
                == VisitControl::Stop
            {
                return Ok(Some(VisitOutcome::Stopped));
            }
        }
        Ok(Some(VisitOutcome::Complete))
    }

    pub(crate) fn visit_index_entries(
        &self,
        table: &str,
        columns: &[String],
        range: &KeyRange,
        layout: &IndexEntryLayout,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<Option<VisitOutcome>> {
        let (table_data, Some(index)) = self.index(table, columns)? else {
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
                self.charge()?;
                visitor(&RowRef::index(IndexEntry::new(entry, layout)))
            },
        )
        .map(Some)
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
        let mut cursor = self.cursor(root, tree_id, Some(range.start()), false)?;
        let mut read = |page_id: PageId| self.pages.borrow_mut().read_btree_page(page_id);
        while cursor.next_leaf_from(&mut *self.pages.borrow_mut())? {
            while let Some((entry, value)) = cursor.next_in_leaf(&mut read)? {
                if !range.contains(leading_key_component(entry, leading)?) {
                    return Ok(VisitOutcome::Complete);
                }
                if each(entry, &value)? == VisitControl::Stop {
                    return Ok(VisitOutcome::Stopped);
                }
            }
        }
        Ok(VisitOutcome::Complete)
    }
}

/// The root of the table an index with entries indexes, which must have rows.
fn indexed_table_root(table: &PagedTable, index: &PagedIndex) -> Result<PageId> {
    table.root_page_id.ok_or_else(|| {
        storage_corrupt(format!(
            "Non-empty index `{}` references empty table `{}`",
            index.definition.name, table.schema.name
        ))
    })
}

/// A relational view over the crash-safe paged B-tree store.
///
/// Table definitions and row changes publish directly through the pager's atomic generation
/// switch. The broader SQL/DDL storage driver remains deliberately unavailable until every
/// operation can share that publication path.
pub(crate) struct PagedStorage<D: PageDevice> {
    pager: RefCell<Pager<D>>,
    revision: u64,
    next_tree_id: TreeId,
    schema_version: u64,
    // Shared with each script candidate, which copies them only if it changes the catalog.
    tables: Rc<NameMap<PagedTable>>,
    indexes: Rc<NameMap<PagedIndex>>,
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

    /// Where the table's columns live in its stored entries.
    pub(crate) fn layout(&self) -> &Rc<RecordLayout> {
        &self.layout
    }

    /// The type of the leading primary-key column, which orders this table's keys.
    pub(crate) fn leading_key_type(&self) -> ColumnType {
        self.layout.key_types()[0]
    }

    /// The types of `columns`, in the order given.
    pub(crate) fn column_types(&self, columns: &[String]) -> Result<Vec<ColumnType>> {
        let mut types = Vec::with_capacity(columns.len());
        for name in columns {
            types.push(
                self.schema
                    .columns
                    .iter()
                    .find(|column| column.name == *name)
                    .ok_or_else(|| EngineError::column_not_found(name, &self.schema.name))?
                    .data_type,
            );
        }
        Ok(types)
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
        // Built as `json!` would build it, without its unwrapping, which would link the
        // formatting of serde's errors.
        let value = |value: std::result::Result<Value, serde_json::Error>| {
            value.map_err(|error| storage_corrupt(format!("A schema cannot be encoded: {error}")))
        };
        let mut columns = serde_json::Map::new();
        columns.insert(
            "columns".to_owned(),
            value(serde_json::to_value(&schema.columns))?,
        );
        columns.insert(
            "primaryKey".to_owned(),
            value(serde_json::to_value(&schema.primary_key))?,
        );
        let columns = Value::Object(columns);
        Ok(Self {
            encoded: encode_catalog_schema(schema)?,
            columns_fingerprint: crate::checksum::xxh64(
                &crate::paged_codec::encode_canonical_json(&columns)?,
                0,
            ),
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

    /// This usage with `operations` more operations, failing as [`Self::plus`] does.
    pub(crate) fn with_operations(self, operations: usize) -> Result<Self> {
        self.plus(Self {
            operations,
            ..Self::default()
        })
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

/// A changed row as a write set measures it, with its [`crate::storage::estimated_row_bytes`]: a
/// map planning normalized, or for a delete its key, or the record an upsert was planned into.
#[derive(Clone, Copy)]
pub(crate) enum ChangeRow<'a> {
    Map(&'a Row, usize),
    Record(&'a [u8], usize),
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
type LoadedCatalog = (TreeId, u64, NameMap<PagedTable>, NameMap<PagedIndex>);

/// A catalog's table and index records, each list in name order, as the catalog holds them.
struct CatalogRecords {
    next_tree_id: TreeId,
    schema_version: u64,
    tables: Vec<(String, CatalogTableRecord)>,
    indexes: Vec<(String, CatalogIndexRecord)>,
}

impl<D: PageDevice> PagedStorage<D> {
    /// Opens a previously published paged database, checking its catalog.
    ///
    /// Rows and index entries are checked as they are read, not all at once here, so that opening
    /// takes the same time however large the database is; [`Self::check`] checks them all.
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
                schema_version: 0,
                tables: Rc::default(),
                indexes: Rc::default(),
                recovery_required: false,
                #[cfg(test)]
                validated_row_count: std::cell::Cell::new(0),
            });
        };

        let (next_tree_id, schema_version, tables, indexes) =
            load_catalog(&mut pager, catalog_root_page_id)?;
        Ok(Self {
            pager: RefCell::new(pager),
            revision,
            next_tree_id,
            schema_version,
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

    /// Checks every row and index entry of the committed database, and every page holding them.
    ///
    /// Each table's rows must decode and match its schema and row count, and each index must hold
    /// exactly one matching entry for each row with no NULL in its columns, with no duplicate
    /// values in a unique index. The first problem found is returned.
    pub(crate) fn check(&self) -> Result<()> {
        self.ensure_ready()?;
        let mut pager = self.pager.borrow_mut();
        let Some(catalog_root_page_id) = pager.catalog_root_page_id() else {
            return Ok(());
        };
        let records = read_catalog_records(&mut pager, catalog_root_page_id)?;
        check_catalog_contents(&mut pager, &records)
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
                self.schema_version,
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
            schema_version,
            tables,
            indexes,
            results,
        } = publication;
        if committed {
            self.revision = revision;
            self.next_tree_id = next_tree_id;
            self.schema_version = schema_version;
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
    #[cfg(test)]
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
                RowChange::Put { .. } | RowChange::Remove { .. } => {
                    unreachable!("the reference validates rows as maps")
                }
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

    /// The table `name`, as the committed catalog describes it.
    pub(crate) fn table(&self, name: &str) -> Result<&PagedTable> {
        self.tables
            .get(name)
            .ok_or_else(|| EngineError::table_not_found(name))
    }

    /// The committed row the encoded primary key `key` holds in `table_name`, as its stored entry.
    pub(crate) fn committed_entry(
        &self,
        table_name: &str,
        key: &[u8],
    ) -> Result<Option<StoredEntry>> {
        self.ensure_ready()?;
        let table = self.table(table_name)?;
        let Some(root) = table.root_page_id else {
            return Ok(None);
        };
        Btree::get(&mut self.pager.borrow_mut(), root, table.tree_id, key)?
            .map(|value| Ok(table.record(key, &value)?.to_entry()))
            .transpose()
    }

    /// Visits the committed rows of `table` in key order, as [`StorageReader::visit_table`] does,
    /// except those whose encoded keys `except` holds, in ascending order, charging each row to
    /// `work`, visited or not. Only a leaf that could hold one of `except` looks for it, once, so
    /// that a scan passing over the rows a transaction replaced compares no row with their keys.
    /// Plain scans keep to [`StorageReader::visit_table`], whose loop does nothing else.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn visit_rows_where(
        &self,
        table: &str,
        range: Option<&KeyRange>,
        order: KeyOrder,
        except: &[&[u8]],
        work: Option<&Cell<usize>>,
        filter: Option<&Filter<'_>>,
        scanned: &mut dyn FnMut(usize) -> Result<()>,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.ensure_ready()?;
        self.reader_charging(work)
            .visit_rows_where(table, range, order, except, filter, scanned, visitor)
    }

    /// The reader of the committed catalog's B-trees.
    pub(crate) fn reader(&self) -> TreeReader<'_> {
        self.reader_charging(None)
    }

    /// The reader of the committed catalog's B-trees, charging each row it reads to `work`.
    fn reader_charging<'a>(&'a self, work: Option<&'a Cell<usize>>) -> TreeReader<'a> {
        TreeReader {
            pages: &self.pager,
            tables: &self.tables,
            indexes: &self.indexes,
            work,
        }
    }

    pub(crate) fn visit_encoded_key(
        &self,
        table: &str,
        encoded_key: &[u8],
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.ensure_ready()?;
        self.reader().visit_encoded_key(table, encoded_key, visitor)
    }

    /// Validates one change of a transaction's write set, and measures what it adds to the write
    /// set's usage exactly as [`Self::validate_row_write_set`] does for each change: an upsert of
    /// `row`, which planning normalized, or where `is_delete`, a delete of the key `row`, with the
    /// row's [`crate::storage::estimated_row_bytes`]. `key` is the row's encoded primary key, and
    /// `base` the committed row the change replaces. The catalog operations charged once per
    /// changed table are left to [`Self::write_set_usage`], and conflicts between claims to the
    /// caller.
    #[cfg(test)]
    pub(crate) fn change_cost(
        &self,
        table_name: &str,
        row: ChangeRow<'_>,
        is_delete: bool,
        key: &[u8],
        base: Option<&HeldRow>,
    ) -> Result<ChangeCost> {
        let table = self
            .tables
            .get(table_name)
            .ok_or_else(|| EngineError::table_not_found(table_name))?;
        let indexes = self.table_indexes(table_name);
        self.change_cost_in(table, &indexes, row, is_delete, key, base)
    }

    /// The indexes on `table`, which a statement changing many rows finds once for all of them.
    pub(crate) fn table_indexes(&self, table: &str) -> Vec<&PagedIndex> {
        self.indexes
            .values()
            .filter(|index| index.definition.table == table)
            .collect()
    }

    /// [`Self::change_cost`] for a change to `table`, whose indexes are `indexes`.
    pub(crate) fn change_cost_in(
        &self,
        table: &PagedTable,
        indexes: &[&PagedIndex],
        row: ChangeRow<'_>,
        is_delete: bool,
        key: &[u8],
        base: Option<&HeldRow>,
    ) -> Result<ChangeCost> {
        self.ensure_ready()?;
        #[cfg(test)]
        self.validated_row_count
            .set(self.validated_row_count.get() + 1);
        let table_name = table.schema.name.as_str();
        // A table without indexes, as most are, borrows an empty list.
        let definitions = indexes
            .iter()
            .map(|index| &index.definition)
            .collect::<Vec<_>>();
        let record = match row {
            ChangeRow::Record(value, _) => Some(table.record(key, value)?),
            ChangeRow::Map(..) => None,
        };
        let row_write = match (row, &record) {
            (ChangeRow::Map(row, bytes), _) => preflight_row_write(
                table_name,
                &table.schema,
                (row, bytes),
                is_delete,
                &definitions,
                RowWriteUsage::default(),
            )?,
            (ChangeRow::Record(_, bytes), Some(record)) => preflight_record_write(
                table_name,
                record,
                bytes,
                &definitions,
                RowWriteUsage::default(),
            )?,
            (ChangeRow::Record(..), None) => unreachable!("a record change opened its record"),
        };
        let row_bytes = match (row, &record) {
            (ChangeRow::Map(row, _), _) => estimated_row_bytes(row)?,
            (_, Some(record)) => estimated_record_batch_bytes(record)?,
            (ChangeRow::Record(..), None) => unreachable!("a record change opened its record"),
        };
        let input_bytes = table_name
            .len()
            .checked_add(row_bytes)
            .and_then(|bytes| bytes.checked_add(64))
            .ok_or_else(batch_too_large)?;
        let base_bytes = match base {
            None => 0,
            Some(HeldRow::Map(base)) => estimated_row_bytes(base)?,
            Some(HeldRow::Stored(base)) => {
                estimated_record_batch_bytes(&table.record(base.key(), base.value())?)?
            }
        };
        let next_bytes = if is_delete { 0 } else { row_bytes };
        let mut prepared_bytes = key
            .len()
            .checked_add(base_bytes)
            .and_then(|bytes| bytes.checked_add(next_bytes))
            .and_then(|bytes| bytes.checked_add(96))
            .ok_or_else(batch_too_large)?;
        let mut claims = Vec::new();
        if !is_delete {
            for index in indexes.iter().filter(|index| index.definition.unique) {
                let prefix = match (row, &record) {
                    (ChangeRow::Map(row, _), _) => {
                        encode_secondary_index_prefix(&table.schema, &index.definition, row)?
                    }
                    (_, Some(record)) => encode_record_index_prefix(
                        &index_column_positions(&table.schema, &index.definition)?,
                        record,
                    )?,
                    (ChangeRow::Record(..), None) => {
                        unreachable!("a record change opened its record")
                    }
                };
                let Some(prefix) = prefix else {
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
        row: &HeldRow,
    ) -> Result<Vec<(TreeId, Vec<u8>)>> {
        let table = self.table(table_name)?;
        let mut values = Vec::new();
        for index in self
            .indexes
            .values()
            .filter(|index| index.definition.table == table_name && index.definition.unique)
        {
            let prefix = match row {
                HeldRow::Map(row) => {
                    encode_secondary_index_prefix(&table.schema, &index.definition, row)?
                }
                HeldRow::Stored(entry) => encode_record_index_prefix(
                    &index_column_positions(&table.schema, &index.definition)?,
                    &table.record(entry.key(), entry.value())?,
                )?,
            };
            values.extend(prefix.map(|prefix| (index.tree_id, prefix)));
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
    #[cfg(test)]
    pub(crate) fn write_set_usage<'t>(
        &self,
        changes: PagedWriteUsage,
        changed_tables: impl Iterator<Item = &'t str>,
    ) -> Result<PagedWriteUsage> {
        let mut operations = 0usize;
        for table in changed_tables {
            operations = operations
                .checked_add(self.catalog_operations(table))
                .ok_or_else(batch_too_large)?;
        }
        changes.with_operations(operations)
    }

    /// The operations a write set is charged once for each table it changes: the table's own
    /// catalog entry, and one for each of its indexes.
    /// The version an application last gave the schema, or zero.
    pub(crate) fn schema_version(&self) -> u64 {
        self.schema_version
    }

    /// Every table, in name order, with the indexes on it, also in name order.
    pub(crate) fn schema(&self) -> Result<Vec<(Rc<TableDefinition>, Vec<IndexDefinition>)>> {
        self.ensure_ready()?;
        let mut tables = Vec::with_capacity(self.tables.len());
        for table in self.tables.values() {
            let mut indexes = Vec::new();
            for index in self.indexes.values() {
                if index.definition.table == table.schema.name {
                    indexes.push(index.definition.clone());
                }
            }
            tables.push((Rc::clone(&table.schema), indexes));
        }
        Ok(tables)
    }

    pub(crate) fn catalog_operations(&self, table: &str) -> usize {
        1 + self
            .indexes
            .values()
            .filter(|index| index.definition.table == table)
            .count()
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
                RowChange::Put { .. } | RowChange::Remove { .. } => {
                    unreachable!("the reference validates rows as maps")
                }
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
        let keys = transaction.changed_keys(self)?;
        let publication = {
            let mut pager = self.pager.borrow_mut();
            crate::paged_script::execute_changed_rows(
                &mut pager,
                self.revision,
                self.next_tree_id,
                self.schema_version,
                self.tables.clone(),
                self.indexes.clone(),
                &changes,
            )
        };
        let (revision, _) = self.accept_script_publication(publication)?;
        Ok(ApplyOutcome {
            revision,
            tables: transaction.touched_tables().into_iter().collect(),
            keys,
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
    tables: &NameMap<PagedTable>,
    indexes: &NameMap<PagedIndex>,
) -> Result<PagedWriteUsage> {
    let definitions = indexes
        .values()
        .map(|index| &index.definition)
        .collect::<Vec<_>>();
    let mut usage = PagedWriteUsage::default();
    usage.row_write = preflight_row_write_set(
        changes,
        &|name| {
            let table = tables.get(name)?;
            Some((&*table.schema, Some(&**table.layout())))
        },
        &definitions,
        usage.row_write,
    )?;
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
            RowChange::Put { .. } | RowChange::Remove { .. } => {
                unreachable!("the reference validates rows as maps")
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

/// [`estimated_row_bytes`] for the row a stored record decodes to, reading its columns in place.
fn estimated_record_batch_bytes(record: &StoredRecord<'_>) -> Result<usize> {
    let mut bytes = 64usize;
    for (position, column) in record.schema().columns.iter().enumerate() {
        let value_bytes = if column.data_type == ColumnType::Text {
            record
                .text_len(position)?
                .map_or(Ok(8), estimated_string_bytes)?
        } else {
            match record.column(position)? {
                ValueRef::Null | ValueRef::Boolean(_) => 8,
                ValueRef::Integer(_) | ValueRef::Float(_) => 32,
                ValueRef::Text(text) => estimated_string_bytes(text.len())?,
                ValueRef::Json(value) => estimated_json_bytes(&value, 1)?,
            }
        };
        bytes = column
            .name
            .len()
            .checked_mul(6)
            .and_then(|key_bytes| bytes.checked_add(key_bytes))
            .and_then(|bytes| bytes.checked_add(value_bytes))
            .and_then(|bytes| bytes.checked_add(64))
            .ok_or_else(batch_too_large)?;
    }
    Ok(bytes)
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
        serde_json::Value::String(value) => estimated_string_bytes(value.len()),
        serde_json::Value::Array(values) => values.iter().try_fold(64usize, |bytes, value| {
            bytes
                .checked_add(estimated_json_bytes(value, depth + 1)?)
                .and_then(|bytes| bytes.checked_add(16))
                .ok_or_else(batch_too_large)
        }),
        serde_json::Value::Object(values) => estimated_object_bytes(values, depth),
    }
}

/// A string of `length` bytes, escaped as JSON at worst.
fn estimated_string_bytes(length: usize) -> Result<usize> {
    length
        .checked_mul(6)
        .and_then(|bytes| bytes.checked_add(32))
        .ok_or_else(batch_too_large)
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

    fn tables_with_foreign_keys(&self) -> Vec<Rc<TableDefinition>> {
        tables_with_foreign_keys(&self.tables)
    }

    fn visit_table(
        &self,
        table: &str,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.ensure_ready()?;
        self.reader().visit_table(table, visitor)
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
        self.reader()
            .visit_table_range(table, range, order, visitor)
    }

    fn visit_table_where(
        &self,
        table: &str,
        filter: &Filter<'_>,
        scanned: &mut dyn FnMut(usize) -> Result<()>,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.visit_rows_where(
            table,
            None,
            KeyOrder::Ascending,
            &[],
            None,
            Some(filter),
            scanned,
            visitor,
        )
    }

    fn visit_table_range_where(
        &self,
        table: &str,
        range: &KeyRange,
        order: KeyOrder,
        filter: &Filter<'_>,
        scanned: &mut dyn FnMut(usize) -> Result<()>,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.visit_rows_where(
            table,
            Some(range),
            order,
            &[],
            None,
            Some(filter),
            scanned,
            visitor,
        )
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
        self.reader().lookup_primary_key(table, key)
    }

    fn visit_primary_key(
        &self,
        table: &str,
        key: &Row,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.ensure_ready()?;
        self.reader()
            .visit_key(table, PrimaryKey::Row(key), visitor)
    }

    fn visit_primary_key_values(
        &self,
        table: &str,
        _schema: &TableDefinition,
        values: &[&Value],
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.ensure_ready()?;
        self.reader()
            .visit_key(table, PrimaryKey::Values(values), visitor)
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
        self.reader().visit_index(table, columns, key, visitor)
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
        self.reader()
            .visit_index_range(table, columns, range, limit, visitor)
    }

    fn visit_index_entries(
        &self,
        table: &str,
        columns: &[String],
        range: &KeyRange,
        layout: &IndexEntryLayout,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<Option<VisitOutcome>> {
        self.ensure_ready()?;
        self.reader()
            .visit_index_entries(table, columns, range, layout, visitor)
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

/// Loads the catalog of a database being opened, checking its records against each other.
fn load_catalog<D: PageDevice>(
    pager: &mut Pager<D>,
    catalog_root_page_id: PageId,
) -> Result<LoadedCatalog> {
    let CatalogRecords {
        next_tree_id,
        schema_version,
        tables: table_records,
        indexes: index_records,
    } = read_catalog_records(pager, catalog_root_page_id)?;
    let mut tables = NameMap::new();
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
    let mut indexes = NameMap::new();
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
    Ok((next_tree_id, schema_version, tables, indexes))
}

/// Reads every catalog record, and checks the records against each other: one header, whose
/// counts match the records, unique tree IDs within its range, and each index on columns its table
/// has.
fn read_catalog_records<D: PageDevice>(
    pager: &mut Pager<D>,
    catalog_root_page_id: PageId,
) -> Result<CatalogRecords> {
    let mut header = None;
    // Catalog keys arrive in order, so each list is in name order, and a name listed twice would
    // follow itself.
    let mut table_records: Vec<(String, CatalogTableRecord)> = Vec::new();
    let mut index_records: Vec<(String, CatalogIndexRecord)> = Vec::new();
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
                if table_records.last().is_some_and(|(last, _)| *last >= name) {
                    return Err(storage_corrupt(format!(
                        "The catalog lists table `{name}` out of order or more than once"
                    )));
                }
                table_records.push((name, record));
            }
            CatalogKey::Index(name) => {
                let record = decode_catalog_index_record(&key, &value)?;
                if index_records.last().is_some_and(|(last, _)| *last >= name) {
                    return Err(storage_corrupt(format!(
                        "The catalog lists index `{name}` out of order or more than once"
                    )));
                }
                index_records.push((name, record));
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
    for (_, record) in &table_records {
        validate_catalog_tree_id(record.tree_id, header.next_tree_id, &mut tree_ids)?;
    }
    for (_, record) in &index_records {
        validate_catalog_tree_id(record.tree_id, header.next_tree_id, &mut tree_ids)?;
        validate_index_against_catalog(&record.definition, &table_records)?;
    }
    if tree_ids.len() != 1 + table_records.len() + index_records.len() {
        return Err(storage_corrupt("Catalog tree IDs are not unique"));
    }
    Ok(CatalogRecords {
        next_tree_id: header.next_tree_id,
        schema_version: header.schema_version,
        tables: table_records,
        indexes: index_records,
    })
}

/// Checks every table's rows and every index's entries against the catalog records.
fn check_catalog_contents<D: PageDevice>(
    pager: &mut Pager<D>,
    records: &CatalogRecords,
) -> Result<()> {
    // Each table's rows are validated in one pass, which also counts, for each index on the
    // table, the rows that should have an entry in it.
    let mut expected_entry_counts = vec![0; records.indexes.len()];
    for (name, record) in &records.tables {
        let mut indexes = Vec::new();
        for (index, (_, index_record)) in records.indexes.iter().enumerate() {
            if index_record.definition.table == *name {
                indexes.push((
                    index_column_positions(&record.schema, &index_record.definition)?,
                    index,
                ));
            }
        }
        validate_table_tree(pager, record, &indexes, &mut expected_entry_counts)?;
    }
    for ((_, record), expected_entry_count) in records.indexes.iter().zip(expected_entry_counts) {
        validate_index_tree(pager, record, &records.tables, expected_entry_count)?;
    }
    Ok(())
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

/// The catalog record of the table `name`, from records in name order.
fn catalog_table<'a>(
    tables: &'a [(String, CatalogTableRecord)],
    name: &str,
) -> Option<&'a CatalogTableRecord> {
    let position = tables.partition_point(|(table, _)| table.as_str() < name);
    tables
        .get(position)
        .filter(|(table, _)| table == name)
        .map(|(_, record)| record)
}

fn validate_index_against_catalog(
    definition: &IndexDefinition,
    tables: &[(String, CatalogTableRecord)],
) -> Result<()> {
    let table = catalog_table(tables, &definition.table).ok_or_else(|| {
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

/// Validates every row of a table, and counts the rows that should have an entry in each index
/// on it: in `expected_entry_counts`, at the place given with the positions of its columns.
fn validate_table_tree<D: PageDevice>(
    pager: &mut Pager<D>,
    record: &CatalogTableRecord,
    indexes: &[(Vec<usize>, usize)],
    expected_entry_counts: &mut [u64],
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
        if !indexes.is_empty() {
            let row = StoredRecord::new(&record.schema, &layout, key, &value)?;
            for (positions, index) in indexes {
                if has_index_entry(&row, positions)? {
                    expected_entry_counts[*index] += 1;
                }
            }
        }
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

/// Whether a stored row has an entry in the index on the columns at `positions`, as
/// [`encode_record_index_entry`] finds: whether none of them is NULL.
fn has_index_entry(row: &StoredRecord<'_>, positions: &[usize]) -> Result<bool> {
    for position in positions {
        if row.column(*position)?.is_null() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn validate_index_tree<D: PageDevice>(
    pager: &mut Pager<D>,
    record: &CatalogIndexRecord,
    tables: &[(String, CatalogTableRecord)],
    expected_entry_count: u64,
) -> Result<()> {
    let table = catalog_table(tables, &record.definition.table).ok_or_else(|| {
        storage_corrupt(format!(
            "Index `{}` references missing table `{}`",
            record.definition.name, record.definition.table
        ))
    })?;
    if record.entry_count != expected_entry_count {
        return Err(storage_corrupt(format!(
            "Index `{}` catalog count {} does not match its {expected_entry_count} eligible table rows",
            record.definition.name, record.entry_count
        )));
    }
    let Some(root) = record.root_page_id else {
        return if record.entry_count == 0 {
            Ok(())
        } else {
            Err(storage_corrupt(format!(
                "Index `{}` claims {} entries without a tree root",
                record.definition.name, record.entry_count
            )))
        };
    };
    let table_root = table.root_page_id.ok_or_else(|| {
        storage_corrupt(format!(
            "Non-empty index `{}` references an empty table",
            record.definition.name
        ))
    })?;
    let layout = RecordLayout::new(&table.schema)?;
    let positions = index_column_positions(&table.schema, &record.definition)?;
    let mut count = 0_u64;
    let mut unique_prefixes = KeySet::default();
    let mut cursor = Btree::validating_cursor(pager, root, record.tree_id)?;
    while let Some((entry_key, value)) = cursor.next_entry(pager)? {
        if !value.is_empty() {
            return Err(storage_corrupt(format!(
                "Secondary index `{}` contains a non-empty value",
                record.definition.name
            )));
        }
        let primary_key = secondary_index_primary_key_for_definition(
            &table.schema,
            &record.definition,
            entry_key,
        )?;
        let row_value =
            Btree::get(pager, table_root, table.tree_id, primary_key)?.ok_or_else(|| {
                storage_corrupt(format!(
                    "Secondary index `{}` contains a dangling primary key",
                    record.definition.name
                ))
            })?;
        // Every table row was validated strictly before its indexes are checked, so its entry is
        // read from its record, as writers read it. A row with a NULL in the tuple has none.
        let row = StoredRecord::new(&table.schema, &layout, primary_key, &row_value)?;
        let Some((expected, tuple)) = encode_record_index_entry(&positions, &row)?
            .filter(|(expected, _)| expected.as_slice() == entry_key)
        else {
            return Err(storage_corrupt(format!(
                "Secondary index `{}` entry does not match its table row",
                record.definition.name
            )));
        };
        if record.definition.unique {
            let mut prefix = expected;
            prefix.truncate(tuple);
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

/// Every table of a catalog that has a foreign key.
pub(crate) fn tables_with_foreign_keys(tables: &NameMap<PagedTable>) -> Vec<Rc<TableDefinition>> {
    tables
        .values()
        .filter(|table| !table.schema.foreign_keys.is_empty())
        .map(|table| Rc::clone(&table.schema))
        .collect()
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
                    max_length: None,
                })
                .collect(),
            foreign_keys: vec![],
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
            let RowChange::Upsert { row, .. } = change else {
                unreachable!()
            };
            let key = encode_primary_key(&storage.table(table).unwrap().schema, row).unwrap();
            let row = (row, crate::storage::estimated_row_bytes(row).unwrap());
            let cost = storage
                .change_cost(table, ChangeRow::Map(row.0, row.1), false, &key, None)
                .unwrap();
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
        let RowChange::Upsert { row, .. } = &change else {
            unreachable!()
        };
        let key = encode_primary_key(&storage.table("items").unwrap().schema, row).unwrap();
        let row = (row, crate::storage::estimated_row_bytes(row).unwrap());
        let addition = storage
            .change_cost("items", ChangeRow::Map(row.0, row.1), false, &key, None)
            .unwrap()
            .usage;
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
        reopened.check().unwrap();
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
            validate_table_tree(&mut pager, &table, &[], &mut [])
                .unwrap_err()
                .code,
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
            validate_index_tree(&mut pager, &index, &[("missing_rows".to_owned(), table)], 1)
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
        reopened.check().unwrap();
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
            schema_version: 0,
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
                    schema_version: 0,
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

    /// Opens a database whose catalog is sound, and returns the problem its full check finds.
    fn check_error(device: MemoryPageDevice) -> EngineError {
        let storage = PagedStorage::open(device).unwrap();
        storage.check().unwrap_err()
    }

    #[test]
    fn checking_rejects_an_index_missing_eligible_table_rows() {
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

            let error = check_error(pager.into_device());
            assert_eq!(error.code, "STORAGE_CORRUPT");
        }
    }

    #[test]
    fn checking_counts_required_and_nullable_index_entries() {
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
        reopened.check().unwrap();
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
    fn checking_rejects_null_in_a_required_index_column() {
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
        let error = check_error(pager.into_device());
        assert_eq!(error.code, "STORAGE_CORRUPT");
    }

    #[test]
    fn checking_rejects_duplicate_required_unique_index_prefixes() {
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
        let error = check_error(pager.into_device());
        assert_eq!(error.code, "STORAGE_CORRUPT");
    }

    #[test]
    fn checking_rejects_a_required_index_entry_mismatching_its_row() {
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
        let error = check_error(pager.into_device());
        assert_eq!(error.code, "STORAGE_CORRUPT");
    }
}
