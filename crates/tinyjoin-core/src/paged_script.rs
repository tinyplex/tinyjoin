use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
};

use crate::{
    Btree, EngineError, ExecuteResult, PageDevice, PageId, Pager, PagerWriteTransaction,
    QueryResult, Result, Row, RowChange, StorageReader, TreeId, VisitControl, VisitOutcome,
    hash::{EMPTY_HASH, combine, identify},
    paged_codec::{
        CATALOG_TREE_ID, CatalogHeader, CatalogIndexRecord, CatalogTableRecord,
        MAX_CATALOG_INDEXES, MAX_CATALOG_TABLES, MAX_TREE_ID, encode_catalog_header_record,
        encode_catalog_index_key, encode_catalog_index_record, encode_catalog_table_key,
        encode_catalog_table_record, encode_primary_key, encode_row,
        encode_secondary_index_entry_key, encode_secondary_index_prefix,
        secondary_index_entry_matches_prefix, secondary_index_primary_key,
        secondary_index_primary_key_for_definition,
    },
    paged_storage::{
        PagedIndex, PagedTable, adjusted_count, batch_too_large, ensure_batch_bytes, limit_error,
        storage_corrupt, unique_violation, validated_row,
    },
    statement::{PlannedDml, Statement, WriteStatement},
    storage::{
        estimated_row_bytes, normalize_row, preflight_row_write_set, schema_with_added_column,
        validate_schema,
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
    pub(crate) tables: BTreeMap<String, PagedTable>,
    pub(crate) indexes: BTreeMap<String, PagedIndex>,
    pub(crate) results: Vec<ExecuteResult>,
}

struct ScriptRowChange {
    old: Option<Row>,
    next: Option<Row>,
}

struct PagedScriptCandidate<'a, D: PageDevice> {
    transaction: RefCell<PagerWriteTransaction<'a, D>>,
    catalog_root: Option<PageId>,
    base_revision: u64,
    next_tree_id: TreeId,
    tables: BTreeMap<String, PagedTable>,
    indexes: BTreeMap<String, PagedIndex>,
    base_tables: BTreeSet<String>,
    base_indexes: BTreeSet<String>,
    mutated: bool,
    operations: Cell<usize>,
    result_bytes: usize,
}

pub(crate) fn execute<D: PageDevice>(
    pager: &mut Pager<D>,
    base_revision: u64,
    next_tree_id: TreeId,
    tables: BTreeMap<String, PagedTable>,
    indexes: BTreeMap<String, PagedIndex>,
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

pub(crate) fn execute_row_changes<D: PageDevice>(
    pager: &mut Pager<D>,
    base_revision: u64,
    next_tree_id: TreeId,
    tables: BTreeMap<String, PagedTable>,
    indexes: BTreeMap<String, PagedIndex>,
    changes: &[RowChange],
) -> Result<ScriptPublication> {
    let mut candidate = begin_candidate(pager, base_revision, next_tree_id, tables, indexes)?;
    let execution = (|| {
        candidate.apply_changes(changes)?;
        candidate.mutated = true;
        Ok(vec![])
    })();
    finish_candidate(candidate, execution)
}

fn begin_candidate<'a, D: PageDevice>(
    pager: &'a mut Pager<D>,
    base_revision: u64,
    next_tree_id: TreeId,
    tables: BTreeMap<String, PagedTable>,
    indexes: BTreeMap<String, PagedIndex>,
) -> Result<PagedScriptCandidate<'a, D>> {
    let catalog_root = pager.catalog_root_page_id();
    let transaction = pager.begin_write()?;
    Ok(PagedScriptCandidate {
        transaction: RefCell::new(transaction),
        catalog_root,
        base_revision,
        next_tree_id,
        base_tables: tables.keys().cloned().collect(),
        base_indexes: indexes.keys().cloned().collect(),
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
                    self.tables.insert(
                        schema.name.clone(),
                        PagedTable {
                            schema: schema.clone(),
                            tree_id,
                            root_page_id: None,
                            row_count: 0,
                            hash: EMPTY_HASH,
                        },
                    );
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
                let PlannedDml { outcome, changes } = crate::statement::plan_dml(self, statement)?;
                if outcome.mutated {
                    self.apply_changes(&changes)?;
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
        let mut root_page_id = None;
        let mut entry_count = 0usize;
        if let Some(table_root) = table.root_page_id {
            let mut transaction = self.transaction.borrow_mut();
            let mut rows =
                Btree::cursor_in_transaction(&mut transaction, table_root, table.tree_id)?;
            while let Some((primary_key, value)) = rows.next_in_transaction(&mut transaction)? {
                self.charge_operations(1)?;
                let row = validated_row(&table.schema, &primary_key, &value)?;
                let Some(index_key) =
                    encode_secondary_index_entry_key(&table.schema, definition, &row)?
                else {
                    continue;
                };
                if definition.unique
                    && let Some(prefix) =
                        encode_secondary_index_prefix(&table.schema, definition, &row)?
                    && let Some(root) = root_page_id
                {
                    let mut existing = Btree::cursor_from_in_transaction(
                        &mut transaction,
                        root,
                        tree_id,
                        &prefix,
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
                        if secondary_index_entry_matches_prefix(&existing_key, &prefix) {
                            return Err(unique_violation(&definition.name));
                        }
                    }
                }
                let root = match root_page_id {
                    Some(root) => root,
                    None => Btree::create(&mut transaction, tree_id)?,
                };
                root_page_id = Some(
                    Btree::upsert(&mut transaction, root, tree_id, &index_key, &[])?.root_page_id,
                );
                entry_count = entry_count.checked_add(1).ok_or_else(batch_too_large)?;
            }
        }
        self.indexes.insert(
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
        let index = self.indexes.remove(name).ok_or_else(|| {
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
        let table = self
            .tables
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
            .cloned()
            .ok_or_else(|| EngineError::table_not_found(table_name))?;
        let schema = schema_with_added_column(&table.schema, column)?;
        let Some(old_root) = table.root_page_id else {
            self.tables
                .get_mut(table_name)
                .expect("the altered table was resolved above")
                .schema = schema;
            return Ok(());
        };

        let new_tree_id = self.allocate_tree_id()?;
        let mut new_root = None;
        let mut new_hash = EMPTY_HASH;
        let mut rewritten = 0usize;
        {
            let mut transaction = self.transaction.borrow_mut();
            let mut rows = Btree::cursor_in_transaction(&mut transaction, old_root, table.tree_id)?;
            while let Some((primary_key, value)) = rows.next_in_transaction(&mut transaction)? {
                self.charge_operations(2)?;
                let mut row = validated_row(&table.schema, &primary_key, &value)?;
                row.insert(
                    column.name.clone(),
                    column.default.clone().unwrap_or(serde_json::Value::Null),
                );
                let row = normalize_row(&schema, row)?;
                let next_key = encode_primary_key(&schema, &row)?;
                if next_key != primary_key {
                    return Err(storage_corrupt(format!(
                        "ALTER TABLE changed a primary key in `{table_name}`"
                    )));
                }
                let root = match new_root {
                    Some(root) => root,
                    None => Btree::create(&mut transaction, new_tree_id)?,
                };
                let upserted = Btree::upsert(
                    &mut transaction,
                    root,
                    new_tree_id,
                    &primary_key,
                    &encode_row(&row)?,
                )?;
                new_root = Some(upserted.root_page_id);
                new_hash = upserted.hash;
                rewritten = rewritten.checked_add(1).ok_or_else(batch_too_large)?;
            }
            if rewritten != table.row_count {
                return Err(storage_corrupt(format!(
                    "Table `{table_name}` catalog count {} does not match {rewritten} rewritten rows",
                    table.row_count
                )));
            }
            Btree::reclaim(&mut transaction, old_root, table.tree_id)?;
        }
        let table = self
            .tables
            .get_mut(table_name)
            .expect("the altered table was resolved above");
        table.schema = schema;
        table.tree_id = new_tree_id;
        table.root_page_id = new_root;
        // Adding a column rewrites every row, so the table's rows have genuinely changed even
        // though no statement touched them.
        table.hash = new_hash;
        Ok(())
    }

    fn apply_changes(&mut self, input_changes: &[RowChange]) -> Result<()> {
        let schemas = self
            .tables
            .iter()
            .map(|(name, table)| (name.as_str(), &table.schema))
            .collect();
        let definitions = self
            .indexes
            .values()
            .map(|index| &index.definition)
            .collect::<Vec<_>>();
        preflight_row_write_set(input_changes, &schemas, &definitions, Default::default())?;
        for change in input_changes {
            let table = match change {
                RowChange::Upsert { table, .. } | RowChange::Delete { table, .. } => table,
            };
            let index_count = self
                .indexes
                .values()
                .filter(|index| index.definition.table == *table)
                .count();
            self.charge_operations(index_count.saturating_mul(2).saturating_add(1))?;
        }

        let mut duplicate_upserts = BTreeMap::<String, BTreeSet<Vec<u8>>>::new();
        let mut retained_bytes = 0usize;
        let mut changes = BTreeMap::<String, BTreeMap<Vec<u8>, ScriptRowChange>>::new();
        for change in input_changes {
            let (table_name, input, is_delete) = match change {
                RowChange::Upsert { table, row } => (table, row, false),
                RowChange::Delete { table, key } => (table, key, true),
            };
            let table = self
                .tables
                .get(table_name)
                .ok_or_else(|| EngineError::table_not_found(table_name))?;
            let row = if is_delete {
                input.clone()
            } else {
                normalize_row(&table.schema, input.clone())?
            };
            let key = encode_primary_key(&table.schema, &row)?;
            if !is_delete
                && !duplicate_upserts
                    .entry(table_name.clone())
                    .or_default()
                    .insert(key.clone())
            {
                return Err(EngineError::constraint_violation(format!(
                    "SQL statement would write canonical primary key in `{table_name}` more than once"
                )));
            }
            let next = (!is_delete).then_some(row);
            let table_changes = changes.entry(table_name.clone()).or_default();
            if let Some(existing) = table_changes.get_mut(&key) {
                let previous_bytes = existing.next.as_ref().map_or(Ok(0), estimated_row_bytes)?;
                let next_bytes = next.as_ref().map_or(Ok(0), estimated_row_bytes)?;
                retained_bytes = retained_bytes
                    .checked_sub(previous_bytes)
                    .and_then(|bytes| bytes.checked_add(next_bytes))
                    .ok_or_else(batch_too_large)?;
                ensure_batch_bytes(retained_bytes)?;
                existing.next = next;
                continue;
            }
            let old = self.lookup_encoded_primary_key(table_name, &key)?;
            let old_bytes = old.as_ref().map_or(Ok(0), estimated_row_bytes)?;
            let next_bytes = next.as_ref().map_or(Ok(0), estimated_row_bytes)?;
            retained_bytes = retained_bytes
                .checked_add(key.len())
                .and_then(|bytes| bytes.checked_add(old_bytes))
                .and_then(|bytes| bytes.checked_add(next_bytes))
                .and_then(|bytes| bytes.checked_add(96))
                .ok_or_else(batch_too_large)?;
            ensure_batch_bytes(retained_bytes)?;
            table_changes.insert(key, ScriptRowChange { old, next });
        }
        self.validate_changed_unique_indexes(&changes, retained_bytes)?;
        self.apply_row_changes(&changes)
    }

    fn validate_changed_unique_indexes(
        &self,
        changes: &BTreeMap<String, BTreeMap<Vec<u8>, ScriptRowChange>>,
        mut retained_bytes: usize,
    ) -> Result<()> {
        for (table_name, table_changes) in changes {
            let table = &self.tables[table_name];
            for index in self
                .indexes
                .values()
                .filter(|index| index.definition.table == *table_name && index.definition.unique)
            {
                let mut changed_prefixes = BTreeMap::<Vec<u8>, &[u8]>::new();
                for (primary_key, change) in table_changes {
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
                    for existing_primary_key in self.index_primary_keys(index, &prefix)? {
                        retained_bytes = retained_bytes
                            .checked_add(existing_primary_key.len())
                            .ok_or_else(batch_too_large)?;
                        ensure_batch_bytes(retained_bytes)?;
                        if existing_primary_key == *primary_key {
                            continue;
                        }
                        let existing_moves = match table_changes.get(&existing_primary_key) {
                            Some(existing) => match &existing.next {
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
                            },
                            None => false,
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

    fn apply_row_changes(
        &mut self,
        changes: &BTreeMap<String, BTreeMap<Vec<u8>, ScriptRowChange>>,
    ) -> Result<()> {
        for (table_name, table_changes) in changes {
            let table = self
                .tables
                .get_mut(table_name)
                .expect("every changed table was resolved above");
            let mut transaction = self.transaction.borrow_mut();
            for (key, change) in table_changes {
                match &change.next {
                    Some(row) => {
                        let root = match table.root_page_id {
                            Some(root) => root,
                            None => Btree::create(&mut transaction, table.tree_id)?,
                        };
                        let upserted = Btree::upsert(
                            &mut transaction,
                            root,
                            table.tree_id,
                            key,
                            &encode_row(row)?,
                        )?;
                        table.root_page_id = Some(upserted.root_page_id);
                        table.hash = upserted.hash;
                    }
                    None => {
                        if let Some(root) = table.root_page_id {
                            let deleted =
                                Btree::delete(&mut transaction, root, table.tree_id, key)?;
                            table.root_page_id = deleted.root_page_id;
                            if let Some(hash) = deleted.hash {
                                table.hash = hash;
                            }
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

            for index in self
                .indexes
                .values_mut()
                .filter(|index| index.definition.table == *table_name)
            {
                let mut inserted = 0usize;
                let mut deleted = 0usize;
                for change in table_changes.values() {
                    let old_key = change
                        .old
                        .as_ref()
                        .map(|row| {
                            encode_secondary_index_entry_key(&table.schema, &index.definition, row)
                        })
                        .transpose()?
                        .flatten();
                    let next_key = change
                        .next
                        .as_ref()
                        .map(|row| {
                            encode_secondary_index_entry_key(&table.schema, &index.definition, row)
                        })
                        .transpose()?
                        .flatten();
                    if old_key == next_key {
                        continue;
                    }
                    if let Some(key) = old_key
                        && let Some(root) = index.root_page_id
                    {
                        let removal = Btree::delete(&mut transaction, root, index.tree_id, &key)?;
                        if !removal.removed {
                            return Err(storage_corrupt(format!(
                                "Index `{}` is missing an entry for a candidate row",
                                index.definition.name
                            )));
                        }
                        index.root_page_id = removal.root_page_id;
                        deleted += 1;
                    }
                    if let Some(key) = next_key {
                        let root = match index.root_page_id {
                            Some(root) => root,
                            None => Btree::create(&mut transaction, index.tree_id)?,
                        };
                        index.root_page_id = Some(
                            Btree::upsert(&mut transaction, root, index.tree_id, &key, &[])?
                                .root_page_id,
                        );
                        inserted += 1;
                    }
                }
                index.entry_count =
                    adjusted_count(index.entry_count, inserted, deleted, "index entry")?;
            }
        }
        Ok(())
    }

    fn lookup_encoded_primary_key(&self, table_name: &str, key: &[u8]) -> Result<Option<Row>> {
        let table = self
            .tables
            .get(table_name)
            .ok_or_else(|| EngineError::table_not_found(table_name))?;
        let Some(root) = table.root_page_id else {
            return Ok(None);
        };
        self.charge_operations(1)?;
        get_in_transaction(&mut self.transaction.borrow_mut(), root, table.tree_id, key)?
            .map(|value| validated_row(&table.schema, key, &value))
            .transpose()
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
        let header = encode_catalog_header_record(&CatalogHeader {
            next_tree_id: self.next_tree_id,
            table_count: u32::try_from(self.tables.len())
                .map_err(|_| limit_error("The catalog contains too many tables"))?,
            index_count: u32::try_from(self.indexes.len())
                .map_err(|_| limit_error("The catalog contains too many indexes"))?,
        })?;
        let mut transaction = self.transaction.borrow_mut();
        let mut root = match self.catalog_root {
            Some(root) => root,
            None => Btree::create(&mut transaction, CATALOG_TREE_ID)?,
        };
        let final_indexes = self.indexes.keys().cloned().collect::<BTreeSet<_>>();
        let final_tables = self.tables.keys().cloned().collect::<BTreeSet<_>>();
        for name in self.base_indexes.difference(&final_indexes) {
            let deleted = Btree::delete(
                &mut transaction,
                root,
                CATALOG_TREE_ID,
                &encode_catalog_index_key(name)?,
            )?;
            if !deleted.removed {
                return Err(storage_corrupt(format!(
                    "Catalog record for dropped index `{name}` is missing"
                )));
            }
            root = deleted
                .root_page_id
                .ok_or_else(|| storage_corrupt("Dropping an index removed the catalog header"))?;
        }
        for name in self.base_tables.difference(&final_tables) {
            let deleted = Btree::delete(
                &mut transaction,
                root,
                CATALOG_TREE_ID,
                &encode_catalog_table_key(name)?,
            )?;
            if !deleted.removed {
                return Err(storage_corrupt(format!(
                    "Catalog record for dropped table `{name}` is missing"
                )));
            }
            root = deleted
                .root_page_id
                .ok_or_else(|| storage_corrupt("Dropping a table removed the catalog header"))?;
        }
        root = Btree::upsert(
            &mut transaction,
            root,
            CATALOG_TREE_ID,
            &header.0,
            &header.1,
        )?
        .root_page_id;
        let mut database_hash = EMPTY_HASH;
        for table in self.tables.values() {
            let (key, value) = encode_catalog_table_record(&CatalogTableRecord {
                schema: table.schema.clone(),
                tree_id: table.tree_id,
                root_page_id: table.root_page_id,
                row_count: table.row_count as u64,
                hash: table.hash,
            })?;
            root =
                Btree::upsert(&mut transaction, root, CATALOG_TREE_ID, &key, &value)?.root_page_id;
            // Binding each table's fingerprint to its name keeps two tables from cancelling each
            // other out, and makes exchanging the contents of two tables a visible change.
            database_hash = combine(
                database_hash,
                identify(table.schema.name.as_bytes(), table.hash),
            );
        }
        for index in self.indexes.values() {
            let (key, value) = encode_catalog_index_record(&CatalogIndexRecord {
                definition: index.definition.clone(),
                tree_id: index.tree_id,
                root_page_id: index.root_page_id,
                entry_count: index.entry_count as u64,
            })?;
            root =
                Btree::upsert(&mut transaction, root, CATALOG_TREE_ID, &key, &value)?.root_page_id;
        }
        Ok(CatalogPublication {
            root_page_id: root,
            database_hash,
        })
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

fn get_in_transaction<D: PageDevice>(
    transaction: &mut PagerWriteTransaction<'_, D>,
    root: PageId,
    tree_id: TreeId,
    key: &[u8],
) -> Result<Option<Vec<u8>>> {
    let mut cursor = Btree::cursor_from_in_transaction(transaction, root, tree_id, key)?;
    match cursor.next_in_transaction(transaction)? {
        Some((found, value)) if found.as_slice() == key => Ok(Some(value)),
        _ => Ok(None),
    }
}

fn execute_query_result(result: QueryResult) -> Result<ExecuteResult> {
    Ok(ExecuteResult {
        command: "SELECT".to_owned(),
        revision: result.revision,
        row_count: result.rows.len(),
        fields: result.fields,
        rows: result.rows,
        tables: vec![],
    })
}

impl<D: PageDevice> StorageReader for PagedScriptCandidate<'_, D> {
    fn charge_work(&self, operations: usize) -> Result<()> {
        self.charge_operations(operations)
    }

    fn visit_table(
        &self,
        table: &str,
        visitor: &mut dyn FnMut(&Row) -> Result<VisitControl>,
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
            let next = cursor.next_in_transaction(&mut self.transaction.borrow_mut())?;
            let Some((key, value)) = next else {
                break;
            };
            self.charge_operations(1)?;
            let row = validated_row(&table.schema, &key, &value)?;
            if visitor(&row)? == VisitControl::Stop {
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
        self.lookup_encoded_primary_key(table, &key)
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
        visitor: &mut dyn FnMut(&Row) -> Result<VisitControl>,
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
            let next = cursor.next_in_transaction(&mut self.transaction.borrow_mut())?;
            let Some((entry_key, value)) = next else {
                break;
            };
            self.charge_operations(1)?;
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
            let row_value = get_in_transaction(
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
            let row = validated_row(&table_data.schema, primary_key, &row_value)?;
            if visitor(&row)? == VisitControl::Stop {
                return Ok(Some(VisitOutcome::Stopped));
            }
        }
        Ok(Some(VisitOutcome::Complete))
    }

    fn table_schema(&self, table: &str) -> Result<crate::TableDefinition> {
        self.tables
            .get(table)
            .map(|table| table.schema.clone())
            .ok_or_else(|| EngineError::table_not_found(table))
    }

    fn revision(&self) -> u64 {
        self.base_revision
    }
}
