use std::cell::Cell;
use std::rc::Rc;

use serde_json::Value;

use crate::{
    ApplyOutcome, ChangedKeys, EngineError, ExecuteResult, IndexDefinition, PageDevice,
    PagedStorage, PreparedStatementId, QueryResult, Result, StorageReader, TableDefinition,
    paged_transaction::{PagedReadView, PagedTransaction},
    prepared_statement::PreparedStatementRegistry,
    statement::{PlannedDml, Statement, WriteStatement},
};

/// A SQL engine which publishes row mutations directly through the crash-safe page store.
///
/// This page-native engine slice supports reads, table/index lifecycle, streaming column addition,
/// and standalone `INSERT`, `UPDATE`, and `DELETE` statements, including bounded explicit row
/// transactions. Other page-native DDL is not part of this surface yet.
pub struct PagedEngine<D: PageDevice> {
    storage: PagedStorage<D>,
    transaction: Option<PagedTransaction>,
    prepared_statements: PreparedStatementRegistry,
}

impl<D: PageDevice> PagedEngine<D> {
    /// Opens a previously published paged database.
    pub fn open(device: D) -> Result<Self> {
        PagedStorage::open(device).map(|storage| Self {
            storage,
            transaction: None,
            prepared_statements: PreparedStatementRegistry::default(),
        })
    }

    /// The fingerprint of every row in this database.
    #[cfg(test)]
    pub(crate) fn database_hash(&self) -> u64 {
        self.storage.database_hash()
    }

    #[cfg(test)]
    pub(crate) fn query_sql(&self, sql: &str, params: &[Value]) -> Result<QueryResult> {
        match crate::statement::parse(sql, params)? {
            Statement::Select(plan) => crate::query::execute(&self.read_view(), &plan),
            Statement::Aggregate(plan) => crate::aggregate::execute(&self.read_view(), &plan),
            Statement::Join(plan) => crate::join::execute(&self.read_view(), &plan),
            Statement::Write(_) => Err(EngineError::unsupported_sql(
                "query_sql accepts only SELECT statements",
            )),
        }
    }

    /// Executes one read or standalone row-mutation statement.
    ///
    /// A successful mutation is prepared against the current revision and then published in one
    /// pager generation. A statement which matches no rows does not publish a generation or
    /// advance the revision.
    pub fn execute_sql(&mut self, sql: &str, params: &[Value]) -> Result<ExecuteResult> {
        self.execute_sql_rows(sql, params, false)
    }

    /// Executes one statement as [`Self::execute_sql`] does, for a caller that with `array_rows`
    /// reads each row's values in field order rather than by name.
    ///
    /// Only such a caller can tell apart fields of one name, so only it may be returned them: a
    /// `SELECT` whose output names repeat is otherwise refused. Its rows hold each value under
    /// its field's position, written as three digits, and then its name.
    pub fn execute_sql_rows(
        &mut self,
        sql: &str,
        params: &[Value],
        array_rows: bool,
    ) -> Result<ExecuteResult> {
        let mut statement = crate::statement::parse(sql, params)?;
        if array_rows {
            statement.position_outputs()?;
        }
        self.execute_parsed_statement(statement)
    }

    /// Parses and retains one reusable parameterized SQL statement for this engine session.
    pub fn prepare_sql(&mut self, sql: &str) -> Result<PreparedStatementId> {
        self.storage.ensure_readiness()?;
        self.prepared_statements.prepare(sql)
    }

    /// Binds and executes a retained statement against the current catalog and transaction view.
    pub fn execute_prepared(
        &mut self,
        id: PreparedStatementId,
        params: &[Value],
    ) -> Result<ExecuteResult> {
        self.execute_prepared_rows(id, params, false)
    }

    /// Executes a retained statement as [`Self::execute_prepared`] does, with fields of one name
    /// as [`Self::execute_sql_rows`] returns them.
    pub fn execute_prepared_rows(
        &mut self,
        id: PreparedStatementId,
        params: &[Value],
        array_rows: bool,
    ) -> Result<ExecuteResult> {
        let mut statement = self.prepared_statements.bind(id, params)?;
        if array_rows {
            statement.position_outputs()?;
        }
        self.execute_parsed_statement(statement)
    }

    /// Releases a retained statement. Closing an already closed issued ID is idempotent.
    pub fn close_prepared(&mut self, id: PreparedStatementId) -> Result<()> {
        self.prepared_statements.close(id)
    }

    fn execute_parsed_statement(&mut self, statement: Statement) -> Result<ExecuteResult> {
        if self.transaction.is_none() {
            if matches!(statement, Statement::Write(_)) {
                return self
                    .storage
                    .execute_script(vec![statement])?
                    .pop()
                    .ok_or_else(|| {
                        EngineError::new("INTERNAL_ERROR", "SQL statement produced no result")
                    });
            }
            // A read changes nothing, so it reads the committed view without opening a write
            // candidate, and its result is bounded as a one-statement script's would be.
            self.storage.ensure_readiness()?;
            let result = self.execute_statement(statement, None)?;
            crate::paged_script::retain_result(&mut 7, &result)?;
            return Ok(result);
        }
        self.execute_statement(statement, None)
    }

    /// Executes a semicolon-delimited SQL script as one atomic operation.
    ///
    /// Standalone scripts publish all DDL and DML in one pager generation. Within an explicit
    /// transaction, scripts are limited to reads and row DML and install their cloned overlay only
    /// after every statement succeeds.
    pub fn exec_sql(&mut self, sql: &str) -> Result<Vec<ExecuteResult>> {
        self.exec_sql_rows(sql, false)
    }

    /// Executes a script as [`Self::exec_sql`] does, with fields of one name as
    /// [`Self::execute_sql_rows`] returns them.
    pub fn exec_sql_rows(&mut self, sql: &str, array_rows: bool) -> Result<Vec<ExecuteResult>> {
        let script = crate::sql_script::split(sql)?;
        if script.is_empty() {
            return Err(EngineError::invalid_query(
                "exec SQL must contain at least one statement",
            ));
        }
        let mut statements = script
            .into_iter()
            .map(|sql| crate::statement::parse(sql, &[]))
            .collect::<Result<Vec<_>>>()?;
        if array_rows {
            for statement in &mut statements {
                statement.position_outputs()?;
            }
        }
        if self.transaction.is_none() {
            return self.storage.execute_script(statements);
        }
        if statements
            .iter()
            .any(|statement| matches!(statement, Statement::Write(write) if write.is_ddl()))
        {
            return Err(EngineError::unsupported_sql(
                "SQL scripts inside explicit transactions support SELECT, INSERT, UPDATE, and DELETE, but not DDL",
            ));
        }
        let original = self
            .transaction
            .take()
            .expect("the explicit transaction branch was selected above");
        self.transaction = Some(original.clone());
        let work = Cell::new(0);
        let mut result_bytes = 7usize;
        let mut results = Vec::with_capacity(statements.len());
        for statement in statements {
            let result = self
                .execute_statement(statement, Some(&work))
                .and_then(|result| {
                    crate::paged_script::retain_result(&mut result_bytes, &result)?;
                    Ok(result)
                });
            match result {
                Ok(result) => results.push(result),
                Err(error) => {
                    self.transaction = Some(original);
                    return Err(error);
                }
            }
        }
        Ok(results)
    }

    fn execute_statement(
        &mut self,
        statement: Statement,
        work: Option<&Cell<usize>>,
    ) -> Result<ExecuteResult> {
        match statement {
            Statement::Select(plan) => execute_query_result(crate::query::execute(
                &self.read_view_with_work(work),
                &plan,
            )?),
            Statement::Aggregate(plan) => execute_query_result(crate::aggregate::execute(
                &self.read_view_with_work(work),
                &plan,
            )?),
            Statement::Join(plan) => execute_query_result(crate::join::execute(
                &self.read_view_with_work(work),
                &plan,
            )?),
            Statement::Write(statement) => self.execute_transaction_write(&statement, work),
        }
    }

    pub fn revision(&self) -> u64 {
        self.storage.revision()
    }

    /// The database's tables, in name order, each with the indexes on it, also in name order.
    ///
    /// DDL cannot run inside a transaction, so a transaction sees the committed schema.
    pub fn schema(&self) -> Result<Vec<(Rc<TableDefinition>, Vec<IndexDefinition>)>> {
        self.storage.schema()
    }

    /// Checks every row and index entry of the committed database, and every page holding them,
    /// returning the first problem found.
    ///
    /// Opening a database checks only its catalog, and every page and row is checked as it is
    /// read, so this is the one check of the whole database. It reads all of it.
    pub fn check(&self) -> Result<()> {
        self.storage.check()
    }

    #[doc(hidden)]
    pub fn ensure_readiness(&self) -> Result<()> {
        self.storage.ensure_readiness()
    }

    pub fn begin_transaction(&mut self) -> Result<()> {
        self.storage.ensure_readiness()?;
        if self.in_transaction() {
            return Err(EngineError::transaction_active());
        }
        self.transaction = Some(PagedTransaction::new(self.storage.revision()));
        Ok(())
    }

    pub fn commit_transaction(&mut self) -> Result<ApplyOutcome> {
        self.storage.ensure_readiness()?;
        let transaction = self
            .transaction
            .as_ref()
            .ok_or_else(EngineError::no_active_transaction)?;
        if transaction.base_revision() != self.storage.revision() {
            return Err(EngineError::new(
                "WRITE_CONFLICT",
                format!(
                    "Transaction revision {} does not match current revision {}",
                    transaction.base_revision(),
                    self.storage.revision()
                ),
            ));
        }
        if !transaction.is_dirty() {
            self.transaction = None;
            return Ok(ApplyOutcome {
                revision: self.storage.revision(),
                tables: vec![],
                keys: ChangedKeys::default(),
            });
        }
        let outcome = self.storage.commit_transaction(transaction)?;
        self.transaction = None;
        Ok(outcome)
    }

    pub fn rollback_transaction(&mut self) -> Result<()> {
        // Force a fallible storage read first so an ambiguous publication cannot be presented as a
        // successful rollback. Reopening the device is the only valid recovery in that state.
        self.storage.ensure_readiness()?;
        if self.transaction.take().is_none() {
            return Err(EngineError::no_active_transaction());
        }
        Ok(())
    }

    pub fn in_transaction(&self) -> bool {
        self.transaction.is_some()
    }

    pub fn into_device(self) -> D {
        self.storage.into_device()
    }

    fn read_view(&self) -> PagedReadView<'_, D> {
        PagedReadView::new(&self.storage, self.transaction.as_ref())
    }

    fn read_view_with_work<'a>(&'a self, work: Option<&'a Cell<usize>>) -> PagedReadView<'a, D> {
        match work {
            Some(work) => {
                PagedReadView::with_work_budget(&self.storage, self.transaction.as_ref(), work)
            }
            None => self.read_view(),
        }
    }

    fn execute_transaction_write(
        &mut self,
        statement: &WriteStatement,
        work: Option<&Cell<usize>>,
    ) -> Result<ExecuteResult> {
        self.storage.ensure_readiness()?;
        if !matches!(
            statement,
            WriteStatement::Insert { .. }
                | WriteStatement::Update { .. }
                | WriteStatement::Delete { .. }
        ) {
            return Err(EngineError::unsupported_sql(
                "Page-native explicit transactions currently support only INSERT, UPDATE, and DELETE",
            ));
        }
        let (
            PlannedDml {
                outcome,
                changes,
                previous,
            },
            keys,
        ) = {
            let view = self.read_view_with_work(work);
            let planned = crate::statement::plan_dml(&view, statement)?;
            let keys = crate::statement::changed_keys(&view, &planned.changes, &planned.previous)?;
            (planned, keys)
        };
        let fields =
            crate::statement::write_result_fields(&self.read_view_with_work(work), statement)?;
        if outcome.mutated {
            self.transaction
                .as_mut()
                .expect("the transaction branch was selected above")
                .stage(&self.storage, changes, previous)?;
        }
        Ok(ExecuteResult {
            command: outcome.command.to_owned(),
            revision: self.storage.revision(),
            row_count: outcome.row_count,
            fields,
            rows: outcome.rows,
            tables: outcome.tables,
            // Staged work publishes at commit, so these are reported for symmetry with `tables`
            // and ignored by subscribers until the transaction commits.
            keys,
        })
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
        keys: ChangedKeys::default(),
    })
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use serde_json::{Value, json};

    use super::*;
    use crate::{
        ColumnType, Engine, InMemoryStorage, MAX_CHANGED_KEYS_PER_TABLE, MemoryPageDevice,
        PAGE_SIZE, PageId, ResultField, Row,
    };

    #[derive(Default)]
    struct DurableState {
        working: Vec<[u8; PAGE_SIZE]>,
        durable: Vec<[u8; PAGE_SIZE]>,
        flushes: usize,
        fail_after_flush: Option<usize>,
        fail_next_write: bool,
    }

    #[derive(Clone, Default)]
    struct DurableDevice(Rc<RefCell<DurableState>>);

    impl DurableDevice {
        fn arm_after_flush(&self, flush: usize) {
            let mut state = self.0.borrow_mut();
            state.flushes = 0;
            state.fail_after_flush = Some(flush);
        }

        /// Fails the next page write before it changes anything.
        fn arm_next_write(&self) {
            self.0.borrow_mut().fail_next_write = true;
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
            if std::mem::take(&mut state.fail_next_write) {
                return Err(EngineError::new(
                    "INJECTED_IO",
                    "failure before a page write",
                ));
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

    fn source() -> InMemoryStorage {
        let mut engine = Engine::default();
        engine
            .execute_sql(
                "CREATE TABLE accounts (\
                    id INTEGER PRIMARY KEY, \
                    email TEXT, \
                    active BOOLEAN NOT NULL DEFAULT false\
                )",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "CREATE UNIQUE INDEX accounts_email ON accounts (email)",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO accounts (id, email) VALUES \
                    (1, 'ada@example.com'), (2, 'lin@example.com')",
                &[],
            )
            .unwrap();
        engine.into_storage()
    }

    fn database_with(statements: &[&str]) -> PagedEngine<MemoryPageDevice> {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        for sql in statements {
            engine.execute_sql(sql, &[]).unwrap();
        }
        engine
    }

    const LEDGER: &str = "CREATE TABLE ledger (id INTEGER PRIMARY KEY, note TEXT)";

    #[test]
    fn a_key_too_long_to_store_fails_a_lookup_and_a_write_scans_for_it() {
        let mut engine = database_with(&[
            "CREATE TABLE t (id TEXT PRIMARY KEY, v INTEGER NOT NULL)",
            "INSERT INTO t (id, v) VALUES ('a', 1)",
        ]);
        let oversized = [json!("x".repeat(2_000))];
        for in_transaction in [false, true] {
            if in_transaction {
                engine.begin_transaction().unwrap();
            }
            for sql in [
                "SELECT * FROM t WHERE id = $1",
                "SELECT * FROM t WHERE v = 1 AND id = $1",
            ] {
                let error = engine.execute_sql(sql, &oversized).unwrap_err();
                assert_eq!(error.code, "PAGED_VALUE_TOO_LARGE", "{sql}");
            }
            for sql in [
                "UPDATE t SET v = 2 WHERE id = $1",
                "DELETE FROM t WHERE id = $1",
            ] {
                let result = engine.execute_sql(sql, &oversized).unwrap();
                assert_eq!(result.row_count, 0, "{sql}");
            }
            // Of two equalities with the key, one is looked up and the other rejects the row.
            for sql in [
                "SELECT * FROM t WHERE id = 'a' AND id = 'b'",
                "SELECT * FROM t WHERE id = 'b' AND (v = 1 AND id = 'a')",
                "UPDATE t SET v = 3 WHERE id = 'b' AND id = 'a'",
            ] {
                assert_eq!(engine.execute_sql(sql, &[]).unwrap().row_count, 0, "{sql}");
            }
            let found = engine
                .execute_sql("SELECT v FROM t WHERE v = 1 AND id = 'a'", &[])
                .unwrap();
            assert_eq!(found.rows.len(), 1);
            if in_transaction {
                engine.rollback_transaction().unwrap();
            }
        }
    }

    #[test]
    fn the_database_fingerprint_describes_the_rows_and_nothing_else() {
        // Two databases which hold the same rows must agree, however the rows got there. This is
        // the whole point of publishing the fingerprint: a comparison between two databases has to
        // report a difference only when the data genuinely differs.
        let together = database_with(&[
            LEDGER,
            "INSERT INTO ledger (id, note) VALUES (1, 'one'), (2, 'two'), (3, 'three')",
        ]);
        let separately = database_with(&[
            LEDGER,
            "INSERT INTO ledger (id, note) VALUES (3, 'three')",
            "INSERT INTO ledger (id, note) VALUES (1, 'one')",
            "INSERT INTO ledger (id, note) VALUES (2, 'two')",
        ]);
        assert_eq!(together.database_hash(), separately.database_hash());
        assert_ne!(together.database_hash(), crate::hash::EMPTY_HASH);

        // Reaching the same rows by a different route must also agree.
        let corrected = database_with(&[
            LEDGER,
            "INSERT INTO ledger (id, note) VALUES (1, 'one'), (2, 'wrong'), (3, 'three')",
            "UPDATE ledger SET note = 'two' WHERE id = 2",
        ]);
        assert_eq!(together.database_hash(), corrected.database_hash());

        let different = database_with(&[
            LEDGER,
            "INSERT INTO ledger (id, note) VALUES (1, 'one'), (2, 'two'), (3, 'other')",
        ]);
        assert_ne!(together.database_hash(), different.database_hash());

        let fewer = database_with(&[
            LEDGER,
            "INSERT INTO ledger (id, note) VALUES (1, 'one'), (2, 'two')",
        ]);
        assert_ne!(together.database_hash(), fewer.database_hash());
    }

    #[test]
    fn the_database_fingerprint_binds_rows_to_the_table_holding_them() {
        // Exchanging the contents of two tables leaves the same rows in the database. Combining
        // table fingerprints without their names would report the two arrangements as identical.
        let schema = [
            "CREATE TABLE a (id INTEGER PRIMARY KEY)",
            "CREATE TABLE b (id INTEGER PRIMARY KEY)",
        ];
        let arranged = database_with(&[
            schema[0],
            schema[1],
            "INSERT INTO a (id) VALUES (1)",
            "INSERT INTO b (id) VALUES (2)",
        ]);
        let exchanged = database_with(&[
            schema[0],
            schema[1],
            "INSERT INTO a (id) VALUES (2)",
            "INSERT INTO b (id) VALUES (1)",
        ]);
        assert_ne!(arranged.database_hash(), exchanged.database_hash());
    }

    #[test]
    fn the_database_fingerprint_covers_empty_tables_but_not_derived_structures() {
        // A table which exists and holds no rows is a real difference from no table at all, so a
        // table contributes its name even while it is empty.
        let nothing = database_with(&[]);
        assert_eq!(nothing.database_hash(), crate::hash::EMPTY_HASH);

        let empty = database_with(&[LEDGER]);
        assert_ne!(empty.database_hash(), crate::hash::EMPTY_HASH);

        // Emptying a table must return it to exactly the state of one which never held rows;
        // otherwise a fingerprint would carry a memory of deleted data forever.
        let emptied = database_with(&[
            LEDGER,
            "INSERT INTO ledger (id, note) VALUES (1, 'one'), (2, 'two')",
            "DELETE FROM ledger WHERE id > 0",
        ]);
        assert_eq!(emptied.database_hash(), empty.database_hash());

        // A secondary index is derived from rows the fingerprint already covers, so building one
        // must not register as a change to the data.
        let plain = database_with(&[LEDGER, "INSERT INTO ledger (id, note) VALUES (1, 'one')"]);
        let indexed = database_with(&[
            LEDGER,
            "INSERT INTO ledger (id, note) VALUES (1, 'one')",
            "CREATE INDEX ledger_note ON ledger (note)",
        ]);
        assert_eq!(plain.database_hash(), indexed.database_hash());
    }

    #[test]
    fn the_database_fingerprint_is_published_and_survives_reopening() {
        let engine = database_with(&[
            LEDGER,
            "INSERT INTO ledger (id, note) VALUES (1, 'one'), (2, 'two')",
        ]);
        let published = engine.database_hash();
        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        reopened.check().unwrap();
        assert_eq!(reopened.database_hash(), published);
    }

    /// Builds engine tests through the public SQL path.
    fn page_native_fixture<D: PageDevice>(device: D) -> Result<PagedEngine<D>> {
        let mut engine = PagedEngine::open(device)?;
        for sql in [
            "CREATE TABLE accounts (\
                id INTEGER PRIMARY KEY, \
                email TEXT, \
                active BOOLEAN NOT NULL DEFAULT false\
            )",
            "CREATE UNIQUE INDEX accounts_email ON accounts (email)",
            "INSERT INTO accounts (id, email) VALUES \
                (1, 'ada@example.com'), (2, 'lin@example.com')",
        ] {
            engine.execute_sql(sql, &[])?;
        }
        Ok(engine)
    }

    #[test]
    fn page_native_dml_matches_in_memory_and_reopens() {
        let source = source();
        let mut expected = Engine::new(source.clone());
        let mut actual = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();

        for (sql, params) in [
            (
                "INSERT INTO accounts (id, email) VALUES ($1, $2) \
                 RETURNING id, email, active",
                vec![json!(3), json!("grace@example.com")],
            ),
            (
                "UPDATE accounts SET id = 4, active = true WHERE id = 3 \
                 RETURNING id, email, active",
                vec![],
            ),
            (
                "DELETE FROM accounts WHERE id = 2 RETURNING id, email",
                vec![],
            ),
        ] {
            assert_eq!(
                actual.execute_sql(sql, &params).unwrap(),
                expected.execute_sql(sql, &params).unwrap(),
                "{sql}",
            );
        }

        for sql in [
            "SELECT id, email, active FROM accounts ORDER BY id",
            "SELECT active, COUNT(*) AS rows FROM accounts GROUP BY active ORDER BY active",
            "SELECT a.id, b.email FROM accounts a JOIN accounts b ON a.id = b.id ORDER BY a.id",
        ] {
            assert_eq!(
                actual.query_sql(sql, &[]).unwrap(),
                expected.query_sql(sql, &[]).unwrap()
            );
            assert_eq!(
                actual.execute_sql(sql, &[]).unwrap(),
                expected.execute_sql(sql, &[]).unwrap()
            );
        }

        let revision = actual.revision();
        let mut reopened = PagedEngine::open(actual.into_device()).unwrap();
        reopened.check().unwrap();
        assert_eq!(reopened.revision(), revision);
        assert_eq!(
            reopened
                .query_sql("SELECT id, email, active FROM accounts ORDER BY id", &[])
                .unwrap(),
            expected
                .query_sql("SELECT id, email, active FROM accounts ORDER BY id", &[])
                .unwrap()
        );
        assert_eq!(
            reopened
                .execute_sql("DELETE FROM accounts WHERE id = 4 RETURNING id", &[])
                .unwrap()
                .rows,
            vec![row(json!({"id": 4}))]
        );
    }

    #[test]
    fn a_transaction_giving_up_many_unique_values_still_checks_them() {
        // Each update gives up the value before it, and the transaction's claims are rebuilt
        // once many are given up.
        let mut engine = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine.begin_transaction().unwrap();
        for round in 0..200 {
            engine
                .execute_sql(
                    "UPDATE accounts SET email = $1 WHERE id = 1",
                    &[json!(format!("ada{round}@example.com"))],
                )
                .unwrap();
        }
        assert_eq!(
            engine
                .execute_sql(
                    "UPDATE accounts SET email = 'ada199@example.com' WHERE id = 2",
                    &[],
                )
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        engine
            .execute_sql(
                "UPDATE accounts SET email = 'ada100@example.com' WHERE id = 2",
                &[],
            )
            .unwrap();
        engine.commit_transaction().unwrap();
        assert_eq!(
            engine
                .query_sql("SELECT id, email FROM accounts ORDER BY id", &[])
                .unwrap()
                .rows,
            vec![
                row(json!({"id": 1, "email": "ada199@example.com"})),
                row(json!({"id": 2, "email": "ada100@example.com"})),
            ]
        );
    }

    #[test]
    fn scans_read_rows_whose_values_overflow_their_leaves() {
        // Scans take a leaf's rows from the cursor's copy of it, reading a value that overflows
        // the leaf through the pager, in both directions, and inside a write.
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .execute_sql(
                "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT NOT NULL)",
                &[],
            )
            .unwrap();
        let large = |id: u64| format!("{id} {}", "x".repeat(5_000));
        for id in 1..=60 {
            let body = if id % 7 == 0 {
                large(id)
            } else {
                format!("row {id}")
            };
            engine
                .execute_sql(
                    "INSERT INTO docs (id, body) VALUES ($1, $2)",
                    &[json!(id), json!(body)],
                )
                .unwrap();
        }
        let ids = |rows: &[Row]| rows.iter().map(|row| row["id"].clone()).collect::<Vec<_>>();
        let rows = engine
            .query_sql(
                "SELECT id, body FROM docs WHERE id >= 20 ORDER BY id DESC",
                &[],
            )
            .unwrap()
            .rows;
        assert_eq!(rows.len(), 41);
        assert_eq!(rows[0], row(json!({"id": 60, "body": "row 60"})));
        assert_eq!(rows[7], row(json!({"id": 53, "body": "row 53"})));
        assert_eq!(rows[5], row(json!({"id": 55, "body": "row 55"})));
        assert_eq!(rows[4], row(json!({"id": 56, "body": large(56)})));
        let deleted = engine
            .execute_sql("DELETE FROM docs WHERE body LIKE '%xxx' RETURNING id", &[])
            .unwrap();
        assert_eq!(
            ids(&deleted.rows),
            [7, 14, 21, 28, 35, 42, 49, 56].map(|id| json!(id))
        );
        assert_eq!(
            engine
                .query_sql("SELECT count(*) AS n FROM docs WHERE body LIKE 'row%'", &[])
                .unwrap()
                .rows,
            vec![row(json!({"n": 52}))]
        );
    }

    #[test]
    fn page_native_writes_with_keys_out_of_order_match_in_memory() {
        // A statement's changes arrive in key order from a scan, and in any order from VALUES,
        // which the writer handles apart. Each statement here names a key below one before it.
        let mut expected = Engine::new(source());
        let mut actual = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
        for sql in [
            "INSERT INTO accounts (id, email) VALUES \
                (5, 'e@example.com'), (3, 'c@example.com'), (4, 'd@example.com') RETURNING id",
            "INSERT INTO accounts (id, email) VALUES \
                (9, 'i@example.com'), (7, 'g@example.com'), (9, 'again@example.com')",
            "INSERT INTO accounts (id, email) VALUES (8, 'h@example.com'), (8, 'again@example.com')",
            "INSERT INTO accounts (id, email) VALUES (5, 'c@example.com'), (3, 'e@example.com') \
                ON CONFLICT (id) DO UPDATE SET email = EXCLUDED.email RETURNING id, email",
            "INSERT INTO accounts (id, email) VALUES (7, 'x@example.com'), (6, 'x@example.com')",
            "INSERT INTO accounts (id, email) VALUES (6, 'f@example.com'), (1, 'lin@example.com') \
                ON CONFLICT (id) DO UPDATE SET email = EXCLUDED.email",
            "INSERT INTO accounts (id, email) VALUES (6, 'f@example.com'), (1, 'a@example.com') \
                ON CONFLICT (id) DO UPDATE SET email = EXCLUDED.email RETURNING id, email",
            "UPDATE accounts SET active = true WHERE id > 1 RETURNING id",
            "DELETE FROM accounts WHERE id > 2 RETURNING id",
        ] {
            match (actual.execute_sql(sql, &[]), expected.execute_sql(sql, &[])) {
                (Ok(actual), Ok(expected)) => assert_eq!(actual, expected, "{sql}"),
                (Err(actual), Err(expected)) => assert_eq!(actual.code, expected.code, "{sql}"),
                (actual, expected) => panic!("{sql}: {actual:?} is not {expected:?}"),
            }
            let select = "SELECT id, email, active FROM accounts ORDER BY id";
            assert_eq!(
                actual.query_sql(select, &[]).unwrap().rows,
                expected.query_sql(select, &[]).unwrap().rows,
                "{sql}",
            );
        }
    }

    #[test]
    fn failed_and_no_match_dml_do_not_publish() {
        let mut engine = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
        let revision = engine.revision();
        let rows = engine
            .query_sql("SELECT * FROM accounts ORDER BY id", &[])
            .unwrap();

        for sql in [
            "UPDATE accounts SET id = 1 WHERE id = 2",
            // Both rows would move to one key, the first to a key the second leaves.
            "UPDATE accounts SET id = 2 WHERE id >= 1",
            "UPDATE accounts SET id = 7",
            "UPDATE accounts SET email = 'ada@example.com' WHERE id = 2",
            "INSERT INTO accounts (id, email) VALUES \
                (3, 'new@example.com'), (1, 'duplicate@example.com')",
        ] {
            assert_eq!(
                engine.execute_sql(sql, &[]).unwrap_err().code,
                "CONSTRAINT_VIOLATION",
                "{sql}",
            );
            assert_eq!(engine.revision(), revision, "{sql}");
            assert_eq!(
                engine
                    .query_sql("SELECT * FROM accounts ORDER BY id", &[])
                    .unwrap(),
                rows,
                "{sql}",
            );
        }

        for sql in [
            "UPDATE accounts SET active = true WHERE id = 99 RETURNING id",
            "DELETE FROM accounts WHERE id = 99 RETURNING id",
        ] {
            let result = engine.execute_sql(sql, &[]).unwrap();
            assert_eq!(result.row_count, 0, "{sql}");
            assert!(result.rows.is_empty(), "{sql}");
            assert!(result.tables.is_empty(), "{sql}");
            assert_eq!(result.revision, revision, "{sql}");
            assert_eq!(engine.revision(), revision, "{sql}");
        }

        let created = engine
            .execute_sql("CREATE TABLE later (id INTEGER PRIMARY KEY)", &[])
            .unwrap();
        assert_eq!(created.command, "CREATE TABLE");
        assert_eq!(created.revision, revision + 1);
        assert_eq!(created.tables, vec!["later"]);
        assert_eq!(
            engine
                .execute_sql(
                    "CREATE TABLE IF NOT EXISTS later (other TEXT PRIMARY KEY)",
                    &[],
                )
                .unwrap()
                .revision,
            revision + 1
        );
        assert_eq!(
            engine
                .execute_sql("CREATE TABLE later (id INTEGER PRIMARY KEY)", &[])
                .unwrap_err()
                .code,
            "TABLE_ALREADY_EXISTS"
        );

        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        reopened.check().unwrap();
        assert_eq!(reopened.revision(), revision + 1);
        assert_eq!(
            reopened
                .query_sql("SELECT * FROM accounts ORDER BY id", &[])
                .unwrap(),
            QueryResult {
                revision: revision + 1,
                fields: rows.fields,
                rows: rows.rows,
            }
        );
        assert!(
            reopened
                .query_sql("SELECT id FROM later", &[])
                .unwrap()
                .rows
                .is_empty()
        );
    }

    #[test]
    fn rootless_sql_create_matches_the_in_memory_engine_and_reopens() {
        let mut expected = Engine::default();
        let mut actual = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        let sql = "CREATE TABLE tasks (id INTEGER PRIMARY KEY, title TEXT NOT NULL)";

        assert_eq!(
            actual.execute_sql(sql, &[]).unwrap(),
            expected.execute_sql(sql, &[]).unwrap()
        );
        assert_eq!(actual.revision(), 1);
        assert_eq!(
            actual
                .execute_sql("INSERT INTO tasks (id, title) VALUES (1, 'first')", &[])
                .unwrap(),
            expected
                .execute_sql("INSERT INTO tasks (id, title) VALUES (1, 'first')", &[])
                .unwrap()
        );

        let reopened = PagedEngine::open(actual.into_device()).unwrap();
        reopened.check().unwrap();
        assert_eq!(
            reopened
                .query_sql("SELECT id, title FROM tasks ORDER BY id", &[])
                .unwrap(),
            expected
                .query_sql("SELECT id, title FROM tasks ORDER BY id", &[])
                .unwrap()
        );
    }

    #[test]
    fn page_native_create_index_matches_planning_if_not_exists_and_reopens() {
        let source = source();
        let mut expected = Engine::new(source.clone());
        let mut actual = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();

        let sql = "CREATE INDEX accounts_active ON accounts (active)";
        assert_eq!(
            actual.execute_sql(sql, &[]).unwrap(),
            expected.execute_sql(sql, &[]).unwrap()
        );
        assert_eq!(
            actual
                .query_sql(
                    "SELECT id FROM accounts WHERE active = false ORDER BY id",
                    &[],
                )
                .unwrap(),
            expected
                .query_sql(
                    "SELECT id FROM accounts WHERE active = false ORDER BY id",
                    &[],
                )
                .unwrap()
        );

        let revision = actual.revision();
        let no_op = "CREATE INDEX IF NOT EXISTS accounts_active ON accounts (email)";
        assert_eq!(
            actual.execute_sql(no_op, &[]).unwrap(),
            expected.execute_sql(no_op, &[]).unwrap()
        );
        assert_eq!(actual.revision(), revision);
        assert_eq!(
            actual.execute_sql(sql, &[]).unwrap_err(),
            expected.execute_sql(sql, &[]).unwrap_err()
        );
        assert_eq!(actual.revision(), revision);

        let reopened = PagedEngine::open(actual.into_device()).unwrap();
        reopened.check().unwrap();
        assert_eq!(reopened.revision(), revision);
        assert_eq!(
            reopened
                .query_sql(
                    "SELECT id FROM accounts WHERE active = false ORDER BY id",
                    &[],
                )
                .unwrap(),
            expected
                .query_sql(
                    "SELECT id FROM accounts WHERE active = false ORDER BY id",
                    &[],
                )
                .unwrap()
        );
    }

    #[test]
    fn page_native_add_column_matches_in_memory_backfill_and_error_order() {
        let source = source();
        let mut expected = Engine::new(source.clone());
        let mut actual = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();

        for sql in [
            "ALTER TABLE accounts ADD COLUMN score INTEGER NOT NULL DEFAULT 7",
            "ALTER TABLE accounts ADD COLUMN note TEXT",
        ] {
            assert_eq!(
                actual.execute_sql(sql, &[]).unwrap(),
                expected.execute_sql(sql, &[]).unwrap(),
                "{sql}"
            );
        }
        for sql in [
            "SELECT id, email, score, note FROM accounts ORDER BY id",
            "SELECT id, score FROM accounts WHERE email = 'ada@example.com'",
        ] {
            assert_eq!(
                actual.query_sql(sql, &[]).unwrap(),
                expected.query_sql(sql, &[]).unwrap(),
                "{sql}"
            );
        }

        let revision = actual.revision();
        // Name resolution deliberately precedes validation of the requested replacement shape.
        let duplicate =
            "ALTER TABLE accounts ADD COLUMN IF NOT EXISTS score BOOLEAN NOT NULL DEFAULT 'bad'";
        assert_eq!(
            actual.execute_sql(duplicate, &[]).unwrap(),
            expected.execute_sql(duplicate, &[]).unwrap()
        );
        assert_eq!(actual.revision(), revision);
        assert_eq!(
            actual
                .execute_sql("ALTER TABLE accounts ADD COLUMN score BOOLEAN", &[])
                .unwrap_err(),
            expected
                .execute_sql("ALTER TABLE accounts ADD COLUMN score BOOLEAN", &[])
                .unwrap_err()
        );
        assert_eq!(actual.revision(), revision);

        let reopened = PagedEngine::open(actual.into_device()).unwrap();
        reopened.check().unwrap();
        assert_eq!(reopened.revision(), revision);
        assert_eq!(
            reopened
                .query_sql(
                    "SELECT id, email, score, note FROM accounts ORDER BY id",
                    &[]
                )
                .unwrap(),
            expected
                .query_sql(
                    "SELECT id, email, score, note FROM accounts ORDER BY id",
                    &[]
                )
                .unwrap()
        );
    }

    #[test]
    fn page_native_unique_index_omits_nulls_and_rolls_back_duplicates() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .execute_sql(
                "CREATE TABLE items (id INTEGER PRIMARY KEY, code TEXT)",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO items (id, code) VALUES (1, NULL), (2, NULL), (3, 'one')",
                &[],
            )
            .unwrap();
        engine
            .execute_sql("CREATE UNIQUE INDEX items_code ON items (code)", &[])
            .unwrap();
        assert_eq!(
            engine
                .query_sql("SELECT id FROM items WHERE code = 'one'", &[])
                .unwrap()
                .rows,
            vec![row(json!({"id": 3}))]
        );
        assert_eq!(
            engine
                .execute_sql("INSERT INTO items (id, code) VALUES (4, 'one')", &[])
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );

        engine
            .execute_sql(
                "CREATE TABLE duplicates (id INTEGER PRIMARY KEY, code TEXT)",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO duplicates (id, code) VALUES (1, 'same'), (2, 'same')",
                &[],
            )
            .unwrap();
        let revision = engine.revision();
        assert_eq!(
            engine
                .execute_sql(
                    "CREATE UNIQUE INDEX duplicates_code ON duplicates (code)",
                    &[],
                )
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        assert_eq!(engine.revision(), revision);
        engine
            .execute_sql("CREATE INDEX duplicates_code ON duplicates (code)", &[])
            .unwrap();

        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        reopened.check().unwrap();
        assert_eq!(
            reopened
                .query_sql(
                    "SELECT id FROM duplicates WHERE code = 'same' ORDER BY id",
                    &[]
                )
                .unwrap()
                .rows,
            vec![row(json!({"id": 1})), row(json!({"id": 2}))]
        );
    }

    #[test]
    fn index_builds_sort_chunks_of_entries_and_find_duplicates_across_them() {
        // Enough rows that a build sorts and writes its entries in several chunks, with codes
        // scattered so that each chunk lands between the entries of earlier ones.
        let code = |id: i64| (id % 7 != 0).then_some(id * 7_919 % 2_003);
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .execute_sql(
                "CREATE TABLE items (id INTEGER PRIMARY KEY, code INTEGER, label TEXT NOT NULL)",
                &[],
            )
            .unwrap();
        for first in (1..=2_000).step_by(200) {
            let values = (first..first + 200)
                .map(|id| {
                    let code = code(id).map_or("NULL".to_string(), |code| code.to_string());
                    format!("({id}, {code}, 'label {}')", id % 50)
                })
                .collect::<Vec<_>>()
                .join(", ");
            engine
                .execute_sql(
                    &format!("INSERT INTO items (id, code, label) VALUES {values}"),
                    &[],
                )
                .unwrap();
        }
        let ids = |engine: &PagedEngine<MemoryPageDevice>, sql: &str| {
            engine
                .query_sql(sql, &[])
                .unwrap()
                .rows
                .into_iter()
                .map(|row| row["id"].as_i64().unwrap())
                .collect::<Vec<_>>()
        };
        let in_range = (1..=2_000)
            .filter(|id| code(*id).is_some_and(|code| (100..300).contains(&code)))
            .collect::<Vec<_>>();
        let labelled = (1..=2_000).filter(|id| id % 50 == 7).collect::<Vec<_>>();
        let ranged = "SELECT id FROM items WHERE code >= 100 AND code < 300 ORDER BY id";
        let equal = "SELECT id FROM items WHERE label = 'label 7' ORDER BY id";

        for index in [
            "CREATE UNIQUE INDEX items_code ON items (code)",
            "CREATE INDEX items_label ON items (label)",
        ] {
            engine.execute_sql(index, &[]).unwrap();
        }
        assert_eq!(ids(&engine, ranged), in_range);
        assert_eq!(ids(&engine, equal), labelled);

        // Rows 1 and 1,999 fall in different chunks; rows 2 and 3 in the same one.
        engine.execute_sql("DROP INDEX items_code", &[]).unwrap();
        for (duplicate, of) in [(1_999, 1), (3, 2)] {
            engine
                .execute_sql(
                    &format!(
                        "UPDATE items SET code = {} WHERE id = {duplicate}",
                        code(of).unwrap()
                    ),
                    &[],
                )
                .unwrap();
            let revision = engine.revision();
            assert_eq!(
                engine
                    .execute_sql("CREATE UNIQUE INDEX items_code ON items (code)", &[])
                    .unwrap_err()
                    .code,
                "CONSTRAINT_VIOLATION"
            );
            assert_eq!(engine.revision(), revision);
            engine
                .execute_sql(
                    &format!(
                        "UPDATE items SET code = {} WHERE id = {duplicate}",
                        code(duplicate).unwrap()
                    ),
                    &[],
                )
                .unwrap();
        }
        engine
            .execute_sql("CREATE UNIQUE INDEX items_code ON items (code)", &[])
            .unwrap();

        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        reopened.check().unwrap();
        assert_eq!(ids(&reopened, ranged), in_range);
        assert_eq!(ids(&reopened, equal), labelled);
    }

    #[test]
    fn page_native_drop_index_and_table_match_in_memory_and_reopen() {
        let source = source();
        let mut expected = Engine::new(source.clone());
        let mut actual = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();

        for sql in [
            "DROP INDEX accounts_email",
            "DROP INDEX IF EXISTS accounts_email",
            "CREATE INDEX accounts_active ON accounts (active)",
            "DROP TABLE accounts",
            "DROP TABLE IF EXISTS accounts",
        ] {
            assert_eq!(
                actual.execute_sql(sql, &[]).unwrap(),
                expected.execute_sql(sql, &[]).unwrap(),
                "{sql}"
            );
        }
        for sql in ["DROP INDEX accounts_email", "DROP TABLE accounts"] {
            assert_eq!(
                actual.execute_sql(sql, &[]).unwrap_err(),
                expected.execute_sql(sql, &[]).unwrap_err(),
                "{sql}"
            );
        }
        assert_eq!(
            actual.query_sql("SELECT * FROM accounts", &[]).unwrap_err(),
            expected
                .query_sql("SELECT * FROM accounts", &[])
                .unwrap_err()
        );

        let revision = actual.revision();
        let mut reopened = PagedEngine::open(actual.into_device()).unwrap();
        reopened.check().unwrap();
        assert_eq!(reopened.revision(), revision);
        assert_eq!(
            reopened
                .query_sql("SELECT * FROM accounts", &[])
                .unwrap_err()
                .code,
            "TABLE_NOT_FOUND"
        );
        assert_eq!(
            reopened
                .execute_sql("DROP INDEX IF EXISTS accounts_active", &[])
                .unwrap()
                .revision,
            revision
        );
        assert_eq!(
            reopened
                .execute_sql("DROP TABLE IF EXISTS accounts", &[])
                .unwrap()
                .revision,
            revision
        );
    }

    #[test]
    fn query_sql_rejects_mutations_without_publishing() {
        let engine = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
        let revision = engine.revision();
        assert_eq!(
            engine
                .query_sql("DELETE FROM accounts WHERE id = 1", &[])
                .unwrap_err()
                .code,
            "UNSUPPORTED_SQL"
        );
        assert_eq!(engine.revision(), revision);
    }

    #[test]
    fn explicit_row_transactions_match_in_memory_read_their_writes_and_reopen() {
        let source = source();
        let mut expected = Engine::new(source.clone());
        let mut actual = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
        let base_revision = actual.revision();
        expected.begin_transaction().unwrap();
        actual.begin_transaction().unwrap();

        for sql in [
            "INSERT INTO accounts (id, email) VALUES (3, 'grace@example.com') RETURNING id",
            "UPDATE accounts SET active = true WHERE id IN (1, 3) RETURNING id, active",
            "DELETE FROM accounts WHERE id = 2 RETURNING id, email",
        ] {
            assert_eq!(
                actual.execute_sql(sql, &[]).unwrap(),
                expected.execute_sql(sql, &[]).unwrap(),
                "{sql}",
            );
            assert_eq!(actual.revision(), base_revision);
        }

        for sql in [
            "SELECT id, email, active FROM accounts ORDER BY id",
            "SELECT active, COUNT(*) AS rows FROM accounts GROUP BY active ORDER BY active",
            "SELECT a.id, b.email FROM accounts a JOIN accounts b ON a.id = b.id ORDER BY a.id",
        ] {
            assert_eq!(
                actual.query_sql(sql, &[]).unwrap(),
                expected.query_sql(sql, &[]).unwrap(),
                "{sql}",
            );
        }

        let expected_commit = expected.commit_transaction().unwrap();
        let actual_commit = actual.commit_transaction().unwrap();
        assert_eq!(actual_commit, expected_commit);
        assert_eq!(actual.revision(), base_revision + 1);
        assert!(!actual.in_transaction());

        let reopened = PagedEngine::open(actual.into_device()).unwrap();
        reopened.check().unwrap();
        assert_eq!(reopened.revision(), base_revision + 1);
        assert_eq!(
            reopened
                .query_sql("SELECT id, email, active FROM accounts ORDER BY id", &[])
                .unwrap(),
            expected
                .query_sql("SELECT id, email, active FROM accounts ORDER BY id", &[])
                .unwrap()
        );
    }

    #[test]
    fn failed_transaction_statement_preserves_prior_work_and_ddl_is_rejected() {
        let mut engine = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
        let revision = engine.revision();
        engine.begin_transaction().unwrap();
        engine
            .execute_sql(
                "INSERT INTO accounts (id, email) VALUES (3, 'grace@example.com')",
                &[],
            )
            .unwrap();

        for sql in [
            "UPDATE accounts SET email = 'ada@example.com' WHERE id = 3",
            "INSERT INTO accounts (id, email) VALUES (4, 'four@example.com'), (1, 'bad@example.com')",
        ] {
            assert_eq!(
                engine.execute_sql(sql, &[]).unwrap_err().code,
                "CONSTRAINT_VIOLATION",
                "{sql}"
            );
        }
        for sql in [
            "CREATE TABLE later (id INTEGER PRIMARY KEY)",
            "CREATE INDEX accounts_active_tx ON accounts (active)",
            "ALTER TABLE accounts ADD COLUMN note TEXT",
            "DROP INDEX accounts_email",
            "DROP TABLE accounts",
        ] {
            assert_eq!(
                engine.execute_sql(sql, &[]).unwrap_err().code,
                "UNSUPPORTED_SQL",
                "{sql}"
            );
        }
        assert_eq!(
            engine
                .query_sql("SELECT id FROM accounts ORDER BY id", &[])
                .unwrap()
                .rows,
            vec![
                row(json!({"id": 1})),
                row(json!({"id": 2})),
                row(json!({"id": 3})),
            ]
        );
        assert_eq!(engine.revision(), revision);
        let outcome = engine.commit_transaction().unwrap();
        assert_eq!(outcome.revision, revision + 1);
        assert_eq!(outcome.tables, vec!["accounts"]);
    }

    #[test]
    fn rollback_empty_and_dirty_net_zero_transactions_have_exact_revision_semantics() {
        let mut engine = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
        let revision = engine.revision();

        engine.begin_transaction().unwrap();
        assert_eq!(
            engine.commit_transaction().unwrap(),
            ApplyOutcome {
                revision,
                tables: vec![],
                keys: ChangedKeys::default()
            }
        );

        engine.begin_transaction().unwrap();
        engine
            .execute_sql(
                "INSERT INTO accounts (id, email) VALUES (3, 'temp@example.com')",
                &[],
            )
            .unwrap();
        engine
            .execute_sql("DELETE FROM accounts WHERE id = 3", &[])
            .unwrap();
        let outcome = engine.commit_transaction().unwrap();
        assert_eq!(outcome.revision, revision + 1);
        assert_eq!(outcome.tables, vec!["accounts"]);
        assert_eq!(
            engine
                .query_sql("SELECT id FROM accounts ORDER BY id", &[])
                .unwrap()
                .rows,
            vec![row(json!({"id": 1})), row(json!({"id": 2}))]
        );

        engine.begin_transaction().unwrap();
        engine
            .execute_sql("DELETE FROM accounts WHERE id = 1", &[])
            .unwrap();
        engine.rollback_transaction().unwrap();
        assert_eq!(engine.revision(), revision + 1);
        assert!(
            engine
                .query_sql("SELECT id FROM accounts WHERE id = 1", &[])
                .unwrap()
                .rows
                .len()
                == 1
        );
        assert_eq!(
            engine.rollback_transaction().unwrap_err().code,
            "NO_ACTIVE_TRANSACTION"
        );
    }

    #[test]
    fn transaction_validates_unique_indexes_against_its_complete_final_state() {
        let mut engine = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine.begin_transaction().unwrap();
        // The committed unique postings must be interpreted through all staged deletes/upserts.
        engine
            .execute_sql("DELETE FROM accounts WHERE id IN (1, 2)", &[])
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO accounts (id, email) VALUES \
                 (1, 'lin@example.com'), (2, 'ada@example.com')",
                &[],
            )
            .unwrap();
        assert_eq!(
            engine
                .execute_sql(
                    "INSERT INTO accounts (id, email) VALUES (3, 'ada@example.com')",
                    &[],
                )
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        engine
            .commit_transaction()
            .expect("the staged unique-value swap is valid");
        assert_eq!(
            engine
                .query_sql("SELECT id, email FROM accounts ORDER BY id", &[])
                .unwrap()
                .rows,
            vec![
                row(json!({"id": 1, "email": "lin@example.com"})),
                row(json!({"id": 2, "email": "ada@example.com"})),
            ]
        );
    }

    #[test]
    fn prepublication_transaction_failure_retains_staged_work_for_retry() {
        let device = DurableDevice::default();
        let control = device.clone();
        let mut engine = page_native_fixture(device).unwrap();
        let revision = engine.revision();
        engine.begin_transaction().unwrap();
        engine
            .execute_sql(
                "INSERT INTO accounts (id, email) VALUES (3, 'grace@example.com')",
                &[],
            )
            .unwrap();
        control.arm_next_write();
        assert_eq!(engine.commit_transaction().unwrap_err().code, "INJECTED_IO");
        assert!(engine.in_transaction());
        assert_eq!(engine.revision(), revision);
        assert_eq!(
            engine
                .query_sql("SELECT email FROM accounts WHERE id = 3", &[])
                .unwrap()
                .rows,
            vec![row(json!({"email": "grace@example.com"}))]
        );

        let outcome = engine.commit_transaction().unwrap();
        assert_eq!(outcome.revision, revision + 1);
        assert!(!engine.in_transaction());
        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        reopened.check().unwrap();
        assert_eq!(reopened.revision(), revision + 1);
        assert_eq!(
            reopened
                .query_sql("SELECT id FROM accounts WHERE id = 3", &[])
                .unwrap()
                .rows,
            vec![row(json!({"id": 3}))]
        );
    }

    #[test]
    fn ambiguous_transaction_publication_requires_reopen_and_recovers_new_generation() {
        let device = DurableDevice::default();
        let control = device.clone();
        let mut engine = page_native_fixture(device).unwrap();
        let revision = engine.revision();
        engine.begin_transaction().unwrap();
        engine
            .execute_sql(
                "INSERT INTO accounts (id, email) VALUES (3, 'grace@example.com')",
                &[],
            )
            .unwrap();
        control.arm_after_flush(1);
        assert_eq!(
            engine.commit_transaction().unwrap_err().code,
            "RECOVERY_REQUIRED"
        );
        assert!(engine.in_transaction());
        assert_eq!(
            engine
                .query_sql("SELECT id FROM accounts", &[])
                .unwrap_err()
                .code,
            "RECOVERY_REQUIRED"
        );
        assert_eq!(
            engine.rollback_transaction().unwrap_err().code,
            "RECOVERY_REQUIRED"
        );
        assert_eq!(
            engine.begin_transaction().unwrap_err().code,
            "RECOVERY_REQUIRED"
        );
        assert_eq!(
            engine
                .execute_sql("CREATE TABLE later (id INTEGER PRIMARY KEY)", &[])
                .unwrap_err()
                .code,
            "RECOVERY_REQUIRED"
        );

        let device = engine.into_device();
        control.crash();
        let reopened = PagedEngine::open(device).unwrap();
        assert_eq!(reopened.revision(), revision + 1);
        assert_eq!(
            reopened
                .query_sql("SELECT id FROM accounts WHERE id = 3", &[])
                .unwrap()
                .rows,
            vec![row(json!({"id": 3}))]
        );
    }

    /// Numbers that need all 17 significant digits, such as about a tenth of `Math.random()`'s
    /// results, and extreme exponents. Parsing JSON or SQL text without full precision rounds
    /// many of them to a neighbouring float.
    const PRECISE_FLOATS: [f64; 6] = [
        0.9856906946328695,
        124.58223982731829,
        1.0715660391465826e-75,
        20.900000000000002,
        2.2250738585072014e-308,
        1.7976931348623157e308,
    ];

    #[test]
    fn json_and_sql_numbers_keep_every_bit_and_reopen() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql("CREATE TABLE numbers (id INTEGER PRIMARY KEY, doc JSON, f FLOAT)")
            .unwrap();
        for (id, value) in PRECISE_FLOATS.into_iter().enumerate() {
            engine
                .execute_sql(
                    "INSERT INTO numbers VALUES ($1, $2, $3)",
                    &[
                        json!(id),
                        json!({"x": value, "list": [value]}),
                        json!(value),
                    ],
                )
                .unwrap();
            // The same number written as SQL text.
            engine
                .execute_sql(
                    &format!("INSERT INTO numbers VALUES ({}, NULL, {value:e})", id + 100),
                    &[],
                )
                .unwrap();
        }
        let check = |engine: &PagedEngine<MemoryPageDevice>| {
            for (id, value) in PRECISE_FLOATS.into_iter().enumerate() {
                let rows = engine
                    .query_sql(
                        "SELECT doc, f FROM numbers WHERE id = $1 OR id = $2 ORDER BY id",
                        &[json!(id), json!(id + 100)],
                    )
                    .unwrap()
                    .rows;
                assert_eq!(rows[0]["doc"], json!({"x": value, "list": [value]}));
                for row in &rows {
                    assert_eq!(row["f"].as_f64().map(f64::to_bits), Some(value.to_bits()));
                }
                // A literal compares as the value it spells.
                let literal = format!("SELECT id FROM numbers WHERE f = {value:e} ORDER BY id");
                assert_eq!(engine.query_sql(&literal, &[]).unwrap().rows.len(), 2);
            }
        };
        check(&engine);
        // Reopening checks that every stored JSON value is canonical.
        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        reopened.check().unwrap();
        check(&reopened);
    }

    #[test]
    fn transaction_overlay_uses_canonical_typed_float_primary_keys() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .execute_sql(
                "CREATE TABLE float_keys (id FLOAT PRIMARY KEY, label TEXT NOT NULL)",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO float_keys (id, label) VALUES (0.0, 'zero'), (1, 'one')",
                &[],
            )
            .unwrap();
        engine.begin_transaction().unwrap();

        // Each replacement uses a different JSON number spelling for the same typed physical key.
        // They must collapse into one overlay entry and one final upsert, never an ordered
        // upsert-then-delete pair for the same B-tree key.
        engine
            .execute_sql("DELETE FROM float_keys WHERE id = 0.0", &[])
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO float_keys (id, label) VALUES (-0.0, 'negative zero')",
                &[],
            )
            .unwrap();
        engine
            .execute_sql("DELETE FROM float_keys WHERE id = 1", &[])
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO float_keys (id, label) VALUES (1.0, 'one point zero')",
                &[],
            )
            .unwrap();
        assert_eq!(
            engine
                .query_sql("SELECT label FROM float_keys WHERE id = 0", &[])
                .unwrap()
                .rows,
            vec![row(json!({"label": "negative zero"}))]
        );
        assert_eq!(
            engine
                .query_sql("SELECT label FROM float_keys WHERE id = 1", &[])
                .unwrap()
                .rows,
            vec![row(json!({"label": "one point zero"}))]
        );
        engine.commit_transaction().unwrap();

        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        reopened.check().unwrap();
        assert_eq!(
            reopened
                .query_sql("SELECT label FROM float_keys ORDER BY label", &[])
                .unwrap()
                .rows,
            vec![
                row(json!({"label": "negative zero"})),
                row(json!({"label": "one point zero"})),
            ]
        );
    }

    #[test]
    fn poisoned_storage_precedes_no_active_transaction_errors() {
        let device = DurableDevice::default();
        let control = device.clone();
        let mut engine = page_native_fixture(device).unwrap();
        assert!(!engine.in_transaction());
        control.arm_after_flush(1);
        assert_eq!(
            engine
                .execute_sql(
                    "INSERT INTO accounts (id, email) VALUES (3, 'grace@example.com')",
                    &[],
                )
                .unwrap_err()
                .code,
            "RECOVERY_REQUIRED"
        );
        assert_eq!(
            engine
                .execute_sql(
                    "INSERT INTO accounts (id, email) VALUES (4, 'lin@example.com')",
                    &[],
                )
                .unwrap_err()
                .code,
            "RECOVERY_REQUIRED"
        );
        assert_eq!(
            engine.commit_transaction().unwrap_err().code,
            "RECOVERY_REQUIRED"
        );
    }

    #[test]
    fn canonical_float_primary_key_collisions_fail_standalone_and_transaction_statements() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .execute_sql(
                "CREATE TABLE aliases (id FLOAT PRIMARY KEY, label TEXT NOT NULL)",
                &[],
            )
            .unwrap();
        let revision = engine.revision();
        let collision = "INSERT INTO aliases (id, label) VALUES (0.0, 'zero'), (-0.0, 'alias')";

        assert_eq!(
            engine.execute_sql(collision, &[]).unwrap_err().code,
            "CONSTRAINT_VIOLATION"
        );
        assert_eq!(engine.revision(), revision);
        assert!(
            engine
                .query_sql("SELECT id FROM aliases", &[])
                .unwrap()
                .rows
                .is_empty()
        );

        engine.begin_transaction().unwrap();
        engine
            .execute_sql("INSERT INTO aliases (id, label) VALUES (2, 'prior')", &[])
            .unwrap();
        assert_eq!(
            engine.execute_sql(collision, &[]).unwrap_err().code,
            "CONSTRAINT_VIOLATION"
        );
        assert_eq!(
            engine
                .query_sql("SELECT id, label FROM aliases ORDER BY id", &[])
                .unwrap()
                .rows,
            vec![row(json!({"id": 2.0, "label": "prior"}))]
        );
        let committed = engine.commit_transaction().unwrap();
        assert_eq!(committed.revision, revision + 1);
        assert_eq!(committed.tables, vec!["aliases"]);

        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        reopened.check().unwrap();
        assert_eq!(
            reopened
                .query_sql("SELECT id, label FROM aliases ORDER BY id", &[])
                .unwrap()
                .rows,
            vec![row(json!({"id": 2.0, "label": "prior"}))]
        );
    }

    #[test]
    fn update_allows_self_canonical_float_aliases_but_rejects_other_rows() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .execute_sql(
                "CREATE TABLE float_updates (id FLOAT PRIMARY KEY, label TEXT NOT NULL)",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO float_updates (id, label) VALUES (0.0, 'zero'), (1, 'one')",
                &[],
            )
            .unwrap();

        let standalone = engine
            .execute_sql(
                "UPDATE float_updates SET id = -0.0, label = 'standalone' \
                 WHERE id = 0 RETURNING label",
                &[],
            )
            .unwrap();
        assert_eq!(standalone.rows, vec![row(json!({"label": "standalone"}))]);
        let revision = engine.revision();
        assert_eq!(
            engine
                .execute_sql("UPDATE float_updates SET id = -0.0 WHERE id = 1", &[])
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        assert_eq!(engine.revision(), revision);

        engine.begin_transaction().unwrap();
        let staged = engine
            .execute_sql(
                "UPDATE float_updates SET id = 1.0, label = 'transaction' \
                 WHERE id = 1 RETURNING label",
                &[],
            )
            .unwrap();
        assert_eq!(staged.rows, vec![row(json!({"label": "transaction"}))]);
        assert_eq!(
            engine
                .query_sql("SELECT label FROM float_updates ORDER BY label", &[])
                .unwrap()
                .rows,
            vec![
                row(json!({"label": "standalone"})),
                row(json!({"label": "transaction"})),
            ]
        );
        engine.commit_transaction().unwrap();

        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        reopened.check().unwrap();
        assert_eq!(
            reopened
                .query_sql("SELECT id, label FROM float_updates ORDER BY id", &[])
                .unwrap()
                .rows,
            vec![
                row(json!({"id": -0.0, "label": "standalone"})),
                row(json!({"id": 1.0, "label": "transaction"})),
            ]
        );
    }

    #[test]
    fn a_column_declared_default_null_reopens() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql(
                "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT DEFAULT NULL, doc JSON);\
                 ALTER TABLE notes ADD COLUMN rating FLOAT DEFAULT NULL;\
                 INSERT INTO notes (id, doc) VALUES (1, '{\"a\":1}');",
            )
            .unwrap();
        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        reopened.check().unwrap();
        assert_eq!(
            reopened
                .query_sql("SELECT id, body, doc, rating FROM notes", &[])
                .unwrap()
                .rows,
            vec![row(
                json!({"id": 1, "body": null, "doc": "{\"a\":1}", "rating": null})
            )]
        );
    }

    #[test]
    fn exec_sql_requires_at_least_one_statement() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        for sql in ["", ";;;", "-- comment only ;\n/* still empty */"] {
            assert_eq!(engine.exec_sql(sql).unwrap_err().code, "INVALID_QUERY");
        }
        assert_eq!(engine.revision(), 0);
    }

    #[test]
    fn exec_sql_publishes_mixed_ddl_dml_and_select_once_and_reopens() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        let results = engine
            .exec_sql(
                "-- the comment's semicolon is not a statement ;\n\
                 CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
                 INSERT INTO notes (id, body) VALUES (1, 'one;two'), (2, 'second');\
                 CREATE INDEX notes_body ON notes (body);\
                 SELECT id, body FROM notes ORDER BY id;;;",
            )
            .unwrap();

        assert_eq!(results.len(), 4);
        assert_eq!(
            results
                .iter()
                .map(|result| result.command.as_str())
                .collect::<Vec<_>>(),
            ["CREATE TABLE", "INSERT", "CREATE INDEX", "SELECT"]
        );
        assert!(results.iter().all(|result| result.revision == 1));
        assert_eq!(
            results[3].rows,
            vec![
                row(json!({"id": 1, "body": "one;two"})),
                row(json!({"id": 2, "body": "second"})),
            ]
        );
        assert_eq!(engine.revision(), 1);

        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        reopened.check().unwrap();
        assert_eq!(reopened.revision(), 1);
        assert_eq!(
            reopened
                .query_sql("SELECT id FROM notes WHERE body = 'second'", &[])
                .unwrap()
                .rows,
            vec![row(json!({"id": 2}))]
        );
    }

    #[test]
    fn exec_sql_late_failure_aborts_pages_catalog_revision_and_reopen() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql(
                "CREATE TABLE accounts (id INTEGER PRIMARY KEY, email TEXT);\
                 INSERT INTO accounts (id, email) VALUES (1, 'one'), (2, 'two');\
                 CREATE UNIQUE INDEX accounts_email ON accounts (email);",
            )
            .unwrap();
        let revision = engine.revision();
        let before = engine
            .query_sql("SELECT id, email FROM accounts ORDER BY id", &[])
            .unwrap();

        assert_eq!(
            engine
                .exec_sql(
                    "UPDATE accounts SET email = 'changed' WHERE id = 1;\
                     CREATE TABLE should_not_exist (id INTEGER PRIMARY KEY);\
                     INSERT INTO accounts (id, email) VALUES (3, 'two');",
                )
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        assert_eq!(engine.revision(), revision);
        assert_eq!(
            engine
                .query_sql("SELECT id, email FROM accounts ORDER BY id", &[])
                .unwrap(),
            before
        );
        assert_eq!(
            engine
                .query_sql("SELECT id FROM should_not_exist", &[])
                .unwrap_err()
                .code,
            "TABLE_NOT_FOUND"
        );

        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        reopened.check().unwrap();
        assert_eq!(reopened.revision(), revision);
        assert_eq!(
            reopened
                .query_sql("SELECT id, email FROM accounts ORDER BY id", &[])
                .unwrap(),
            before
        );
    }

    #[test]
    fn transaction_exec_is_a_savepoint_and_preflights_ddl() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql(
                "CREATE TABLE accounts (id INTEGER PRIMARY KEY, label TEXT NOT NULL);\
                 INSERT INTO accounts (id, label) VALUES (1, 'base');",
            )
            .unwrap();
        let revision = engine.revision();
        engine.begin_transaction().unwrap();
        engine
            .execute_sql("INSERT INTO accounts (id, label) VALUES (2, 'prior')", &[])
            .unwrap();

        assert_eq!(
            engine
                .exec_sql(
                    "INSERT INTO accounts (id, label) VALUES (3, 'temporary');\
                     UPDATE accounts SET label = 'changed' WHERE id = 1;\
                     INSERT INTO accounts (id, label) VALUES (2, 'duplicate');",
                )
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        assert_eq!(
            engine
                .query_sql("SELECT id, label FROM accounts ORDER BY id", &[])
                .unwrap()
                .rows,
            vec![
                row(json!({"id": 1, "label": "base"})),
                row(json!({"id": 2, "label": "prior"})),
            ]
        );

        assert_eq!(
            engine
                .exec_sql(
                    "INSERT INTO accounts (id, label) VALUES (4, 'blocked');\
                     CREATE TABLE blocked (id INTEGER PRIMARY KEY);",
                )
                .unwrap_err()
                .code,
            "UNSUPPORTED_SQL"
        );
        assert!(
            engine
                .query_sql("SELECT id FROM accounts WHERE id = 4", &[])
                .unwrap()
                .rows
                .is_empty()
        );

        let results = engine
            .exec_sql(
                "INSERT INTO accounts (id, label) VALUES (3, 'installed');\
                 SELECT id, label FROM accounts ORDER BY id;",
            )
            .unwrap();
        assert!(results.iter().all(|result| result.revision == revision));
        assert_eq!(results[1].rows.len(), 3);
        let outcome = engine.commit_transaction().unwrap();
        assert_eq!(outcome.revision, revision + 1);

        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        reopened.check().unwrap();
        assert_eq!(
            reopened
                .query_sql("SELECT id FROM accounts ORDER BY id", &[])
                .unwrap()
                .rows,
            vec![
                row(json!({"id": 1})),
                row(json!({"id": 2})),
                row(json!({"id": 3})),
            ]
        );
    }

    #[test]
    fn exec_sql_rejects_cumulative_results_before_publishing() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .execute_sql(
                "CREATE TABLE blobs (\
                    id INTEGER PRIMARY KEY, payload TEXT NOT NULL, marker INTEGER NOT NULL DEFAULT 0\
                 )",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO blobs (id, payload) VALUES (1, $1)",
                &[json!("x".repeat(80_000))],
            )
            .unwrap();
        let revision = engine.revision();
        let script = std::iter::once("UPDATE blobs SET marker = 1 WHERE id = 1;")
            .chain(std::iter::repeat_n(
                "SELECT payload FROM blobs WHERE id = 1;",
                220,
            ))
            .collect::<String>();

        assert_eq!(
            engine.exec_sql(&script).unwrap_err().code,
            "TRANSACTION_TOO_LARGE"
        );
        assert_eq!(engine.revision(), revision);
        assert_eq!(
            engine
                .query_sql("SELECT marker FROM blobs WHERE id = 1", &[])
                .unwrap()
                .rows,
            vec![row(json!({"marker": 0}))]
        );
        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        reopened.check().unwrap();
        assert_eq!(reopened.revision(), revision);
        assert_eq!(
            reopened
                .query_sql("SELECT marker FROM blobs WHERE id = 1", &[])
                .unwrap()
                .rows,
            vec![row(json!({"marker": 0}))]
        );
    }

    #[test]
    fn exec_sql_bounds_cumulative_scans_across_small_aggregate_results() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .execute_sql(
                "CREATE TABLE numbers (\
                    id INTEGER PRIMARY KEY, marker BOOLEAN NOT NULL DEFAULT false\
                 )",
                &[],
            )
            .unwrap();
        for start in (0..4_000).step_by(500) {
            let values = (start..start + 500)
                .map(|id| format!("({id})"))
                .collect::<Vec<_>>()
                .join(",");
            engine
                .execute_sql(&format!("INSERT INTO numbers (id) VALUES {values}"), &[])
                .unwrap();
        }
        let revision = engine.revision();
        let script = std::iter::once("UPDATE numbers SET marker = true WHERE id = 0;")
            .chain(std::iter::repeat_n(
                "SELECT COUNT(*) AS count FROM numbers;",
                250,
            ))
            .collect::<String>();

        assert_eq!(
            engine.exec_sql(&script).unwrap_err().code,
            "TRANSACTION_TOO_LARGE"
        );
        assert_eq!(engine.revision(), revision);
        assert_eq!(
            engine
                .query_sql("SELECT marker FROM numbers WHERE id = 0", &[])
                .unwrap()
                .rows,
            vec![row(json!({"marker": false}))]
        );
    }

    #[test]
    fn exec_sql_shares_join_work_budget_and_rolls_back_earlier_mutations() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql(
                "CREATE TABLE marker (id INTEGER PRIMARY KEY, changed BOOLEAN NOT NULL); \
                 INSERT INTO marker VALUES (1, false); \
                 CREATE TABLE a (id INTEGER PRIMARY KEY, join_key INTEGER NOT NULL); \
                 CREATE TABLE b (id INTEGER PRIMARY KEY, join_key INTEGER NOT NULL)",
            )
            .unwrap();
        for (table, count) in [("a", 600), ("b", 1_000)] {
            for start in (0..count).step_by(500) {
                let values = (start..(start + 500).min(count))
                    .map(|id| format!("({id}, 1)"))
                    .collect::<Vec<_>>()
                    .join(",");
                engine
                    .execute_sql(&format!("INSERT INTO {table} VALUES {values}"), &[])
                    .unwrap();
            }
        }
        let join =
            "SELECT a.id AS id FROM a JOIN b ON a.join_key = b.join_key WHERE a.id < 0 OR b.id < 0";
        // Each query examines 600,000 pairs plus its scanned rows. Separate
        // requests fit, but two joins in one script share the work budget.
        for _ in 0..2 {
            assert!(engine.execute_sql(join, &[]).unwrap().rows.is_empty());
        }
        let revision = engine.revision();
        let script = format!("UPDATE marker SET changed = true WHERE id = 1; {join}; {join}");

        assert_eq!(
            engine.exec_sql(&script).unwrap_err().code,
            "TRANSACTION_TOO_LARGE"
        );
        assert_eq!(engine.revision(), revision);
        assert_eq!(
            engine
                .query_sql("SELECT changed FROM marker WHERE id = 1", &[])
                .unwrap()
                .rows,
            vec![row(json!({"changed": false}))]
        );
        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        reopened.check().unwrap();
        assert_eq!(reopened.revision(), revision);
        assert_eq!(
            reopened
                .query_sql("SELECT changed FROM marker WHERE id = 1", &[])
                .unwrap()
                .rows,
            vec![row(json!({"changed": false}))]
        );
    }

    #[test]
    fn heterogeneous_json_predicates_preserve_structural_equality_and_sql_nulls() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .execute_sql(
                "CREATE TABLE items (id INTEGER PRIMARY KEY, payload JSON)",
                &[],
            )
            .unwrap();
        let values = [
            Value::Null,
            json!(false),
            json!(true),
            json!(0),
            json!(1),
            json!("one"),
            json!([]),
            json!([1]),
            json!({}),
            json!({"a": [1, true]}),
        ];
        for (id, value) in values.iter().enumerate() {
            engine
                .execute_sql(
                    "INSERT INTO items VALUES ($1, $2)",
                    &[json!(id), value.clone()],
                )
                .unwrap();
        }
        let predicates = [
            "= $1",
            "<> $1",
            "IN ($1, NULL)",
            "NOT IN ($1)",
            "NOT IN ($1, NULL)",
        ];
        let statements = predicates
            .iter()
            .map(|predicate| {
                let sql = format!("SELECT id FROM items WHERE payload {predicate} ORDER BY id");
                let prepared = engine.prepare_sql(&sql).unwrap();
                (sql, prepared)
            })
            .collect::<Vec<_>>();

        for transaction in [false, true] {
            if transaction {
                engine.begin_transaction().unwrap();
            }
            for (id, value) in values.iter().enumerate() {
                for (predicate, (sql, prepared)) in statements.iter().enumerate() {
                    let expected_ids = match (id, predicate) {
                        (0, _) | (_, 4) => Vec::new(),
                        (_, 0 | 2) => vec![id],
                        (_, 1 | 3) => (1..values.len()).filter(|other| *other != id).collect(),
                        _ => unreachable!(),
                    };
                    let expected = expected_ids
                        .into_iter()
                        .map(|id| row(json!({"id": id})))
                        .collect::<Vec<_>>();
                    assert_eq!(
                        engine
                            .execute_sql(sql, std::slice::from_ref(value))
                            .unwrap()
                            .rows,
                        expected,
                        "direct: {sql}, parameter {value}, transaction {transaction}"
                    );
                    assert_eq!(
                        engine
                            .execute_prepared(*prepared, std::slice::from_ref(value))
                            .unwrap()
                            .rows,
                        expected,
                        "prepared: {sql}, parameter {value}, transaction {transaction}"
                    );
                }
            }
            if transaction {
                engine.rollback_transaction().unwrap();
            }
        }

        let object = json!({"a": [1, true]});
        assert_eq!(
            engine
                .execute_sql(
                    "SELECT COUNT(*) AS count FROM items WHERE payload = $1",
                    std::slice::from_ref(&object)
                )
                .unwrap()
                .rows,
            vec![row(json!({"count": 1}))]
        );
        assert_eq!(
            engine.execute_sql("SELECT a.id FROM items AS a JOIN items AS b ON a.id = b.id WHERE a.payload = $1", std::slice::from_ref(&object)).unwrap().rows,
            vec![row(json!({"id": 9}))]
        );
        engine.begin_transaction().unwrap();
        assert_eq!(
            engine
                .execute_sql(
                    "UPDATE items SET payload = $1 WHERE payload = $2 RETURNING id",
                    &[json!("changed"), object]
                )
                .unwrap()
                .rows,
            vec![row(json!({"id": 9}))]
        );
        assert_eq!(
            engine
                .execute_sql(
                    "DELETE FROM items WHERE payload = $1 RETURNING id",
                    &[json!("changed")]
                )
                .unwrap()
                .rows,
            vec![row(json!({"id": 9}))]
        );
        engine.rollback_transaction().unwrap();
    }

    #[test]
    fn dml_rejects_invalid_predicates_and_assignments_independently_of_matches() {
        let invalid = [
            (
                "UPDATE items SET n = 1 WHERE payload > 2",
                vec![],
                "TYPE_MISMATCH",
            ),
            (
                "DELETE FROM items WHERE payload > 2",
                vec![],
                "TYPE_MISMATCH",
            ),
            (
                "UPDATE items SET n = 1 WHERE id = 99 AND n = 'wrong'",
                vec![],
                "TYPE_MISMATCH",
            ),
            (
                "DELETE FROM items WHERE id = 99 AND n = 'wrong'",
                vec![],
                "TYPE_MISMATCH",
            ),
            (
                "UPDATE items SET n = $1 WHERE id = 99",
                vec![json!("wrong")],
                "TYPE_MISMATCH",
            ),
            (
                "UPDATE items SET n = 1.25 WHERE id = 99",
                vec![],
                "TYPE_MISMATCH",
            ),
            (
                "UPDATE items SET n = NULL WHERE id = 99",
                vec![],
                "CONSTRAINT_VIOLATION",
            ),
            (
                "UPDATE items SET required = DEFAULT WHERE id = 99",
                vec![],
                "CONSTRAINT_VIOLATION",
            ),
        ];
        for populated in [false, true] {
            for transaction in [false, true] {
                let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
                engine.execute_sql("CREATE TABLE items (id INTEGER PRIMARY KEY, n INTEGER NOT NULL DEFAULT 0, payload JSON, required TEXT NOT NULL)", &[]).unwrap();
                if populated {
                    engine
                        .execute_sql(
                            "INSERT INTO items (id, payload, required) VALUES (1, 3, 'base')",
                            &[],
                        )
                        .unwrap();
                }
                let prepared = invalid
                    .iter()
                    .map(|(sql, _, _)| engine.prepare_sql(sql).unwrap())
                    .collect::<Vec<_>>();
                if transaction {
                    engine.begin_transaction().unwrap();
                    if populated {
                        engine
                            .execute_sql(
                                "INSERT INTO items (id, payload, required) VALUES (2, 4, 'staged')",
                                &[],
                            )
                            .unwrap();
                    }
                }
                let before = engine
                    .execute_sql("SELECT * FROM items ORDER BY id", &[])
                    .unwrap();
                for ((sql, params, code), prepared) in invalid.iter().zip(prepared) {
                    assert_eq!(
                        engine.execute_sql(sql, params).unwrap_err().code,
                        *code,
                        "direct: {sql}, populated {populated}, transaction {transaction}"
                    );
                    assert_eq!(
                        engine.execute_prepared(prepared, params).unwrap_err().code,
                        *code,
                        "prepared: {sql}, populated {populated}, transaction {transaction}"
                    );
                    assert_eq!(
                        engine
                            .execute_sql("SELECT * FROM items ORDER BY id", &[])
                            .unwrap(),
                        before
                    );
                    assert_eq!(engine.in_transaction(), transaction);
                }
                // A valid default remains valid even when the UPDATE does not match a row.
                assert_eq!(
                    engine
                        .execute_sql("UPDATE items SET n = DEFAULT WHERE id = 99", &[])
                        .unwrap()
                        .row_count,
                    0
                );
                if transaction {
                    engine.commit_transaction().unwrap();
                }
                let reopened = PagedEngine::open(engine.into_device()).unwrap();
                reopened.check().unwrap();
                assert_eq!(
                    reopened
                        .query_sql("SELECT * FROM items ORDER BY id", &[])
                        .unwrap()
                        .rows,
                    before.rows
                );
            }
        }
    }

    fn keys_for(outcome: &ApplyOutcome, table: &str) -> Option<Vec<Value>> {
        outcome
            .keys
            .get(table)
            .map(|rows| rows.iter().map(|row| row["id"].clone()).collect())
    }

    #[test]
    fn a_write_reports_the_primary_keys_it_changed() {
        let mut engine = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();

        let inserted = engine
            .execute_sql(
                "INSERT INTO accounts (id, email) VALUES (3, 'grace@example.com')",
                &[],
            )
            .unwrap();
        assert_eq!(inserted.keys["accounts"], vec![row(json!({"id": 3}))]);

        let updated = engine
            .execute_sql("UPDATE accounts SET active = true WHERE id = 1", &[])
            .unwrap();
        assert_eq!(updated.keys["accounts"], vec![row(json!({"id": 1}))]);

        // A delete names the row that is going away, which is the only chance to observe it.
        let deleted = engine
            .execute_sql("DELETE FROM accounts WHERE id = 2", &[])
            .unwrap();
        assert_eq!(deleted.keys["accounts"], vec![row(json!({"id": 2}))]);

        // A statement that matches nothing changed nothing, and says so.
        let missed = engine
            .execute_sql("DELETE FROM accounts WHERE id = 99", &[])
            .unwrap();
        assert!(missed.tables.is_empty());
        assert!(missed.keys.is_empty());
    }

    #[test]
    fn a_transaction_reports_every_key_it_committed_exactly_once() {
        let mut engine = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();

        engine.begin_transaction().unwrap();
        engine
            .execute_sql(
                "INSERT INTO accounts (id, email) VALUES (3, 'grace@example.com')",
                &[],
            )
            .unwrap();
        // Touching the same row twice must still report it once: keys are a set.
        engine
            .execute_sql("UPDATE accounts SET active = true WHERE id = 3", &[])
            .unwrap();
        engine
            .execute_sql("DELETE FROM accounts WHERE id = 1", &[])
            .unwrap();
        let outcome = engine.commit_transaction().unwrap();

        assert_eq!(outcome.tables, vec!["accounts"]);
        assert_eq!(
            keys_for(&outcome, "accounts"),
            Some(vec![json!(1), json!(3)])
        );
    }

    #[test]
    fn keys_are_reported_once_each_in_primary_key_order() {
        let mut engine = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
        let ids = |outcome: &ExecuteResult| keys_for_result(outcome, "accounts");

        // Values in no order, and numbers whose text sorts otherwise, report in key order.
        let inserted = engine
            .execute_sql(
                "INSERT INTO accounts (id, email) VALUES \
                 (100, 'a@x'), (9, 'b@x'), (10, 'c@x'), (3, 'd@x')",
                &[],
            )
            .unwrap();
        assert_eq!(ids(&inserted), vec![3, 9, 10, 100]);

        // So do rows a scan visits in index order, and a transaction's statements and commit.
        engine.begin_transaction().unwrap();
        let updated = engine
            .execute_sql(
                "UPDATE accounts SET active = true WHERE email < $1",
                &[json!("c")],
            )
            .unwrap();
        assert_eq!(ids(&updated), vec![1, 9, 100]);
        // A key that moves reports both the key it leaves and the one it takes, once each.
        let moved = engine
            .execute_sql("UPDATE accounts SET id = 4 WHERE id = 100", &[])
            .unwrap();
        assert_eq!(ids(&moved), vec![4, 100]);
        let deleted = engine
            .execute_sql("DELETE FROM accounts WHERE email > $1", &[json!("a@x")])
            .unwrap();
        assert_eq!(ids(&deleted), vec![1, 2, 3, 9, 10]);
        let outcome = engine.commit_transaction().unwrap();
        assert_eq!(
            keys_for(&outcome, "accounts"),
            Some([1, 2, 3, 4, 9, 10, 100].map(|id| json!(id)).to_vec())
        );
    }

    #[test]
    fn a_composite_key_lists_its_columns_in_key_order() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .execute_sql(
                "CREATE TABLE members (team TEXT, id INTEGER, role TEXT, PRIMARY KEY (team, id))",
                &[],
            )
            .unwrap();
        let inserted = engine
            .execute_sql(
                "INSERT INTO members (team, id, role) VALUES ('b', 1, 'x'), ('a', 2, 'y')",
                &[],
            )
            .unwrap();
        let keys = inserted.keys.get("members").unwrap();
        assert_eq!(keys.columns, ["team", "id"]);
        assert_eq!(keys.values, [json!("a"), json!(2), json!("b"), json!(1)]);

        // A transaction's statements and its commit list them the same way.
        engine.begin_transaction().unwrap();
        let updated = engine
            .execute_sql("UPDATE members SET role = 'z' WHERE team = 'b'", &[])
            .unwrap();
        assert_eq!(updated.keys.get("members").unwrap().columns, ["team", "id"]);
        let outcome = engine.commit_transaction().unwrap();
        let keys = outcome.keys.get("members").unwrap();
        assert_eq!(keys.columns, ["team", "id"]);
        assert_eq!(keys.values, [json!("b"), json!(1)]);
    }

    fn keys_for_result(result: &ExecuteResult, table: &str) -> Vec<u64> {
        result.keys[table]
            .iter()
            .map(|key| key["id"].as_u64().unwrap())
            .collect()
    }

    /// Seeds `count` accounts in batches small enough to stay inside the SQL token limit.
    fn seed_accounts<D: PageDevice>(engine: &mut PagedEngine<D>, count: usize) {
        for batch in (0..count).collect::<Vec<_>>().chunks(200) {
            let rows = batch
                .iter()
                .map(|index| format!("({}, 'user{index}@example.com')", index + 10))
                .collect::<Vec<_>>()
                .join(", ");
            engine
                .execute_sql(
                    &format!("INSERT INTO accounts (id, email) VALUES {rows}"),
                    &[],
                )
                .unwrap();
        }
    }

    #[test]
    fn a_write_past_the_reporting_bound_names_no_keys_rather_than_some() {
        // One statement changing every row is the cheap way to cross the bound; the seeding
        // inserts above it are batched only to stay inside the SQL token limit.
        let mut engine = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
        seed_accounts(&mut engine, MAX_CHANGED_KEYS_PER_TABLE + 1);
        let outcome = engine
            .execute_sql("UPDATE accounts SET active = true", &[])
            .unwrap();

        // The table still reports as changed; only the per-row detail is withheld, so a
        // subscriber re-reads rather than mistaking a partial list for a complete one.
        assert_eq!(outcome.tables, vec!["accounts"]);
        assert!(!outcome.keys.contains_key("accounts"));
        // Deletes, planned by their stored keys, are bounded the same way.
        let outcome = engine.execute_sql("DELETE FROM accounts", &[]).unwrap();
        assert_eq!(
            (outcome.row_count, outcome.tables),
            (MAX_CHANGED_KEYS_PER_TABLE + 3, vec!["accounts".to_owned()])
        );
        assert!(!outcome.keys.contains_key("accounts"));

        // Exactly at the bound the keys are still reported in full.
        let mut engine = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
        // The fixture already holds two accounts, so seed the remainder.
        seed_accounts(&mut engine, MAX_CHANGED_KEYS_PER_TABLE - 2);
        let outcome = engine
            .execute_sql("UPDATE accounts SET active = true", &[])
            .unwrap();
        assert_eq!(outcome.keys["accounts"].len(), MAX_CHANGED_KEYS_PER_TABLE);
        let outcome = engine.execute_sql("DELETE FROM accounts", &[]).unwrap();
        let mut deleted = outcome.keys["accounts"]
            .iter()
            .map(|key| key["id"].as_u64().unwrap() as usize)
            .collect::<Vec<_>>();
        deleted.sort_unstable();
        let mut expected = vec![1, 2];
        expected.extend(10..10 + MAX_CHANGED_KEYS_PER_TABLE - 2);
        assert_eq!(deleted, expected);
    }

    /// A table of users for statements that qualify its columns.
    fn users_engine() -> PagedEngine<MemoryPageDevice> {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT, \
                   active BOOLEAN NOT NULL DEFAULT true);\
                 CREATE INDEX users_name ON users (name);\
                 INSERT INTO users (id, name, email, active) VALUES \
                   (1, 'cy', 'c@example.com', true), (2, 'ann', NULL, false), \
                   (3, 'bob', 'b@example.com', false);",
            )
            .unwrap();
        engine
    }

    #[test]
    fn a_statement_over_one_table_reads_columns_qualified_by_its_name() {
        // A qualified statement is the statement its plain column names spell, so it is planned,
        // narrowed and validated the same way.
        for (qualified, plain) in [
            (
                "SELECT users.id, \"users\".name AS who FROM users \
                 WHERE users.active = $1 AND (USERS.id IN (1, 2) OR users.email IS NULL) \
                 ORDER BY users.name DESC LIMIT 2",
                "SELECT id, name AS who FROM users \
                 WHERE active = $1 AND (id IN (1, 2) OR email IS NULL) \
                 ORDER BY name DESC LIMIT 2",
            ),
            (
                "SELECT users.* FROM users WHERE users.name LIKE 'a%'",
                "SELECT * FROM users WHERE name LIKE 'a%'",
            ),
            (
                "SELECT DISTINCT users.active FROM users ORDER BY users.active",
                "SELECT DISTINCT active FROM users ORDER BY active",
            ),
            (
                "SELECT users.active, COUNT(users.id) AS n FROM users \
                 WHERE users.id BETWEEN 1 AND 3 GROUP BY users.active ORDER BY users.active",
                "SELECT active, COUNT(id) AS n FROM users \
                 WHERE id BETWEEN 1 AND 3 GROUP BY active ORDER BY active",
            ),
            (
                "UPDATE users SET name = $1 WHERE users.id = 2 RETURNING users.id, users.name",
                "UPDATE users SET name = $1 WHERE id = 2 RETURNING id, name",
            ),
            (
                "DELETE FROM users WHERE users.id = 2 RETURNING users.*",
                "DELETE FROM users WHERE id = 2 RETURNING *",
            ),
            (
                "INSERT INTO users (id, name) VALUES (4, $1) RETURNING users.id",
                "INSERT INTO users (id, name) VALUES (4, $1) RETURNING id",
            ),
            (
                "SELECT notes.id FROM app.notes WHERE \"notes\".id = 1",
                "SELECT id FROM app.notes WHERE id = 1",
            ),
        ] {
            let parse = |sql| format!("{:?}", crate::statement::parse(sql, &[json!(true)]));
            assert!(parse(plain).starts_with("Ok("), "{plain}");
            assert_eq!(parse(qualified), parse(plain), "{qualified}");
        }

        let mut engine = users_engine();
        let names = |engine: &PagedEngine<MemoryPageDevice>, sql: &str| {
            engine
                .query_sql(sql, &[])
                .unwrap()
                .rows
                .into_iter()
                .map(|row| row.values().next().unwrap().clone())
                .collect::<Vec<_>>()
        };

        // A plain ORDER BY name is an output's before a column's. Behind the table's name it is
        // the table's column, whatever the outputs are called.
        assert_eq!(
            names(
                &engine,
                "SELECT name AS id FROM users ORDER BY users.id DESC"
            ),
            [json!("bob"), json!("ann"), json!("cy")]
        );
        assert_eq!(
            names(&engine, "SELECT name AS id FROM users ORDER BY id DESC"),
            [json!("cy"), json!("bob"), json!("ann")]
        );
        // An aggregate query is ordered by its outputs, so the table's column stands for the
        // output that returns it, and for no other output of the same name.
        assert_eq!(
            names(
                &engine,
                "SELECT active AS a, COUNT(*) AS active FROM users GROUP BY active \
                 ORDER BY users.active"
            ),
            [json!(false), json!(true)]
        );
        assert_eq!(
            names(
                &engine,
                "SELECT active AS a, COUNT(*) AS active FROM users GROUP BY active \
                 ORDER BY active"
            ),
            [json!(true), json!(false)]
        );

        // A prepared statement is qualified the same way, with its parameters where they were.
        let statement = engine
            .prepare_sql(
                "SELECT users.name FROM users WHERE users.id > $1 ORDER BY users.id LIMIT $2",
            )
            .unwrap();
        assert_eq!(
            engine
                .execute_prepared(statement, &[json!(1), json!(1)])
                .unwrap()
                .rows,
            vec![row(json!({"name": "ann"}))]
        );
        assert_eq!(
            engine
                .execute_sql(
                    "UPDATE users SET name = $1 WHERE users.id = 2 RETURNING users.id, users.name",
                    &[json!("anne")],
                )
                .unwrap()
                .rows,
            vec![row(json!({"id": 2, "name": "anne"}))]
        );

        // Only the table's own name qualifies, in the clauses that read its columns.
        for (sql, code) in [
            ("SELECT posts.id FROM users", "COLUMN_NOT_FOUND"),
            (
                "SELECT id FROM users WHERE posts.id = 1",
                "COLUMN_NOT_FOUND",
            ),
            ("SELECT id FROM users ORDER BY posts.id", "UNSUPPORTED_SQL"),
            (
                "SELECT id FROM users WHERE users.id.x = 1",
                "UNSUPPORTED_SQL",
            ),
            (
                "SELECT id FROM users WHERE app.users.id = 1",
                "UNSUPPORTED_SQL",
            ),
            ("SELECT users.missing FROM users", "COLUMN_NOT_FOUND"),
            (
                "SELECT COUNT(*) FROM users GROUP BY active ORDER BY users.count",
                "COLUMN_NOT_FOUND",
            ),
            (
                "UPDATE users SET users.name = 'x' WHERE id = 1",
                "SQL_PARSE_ERROR",
            ),
            (
                "INSERT INTO users (users.id, name) VALUES (9, 'x')",
                "SQL_PARSE_ERROR",
            ),
            ("DELETE FROM users WHERE users.key = 1", "SQL_PARSE_ERROR"),
        ] {
            assert_eq!(
                engine.execute_sql(sql, &[]).unwrap_err().code,
                code,
                "{sql}"
            );
        }
    }

    #[test]
    fn a_table_alias_qualifies_a_single_table_statement_in_place_of_its_name() {
        // An alias, with or without AS, is dropped with the qualifiers it gives.
        for (aliased, plain) in [
            (
                "SELECT u.id, u.name AS who FROM users AS u WHERE u.active = $1 ORDER BY u.name",
                "SELECT id, name AS who FROM users WHERE active = $1 ORDER BY name",
            ),
            (
                "SELECT \"U\".* FROM users \"U\" WHERE \"U\".id = 1",
                "SELECT * FROM users WHERE id = 1",
            ),
            (
                "SELECT * FROM users users LIMIT 1",
                "SELECT * FROM users LIMIT 1",
            ),
            (
                "SELECT u.active, COUNT(*) FROM users u GROUP BY u.active ORDER BY u.active",
                "SELECT active, COUNT(*) FROM users GROUP BY active ORDER BY active",
            ),
            (
                "SELECT DISTINCT n.id FROM app.notes AS n",
                "SELECT DISTINCT id FROM app.notes",
            ),
            (
                "UPDATE users AS u SET name = $1 WHERE u.id = 2 RETURNING u.name",
                "UPDATE users SET name = $1 WHERE id = 2 RETURNING name",
            ),
            (
                "UPDATE users u SET active = false",
                "UPDATE users SET active = false",
            ),
            (
                "DELETE FROM users AS u WHERE u.id = 2 RETURNING u.*",
                "DELETE FROM users WHERE id = 2 RETURNING *",
            ),
            ("DELETE FROM users u", "DELETE FROM users"),
        ] {
            let parse = |sql| format!("{:?}", crate::statement::parse(sql, &[json!(true)]));
            assert!(parse(plain).starts_with("Ok("), "{plain}");
            assert_eq!(parse(aliased), parse(plain), "{aliased}");
        }

        let mut engine = users_engine();
        assert_eq!(
            engine
                .execute_sql(
                    "SELECT u.name AS id FROM users u WHERE u.active = false ORDER BY u.id DESC",
                    &[],
                )
                .unwrap()
                .rows,
            vec![row(json!({"id": "bob"})), row(json!({"id": "ann"}))]
        );
        let renamed = engine
            .execute_sql(
                "UPDATE users AS u SET name = 'anne' WHERE u.id = 2 RETURNING u.id, u.name",
                &[],
            )
            .unwrap();
        assert_eq!(renamed.rows, vec![row(json!({"id": 2, "name": "anne"}))]);
        assert_eq!(renamed.tables, vec!["users"]);

        // An alias hides the table's name, is one unreserved word, and is not taken by INSERT.
        for (sql, code) in [
            ("SELECT users.id FROM users u", "COLUMN_NOT_FOUND"),
            (
                "SELECT id FROM users u WHERE users.id = 1",
                "COLUMN_NOT_FOUND",
            ),
            ("SELECT id FROM users AS", "UNSUPPORTED_SQL"),
            ("SELECT id FROM users AS where", "UNSUPPORTED_SQL"),
            ("SELECT id FROM users AS \"\"", "UNSUPPORTED_SQL"),
            ("SELECT id FROM users u v", "UNSUPPORTED_SQL"),
            ("SELECT id FROM users AS u (id)", "UNSUPPORTED_SQL"),
            (
                "UPDATE users u SET u.name = 'x' WHERE u.id = 1",
                "SQL_PARSE_ERROR",
            ),
            ("DELETE FROM users u v", "UNSUPPORTED_SQL"),
            (
                "INSERT INTO users AS u (id, name) VALUES (9, 'x')",
                "SQL_PARSE_ERROR",
            ),
        ] {
            assert_eq!(
                engine.execute_sql(sql, &[]).unwrap_err().code,
                code,
                "{sql}"
            );
        }
    }

    #[test]
    fn array_rows_hold_fields_of_one_name_under_their_positions() {
        let mut engine = users_engine();
        engine
            .exec_sql(
                "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, \
                   title TEXT NOT NULL);\
                 INSERT INTO posts VALUES (10, 1, 'first'), (11, 3, 'second');",
            )
            .unwrap();
        let revision = engine.revision();
        let names = |result: &ExecuteResult| {
            result
                .fields
                .iter()
                .map(|field| field.name.clone())
                .collect::<Vec<_>>()
        };

        // Each shape of SELECT returns its fields under the names it gave them, and its rows
        // under their positions.
        let join = "SELECT users.id, posts.id, posts.title FROM users \
                    JOIN posts ON users.id = posts.user_id ORDER BY posts.id DESC";
        for (sql, fields, rows) in [
            (
                "SELECT id, name, id AS name FROM users WHERE id = 1",
                vec!["id", "name", "name"],
                vec![json!({"000id": 1, "001name": "cy", "002name": 1})],
            ),
            (
                join,
                vec!["id", "id", "title"],
                vec![
                    json!({"000id": 3, "001id": 11, "002title": "second"}),
                    json!({"000id": 1, "001id": 10, "002title": "first"}),
                ],
            ),
            (
                "SELECT users.id, posts.id, posts.title FROM users \
                 JOIN posts ON users.id = posts.user_id ORDER BY title LIMIT 1",
                vec!["id", "id", "title"],
                vec![json!({"000id": 1, "001id": 10, "002title": "first"})],
            ),
            (
                "SELECT COUNT(*), active, COUNT(email) FROM users GROUP BY active ORDER BY active",
                vec!["count", "active", "count"],
                vec![
                    json!({"000count": 2, "001active": false, "002count": 1}),
                    json!({"000count": 1, "001active": true, "002count": 1}),
                ],
            ),
            (
                "SELECT DISTINCT active, active FROM users ORDER BY active DESC",
                vec!["active", "active"],
                vec![
                    json!({"000active": true, "001active": true}),
                    json!({"000active": false, "001active": false}),
                ],
            ),
            // Outputs of one name that return the same thing can be ordered by that name.
            (
                "SELECT id AS n, id AS n FROM users ORDER BY n DESC LIMIT 1",
                vec!["n", "n"],
                vec![json!({"000n": 3, "001n": 3})],
            ),
            (
                "SELECT COUNT(*), COUNT(*) FROM users ORDER BY count",
                vec!["count", "count"],
                vec![json!({"000count": 3, "001count": 3})],
            ),
        ] {
            let result = engine.execute_sql_rows(sql, &[], true).unwrap();
            assert_eq!(names(&result), fields, "{sql}");
            assert_eq!(
                result.rows,
                rows.into_iter().map(row).collect::<Vec<_>>(),
                "{sql}"
            );
            // Rows read by name are refused them, as before.
            assert_eq!(
                engine.execute_sql(sql, &[]).unwrap_err().code,
                "INVALID_QUERY",
                "{sql}"
            );
        }

        // Distinct names key array rows as they key any others.
        assert_eq!(
            engine
                .execute_sql_rows("SELECT id, name FROM users WHERE id = 1", &[], true)
                .unwrap()
                .rows,
            vec![row(json!({"id": 1, "name": "cy"}))]
        );

        // A prepared statement and a script choose with each execution.
        let statement = engine.prepare_sql(join).unwrap();
        assert_eq!(
            engine.execute_prepared(statement, &[]).unwrap_err().code,
            "INVALID_QUERY"
        );
        let prepared = engine.execute_prepared_rows(statement, &[], true).unwrap();
        assert_eq!(names(&prepared), ["id", "id", "title"]);
        assert_eq!(prepared.rows.len(), 2);
        let script = "INSERT INTO posts VALUES (12, 3, 'third'); SELECT id, id FROM posts;";
        assert_eq!(engine.exec_sql(script).unwrap_err().code, "INVALID_QUERY");
        assert_eq!(engine.revision(), revision);
        let results = engine.exec_sql_rows(script, true).unwrap();
        assert!(results[0].fields.is_empty());
        assert_eq!(names(&results[1]), ["id", "id"]);
        assert_eq!(results[1].rows.len(), 3);
        assert_eq!(engine.revision(), revision + 1);

        // A transaction's reads are returned the same way.
        engine.begin_transaction().unwrap();
        let staged = engine
            .execute_sql_rows("SELECT name, name FROM users WHERE id = 2", &[], true)
            .unwrap();
        assert_eq!(
            staged.rows,
            vec![row(json!({"000name": "ann", "001name": "ann"}))]
        );
        engine.rollback_transaction().unwrap();

        // ORDER BY cannot choose between outputs of one name that return different things, and a
        // position is not a name. RETURNING names stay distinct.
        for (sql, code) in [
            (
                "SELECT id AS n, name AS n FROM users ORDER BY n",
                "INVALID_QUERY",
            ),
            (
                "SELECT users.id, posts.id FROM users JOIN posts ON users.id = posts.user_id \
                 ORDER BY id",
                "INVALID_QUERY",
            ),
            (
                "SELECT COUNT(*), COUNT(email) FROM users ORDER BY count",
                "INVALID_QUERY",
            ),
            (
                "SELECT COUNT(*), COUNT(*) FROM users ORDER BY \"000count\"",
                "COLUMN_NOT_FOUND",
            ),
            (
                "SELECT id, id FROM users ORDER BY \"000id\"",
                "COLUMN_NOT_FOUND",
            ),
            (
                "SELECT users.id, posts.id FROM users JOIN posts ON users.id = posts.user_id \
                 ORDER BY \"000id\"",
                "COLUMN_NOT_FOUND",
            ),
            (
                "UPDATE users SET name = 'x' WHERE id = 1 RETURNING id, id",
                "INVALID_QUERY",
            ),
        ] {
            assert_eq!(
                engine.execute_sql_rows(sql, &[], true).unwrap_err().code,
                code,
                "{sql}"
            );
        }
        assert_eq!(engine.revision(), revision + 1);
    }

    /// A table of counters for assignments that read the row they write.
    fn counters_engine() -> PagedEngine<MemoryPageDevice> {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql(
                "CREATE TABLE counters (id INTEGER PRIMARY KEY, n INTEGER NOT NULL DEFAULT 0, \
                   rate FLOAT, label TEXT, doc JSON);\
                 INSERT INTO counters VALUES (1, 10, 1.5, 'one', NULL), (2, 20, NULL, 'two', \
                   NULL);",
            )
            .unwrap();
        engine
    }

    fn counters(engine: &PagedEngine<MemoryPageDevice>) -> Vec<Row> {
        engine
            .query_sql("SELECT id, n, rate, label FROM counters ORDER BY id", &[])
            .unwrap()
            .rows
    }

    #[test]
    fn an_update_assigns_values_worked_out_from_the_row_it_updates() {
        let mut engine = counters_engine();
        // Every assignment reads the row as it was before the statement.
        let updated = engine
            .execute_sql(
                "UPDATE counters SET n = n * 2 + $1, rate = n / 4.0, label = label || '!' \
                 WHERE id = 1 RETURNING n, rate, label",
                &[json!(1)],
            )
            .unwrap();
        assert_eq!(
            updated.rows,
            vec![row(json!({"n": 21, "rate": 2.5, "label": "one!"}))]
        );
        // The table's name or alias may qualify what an assignment reads, though not what it
        // assigns to; NULL gives NULL; a value that reads nothing is worked out once.
        engine
            .execute_sql("UPDATE counters c SET n = c.n - 1, rate = c.rate * 2", &[])
            .unwrap();
        engine
            .execute_sql(
                "UPDATE counters SET label = counters.label || '?' WHERE id = 2",
                &[],
            )
            .unwrap();
        engine
            .execute_sql("UPDATE counters SET doc = 2 * 3 WHERE id = 1", &[])
            .unwrap();
        assert_eq!(
            counters(&engine),
            vec![
                row(json!({"id": 1, "n": 20, "rate": 5.0, "label": "one!"})),
                row(json!({"id": 2, "n": 19, "rate": null, "label": "two?"})),
            ]
        );
        assert_eq!(
            engine
                .query_sql("SELECT doc FROM counters WHERE id = 1", &[])
                .unwrap()
                .rows,
            vec![row(json!({"doc": 6}))]
        );

        // Two columns swap, since each assignment reads the row as it was.
        for (id, swapped) in [
            (2, json!({"id": 19, "n": 2})),
            (19, json!({"id": 2, "n": 19})),
        ] {
            assert_eq!(
                engine
                    .execute_sql(
                        "UPDATE counters SET n = id, id = n WHERE id = $1 RETURNING id, n",
                        &[json!(id)],
                    )
                    .unwrap()
                    .rows,
                vec![row(swapped)]
            );
        }

        // A key worked out from the row moves the row, and reports both keys.
        let moved = engine
            .execute_sql("UPDATE counters SET id = id + 10", &[])
            .unwrap();
        assert_eq!(moved.row_count, 2);
        assert_eq!(keys_for_result(&moved, "counters"), [1, 2, 11, 12]);

        // Prepared, the same statement reads each execution's parameters.
        let statement = engine
            .prepare_sql("UPDATE counters SET n = n + $1 WHERE id = $2 RETURNING n")
            .unwrap();
        for expected in [25, 30] {
            assert_eq!(
                engine
                    .execute_prepared(statement, &[json!(5), json!(11)])
                    .unwrap()
                    .rows,
                vec![row(json!({"n": expected}))]
            );
        }

        // Inside a transaction, each statement reads the rows staged before it.
        engine.begin_transaction().unwrap();
        for _ in 0..3 {
            engine
                .execute_sql("UPDATE counters SET n = n + 1 WHERE id = 12", &[])
                .unwrap();
        }
        engine.commit_transaction().unwrap();
        assert_eq!(
            engine
                .query_sql("SELECT n FROM counters WHERE id = 12", &[])
                .unwrap()
                .rows,
            vec![row(json!({"n": 22}))]
        );

        // Types are checked before any row is read; values as each row is written, and a
        // statement that fails on one row changes none.
        let revision = engine.revision();
        for (sql, code) in [
            (
                "UPDATE counters SET n = n * 1.5 WHERE id = 99",
                "TYPE_MISMATCH",
            ),
            (
                "UPDATE counters SET n = label || 'x' WHERE id = 99",
                "TYPE_MISMATCH",
            ),
            (
                "UPDATE counters SET label = n + 1 WHERE id = 99",
                "TYPE_MISMATCH",
            ),
            (
                "UPDATE counters SET n = doc + 1 WHERE id = 99",
                "TYPE_MISMATCH",
            ),
            (
                "UPDATE counters SET n = missing + 1 WHERE id = 99",
                "COLUMN_NOT_FOUND",
            ),
            (
                "UPDATE counters SET n = 1 / 0 WHERE id = 99",
                "DIVISION_BY_ZERO",
            ),
            ("UPDATE counters SET n = n / (id - 12)", "DIVISION_BY_ZERO"),
            ("UPDATE counters SET n = n + rate", "TYPE_MISMATCH"),
            ("UPDATE counters SET n = n + NULL", "CONSTRAINT_VIOLATION"),
            (
                "UPDATE counters SET n = n + 9007199254740991",
                "NUMERIC_OVERFLOW",
            ),
            ("UPDATE counters SET id = id - id", "CONSTRAINT_VIOLATION"),
            ("UPDATE counters SET counters.n = 1", "SQL_PARSE_ERROR"),
            ("UPDATE counters SET n = abs(n)", "UNSUPPORTED_SQL"),
        ] {
            assert_eq!(
                engine.execute_sql(sql, &[]).unwrap_err().code,
                code,
                "{sql}"
            );
        }
        assert_eq!(engine.revision(), revision);
    }

    #[test]
    fn an_upsert_assigns_values_worked_out_from_the_stored_and_proposed_rows() {
        let mut engine = counters_engine();
        // A lone upsert, and one among others, each read the stored row as it was.
        let upsert = "INSERT INTO counters (id, n, label) VALUES ($1, $2, 'new') \
                      ON CONFLICT (id) DO UPDATE SET n = counters.n + EXCLUDED.n, \
                      label = counters.label || '+' || EXCLUDED.label RETURNING id, n, label";
        assert_eq!(
            engine
                .execute_sql(upsert, &[json!(1), json!(5)])
                .unwrap()
                .rows,
            vec![row(json!({"id": 1, "n": 15, "label": "one+new"}))]
        );
        assert_eq!(
            engine
                .execute_sql(
                    "INSERT INTO counters (id, n) VALUES (2, 1), (3, 7) ON CONFLICT (id) \
                     DO UPDATE SET n = counters.n * 10 + EXCLUDED.n",
                    &[],
                )
                .unwrap()
                .row_count,
            2
        );
        let statement = engine.prepare_sql(upsert).unwrap();
        assert_eq!(
            engine
                .execute_prepared(statement, &[json!(3), json!(-2)])
                .unwrap()
                .rows,
            vec![row(json!({"id": 3, "n": 5, "label": null}))]
        );
        // An expression of the proposed row alone, and one of no row.
        engine
            .execute_sql(
                "INSERT INTO counters (id, n) VALUES (2, 4) ON CONFLICT (id) \
                 DO UPDATE SET n = EXCLUDED.n * EXCLUDED.n, rate = 1 / 4.0",
                &[],
            )
            .unwrap();
        assert_eq!(
            counters(&engine),
            vec![
                row(json!({"id": 1, "n": 15, "rate": 1.5, "label": "one+new"})),
                row(json!({"id": 2, "n": 16, "rate": 0.25, "label": "two"})),
                row(json!({"id": 3, "n": 5, "rate": null, "label": null})),
            ]
        );

        let revision = engine.revision();
        for (sql, code) in [
            (
                "INSERT INTO counters (id) VALUES (1) ON CONFLICT (id) DO UPDATE SET n = n + 1",
                "INVALID_QUERY",
            ),
            (
                "INSERT INTO counters (id) VALUES (9) ON CONFLICT (id) \
                 DO UPDATE SET n = counters.label || 'x'",
                "TYPE_MISMATCH",
            ),
            (
                "INSERT INTO counters (id) VALUES (1) ON CONFLICT (id) \
                 DO UPDATE SET n = other.n + 1",
                "COLUMN_NOT_FOUND",
            ),
            (
                "INSERT INTO counters (id) VALUES (1) ON CONFLICT (id) \
                 DO UPDATE SET id = counters.id + 1",
                "UNSUPPORTED_SQL",
            ),
            (
                "INSERT INTO counters (id, n) VALUES (1, 0) ON CONFLICT (id) \
                 DO UPDATE SET n = counters.n / EXCLUDED.n",
                "DIVISION_BY_ZERO",
            ),
        ] {
            assert_eq!(
                engine.execute_sql(sql, &[]).unwrap_err().code,
                code,
                "{sql}"
            );
        }
        assert_eq!(engine.revision(), revision);
    }

    #[test]
    fn a_where_clause_compares_expressions_of_the_row() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql(
                "CREATE TABLE scores (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER, \
                   price FLOAT, name TEXT, doc JSON);\
                 CREATE INDEX scores_a ON scores (a);\
                 INSERT INTO scores VALUES (1, 1, 5, 2.5, 'x', '1'), (2, 4, 3, 1.0, 'y', NULL), \
                   (3, 6, 6, NULL, 'z', 'z'), (4, NULL, 2, 3.0, NULL, NULL);",
            )
            .unwrap();
        let ids = |engine: &mut PagedEngine<MemoryPageDevice>, sql: &str, params: &[Value]| {
            engine
                .execute_sql(sql, params)
                .unwrap()
                .rows
                .into_iter()
                .map(|row| row["id"].as_i64().unwrap())
                .collect::<Vec<_>>()
        };
        for (sql, params, expected) in [
            (
                "SELECT id FROM scores WHERE a > b ORDER BY id",
                vec![],
                vec![2],
            ),
            (
                "SELECT id FROM scores WHERE a >= b ORDER BY id",
                vec![],
                vec![2, 3],
            ),
            (
                "SELECT id FROM scores WHERE b = a ORDER BY id",
                vec![],
                vec![3],
            ),
            ("SELECT id FROM scores WHERE a + 1 = b - 3", vec![], vec![1]),
            (
                "SELECT id FROM scores WHERE a * 2 > b ORDER BY id",
                vec![],
                vec![2, 3],
            ),
            (
                "SELECT id FROM scores WHERE price * b > $1 ORDER BY id",
                vec![json!(5)],
                vec![1, 4],
            ),
            (
                "SELECT id FROM scores WHERE (a + 1) * 2 > 10 ORDER BY id",
                vec![],
                vec![3],
            ),
            (
                "SELECT id FROM scores WHERE (a > 1) ORDER BY id",
                vec![],
                vec![2, 3],
            ),
            ("SELECT id FROM scores WHERE ((a)) - 1 = 0", vec![], vec![1]),
            (
                "SELECT id FROM scores WHERE (a > 5 AND b > 5) OR (b - a) * 2 = 8 ORDER BY id",
                vec![],
                vec![1, 3],
            ),
            (
                "SELECT id FROM scores WHERE NOT a < b ORDER BY id",
                vec![],
                vec![2, 3],
            ),
            (
                "SELECT id FROM scores WHERE name || name = 'yy'",
                vec![],
                vec![2],
            ),
            (
                "SELECT id FROM scores WHERE doc = name ORDER BY id",
                vec![],
                vec![3],
            ),
            // NULL makes a comparison unknown, which no row matches either way.
            ("SELECT id FROM scores WHERE a + NULL = 1", vec![], vec![]),
            (
                "SELECT id FROM scores WHERE NOT a + 0 = b",
                vec![],
                vec![1, 2],
            ),
            // A value on the left mirrors onto the column, and one worked out from literals and
            // parameters is a value.
            ("SELECT id FROM scores WHERE 4 < a", vec![], vec![3]),
            (
                "SELECT id FROM scores WHERE a = $1 - 2 * 1",
                vec![json!(6)],
                vec![2],
            ),
            (
                "SELECT COUNT(*) AS id FROM scores WHERE a < b",
                vec![],
                vec![1],
            ),
            (
                "SELECT COUNT(*) AS id FROM scores WHERE a > 0 AND a <> b",
                vec![],
                vec![2],
            ),
            (
                "SELECT DISTINCT b AS id FROM scores WHERE b > a ORDER BY id",
                vec![],
                vec![5],
            ),
        ] {
            assert_eq!(ids(&mut engine, sql, &params), expected, "{sql}");
        }

        // A known value compares as a plain comparison does, and so can narrow the rows read.
        let plan = |sql| format!("{:?}", crate::statement::parse(sql, &[json!(6)]).unwrap());
        assert_eq!(
            plan("SELECT id FROM scores WHERE a = $1 - 2 * 1"),
            plan("SELECT id FROM scores WHERE a = 4")
        );
        assert_eq!(
            plan("SELECT id FROM scores WHERE 4 < a"),
            plan("SELECT id FROM scores WHERE a > 4")
        );
        // A prepared statement's is known once it is bound.
        let statement = engine
            .prepare_sql("SELECT id FROM scores WHERE a = $1 - 2 ORDER BY id")
            .unwrap();
        for (a, expected) in [(3, vec![1]), (8, vec![3])] {
            assert_eq!(
                engine
                    .execute_prepared(statement, &[json!(a)])
                    .unwrap()
                    .rows
                    .into_iter()
                    .map(|row| row["id"].as_i64().unwrap())
                    .collect::<Vec<_>>(),
                expected
            );
        }

        // Writes filter the same way, and the table's name may qualify either side.
        assert_eq!(
            ids(
                &mut engine,
                "UPDATE scores SET b = b + 1 WHERE scores.a > scores.b - 1 RETURNING id",
                &[],
            ),
            [2, 3]
        );
        assert_eq!(
            ids(
                &mut engine,
                "DELETE FROM scores s WHERE s.b * 2 > s.a + 10 RETURNING id",
                &[]
            ),
            Vec::<i64>::new()
        );
        assert_eq!(
            ids(
                &mut engine,
                "DELETE FROM scores WHERE b - a = 0 RETURNING id",
                &[]
            ),
            [2]
        );

        // Types are checked before any row is read; a row that fails fails the statement.
        for (sql, code) in [
            (
                "SELECT id FROM scores WHERE name > a LIMIT 0",
                "TYPE_MISMATCH",
            ),
            (
                "SELECT id FROM scores WHERE doc < 1 LIMIT 0",
                "TYPE_MISMATCH",
            ),
            (
                "SELECT id FROM scores WHERE a + name = 1 LIMIT 0",
                "TYPE_MISMATCH",
            ),
            (
                "SELECT id FROM scores WHERE a = missing",
                "COLUMN_NOT_FOUND",
            ),
            (
                "SELECT id FROM scores WHERE a / (b - 5) > 0",
                "DIVISION_BY_ZERO",
            ),
            ("SELECT id FROM scores WHERE a + 1", "UNSUPPORTED_SQL"),
            (
                "SELECT id FROM scores WHERE a + 1 IS NULL",
                "UNSUPPORTED_SQL",
            ),
            ("SELECT id FROM scores WHERE (a + 1", "UNSUPPORTED_SQL"),
            ("UPDATE scores SET b = 0 WHERE a > name", "TYPE_MISMATCH"),
        ] {
            assert_eq!(
                engine.execute_sql(sql, &[]).unwrap_err().code,
                code,
                "{sql}"
            );
        }
    }

    #[test]
    fn a_join_compares_expressions_of_its_tables() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, limit_score INTEGER);\
                 CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER, score INTEGER);\
                 INSERT INTO users VALUES (1, 5), (2, 10);\
                 INSERT INTO posts VALUES (10, 1, 7), (11, 1, 3), (12, 2, 12), (13, 2, 4);",
            )
            .unwrap();
        let ids = |engine: &mut PagedEngine<MemoryPageDevice>, sql: &str| {
            engine
                .execute_sql(sql, &[])
                .unwrap()
                .rows
                .into_iter()
                .map(|row| row["id"].as_i64().unwrap())
                .collect::<Vec<_>>()
        };
        // Across tables, and within one, which then narrows that table as it is read.
        assert_eq!(
            ids(
                &mut engine,
                "SELECT p.id AS id FROM users u JOIN posts p ON p.user_id = u.id \
                 WHERE p.score > u.limit_score ORDER BY id",
            ),
            [10, 12]
        );
        assert_eq!(
            ids(
                &mut engine,
                "SELECT p.id AS id FROM users u JOIN posts p ON p.user_id = u.id \
                 WHERE p.score * 2 > p.id - 2 AND u.limit_score - 5 = 0 ORDER BY id",
            ),
            [10]
        );
        assert_eq!(
            ids(
                &mut engine,
                "SELECT p.id AS id FROM users u LEFT JOIN posts p ON p.user_id = u.id \
                 WHERE p.score + u.limit_score > 20",
            ),
            [12]
        );
        assert_eq!(
            engine
                .execute_sql(
                    "SELECT p.id AS id FROM users u JOIN posts p ON p.user_id = u.id \
                     WHERE p.score > u.missing",
                    &[],
                )
                .unwrap_err()
                .code,
            "COLUMN_NOT_FOUND"
        );
    }

    #[test]
    fn a_select_list_returns_values_worked_out_from_the_row() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql(
                "CREATE TABLE items (id INTEGER PRIMARY KEY, price FLOAT, qty INTEGER, \
                   name TEXT, doc JSON);\
                 INSERT INTO items VALUES (1, 2.5, 4, 'pen', NULL), (2, 1.0, 3, 'cup', NULL), \
                   (3, 9.0, NULL, 'box', NULL);",
            )
            .unwrap();
        let rows = |engine: &mut PagedEngine<MemoryPageDevice>, sql: &str, params: &[Value]| {
            engine.execute_sql(sql, params).unwrap().rows
        };
        assert_eq!(
            rows(
                &mut engine,
                "SELECT id, price * qty AS total, name || '!' AS shout FROM items WHERE id < 3",
                &[],
            ),
            vec![
                row(json!({"id": 1, "total": 10.0, "shout": "pen!"})),
                row(json!({"id": 2, "total": 3.0, "shout": "cup!"})),
            ]
        );
        // The fields carry each value's type; an expression without an alias is unnamed, and one
        // that gives only NULL is text.
        let result = engine
            .execute_sql(
                "SELECT qty + 1, price - qty AS gap, NULL AS nothing FROM items WHERE id = 1",
                &[],
            )
            .unwrap();
        assert_eq!(
            result.fields,
            vec![
                ResultField::new("?column?", ColumnType::Integer),
                ResultField::new("gap", ColumnType::Float),
                ResultField::new("nothing", ColumnType::Text),
            ]
        );
        assert_eq!(
            result.rows,
            vec![row(json!({"?column?": 5, "gap": -1.5, "nothing": null}))]
        );

        // An output worked out from the row can be ordered by, beside others, as can the column
        // a plain output returns; NULL sorts as it does for a column.
        for (sql, expected) in [
            (
                "SELECT id, price * qty AS total FROM items ORDER BY total DESC",
                vec![3, 1, 2],
            ),
            (
                "SELECT id, price * qty AS total FROM items ORDER BY total NULLS FIRST",
                vec![3, 2, 1],
            ),
            (
                "SELECT id, qty % 2 AS odd FROM items ORDER BY odd, id DESC LIMIT 2",
                vec![1, 2],
            ),
            (
                "SELECT id AS ident, -id AS reversed FROM items ORDER BY reversed OFFSET 1",
                vec![2, 1],
            ),
            (
                "SELECT id, price * 2 AS twice FROM items ORDER BY items.price",
                vec![2, 1, 3],
            ),
            // A column that is not returned is ordered by as ever, beside outputs of any kind.
            (
                "SELECT id, price * 2 AS twice FROM items ORDER BY name",
                vec![3, 2, 1],
            ),
        ] {
            let ids = rows(&mut engine, sql, &[])
                .into_iter()
                .map(|row| row.values().next().unwrap().as_i64().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(ids, expected, "{sql}");
        }

        // Parameters in a prepared projection are bound with each execution.
        let statement = engine
            .prepare_sql("SELECT price * $1 AS scaled FROM items WHERE id = $2")
            .unwrap();
        assert_eq!(
            engine
                .execute_prepared(statement, &[json!(2), json!(2)])
                .unwrap()
                .rows,
            vec![row(json!({"scaled": 2.0}))]
        );
        // Expressions without aliases share a name, which array rows hold by position.
        assert_eq!(
            engine
                .execute_sql_rows(
                    "SELECT id + 1, id * 10 FROM items WHERE id = 2 ORDER BY id",
                    &[],
                    true,
                )
                .unwrap()
                .rows,
            vec![row(json!({"000?column?": 3, "001?column?": 20}))]
        );

        for (sql, code) in [
            ("SELECT name * 2 AS x FROM items LIMIT 0", "TYPE_MISMATCH"),
            ("SELECT id + missing AS x FROM items", "COLUMN_NOT_FOUND"),
            ("SELECT id + 1, id + 2 FROM items", "INVALID_QUERY"),
            (
                "SELECT id + 1 AS x FROM items ORDER BY x, name",
                "UNSUPPORTED_SQL",
            ),
            (
                "SELECT doc AS d, id + 1 AS x FROM items ORDER BY x, d",
                "TYPE_MISMATCH",
            ),
            ("SELECT id / 0 AS x FROM items", "DIVISION_BY_ZERO"),
            ("SELECT lower(name) FROM items", "UNSUPPORTED_SQL"),
            ("SELECT DISTINCT id + 1 AS x FROM items", "UNSUPPORTED_SQL"),
            (
                "SELECT id + 1 AS x, COUNT(*) FROM items GROUP BY id",
                "UNSUPPORTED_SQL",
            ),
        ] {
            assert_eq!(
                engine.execute_sql(sql, &[]).unwrap_err().code,
                code,
                "{sql}"
            );
        }
    }

    #[test]
    fn a_join_returns_values_worked_out_from_its_tables() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);\
                 CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER, score INTEGER);\
                 INSERT INTO users VALUES (1, 'ann'), (2, 'bob');\
                 INSERT INTO posts VALUES (10, 1, 7), (11, 2, 3), (12, 1, 12);",
            )
            .unwrap();
        assert_eq!(
            engine
                .execute_sql(
                    "SELECT p.id AS id, u.name || '#' || u.name AS tag, p.score * 2 AS doubled \
                     FROM posts p JOIN users u ON u.id = p.user_id ORDER BY doubled DESC LIMIT 2",
                    &[],
                )
                .unwrap()
                .rows,
            vec![
                row(json!({"id": 12, "tag": "ann#ann", "doubled": 24})),
                row(json!({"id": 10, "tag": "ann#ann", "doubled": 14})),
            ]
        );
        let statement = engine
            .prepare_sql(
                "SELECT p.score - $1 AS rest FROM posts p JOIN users u ON u.id = p.user_id \
                 WHERE u.name = $2 ORDER BY rest",
            )
            .unwrap();
        assert_eq!(
            engine
                .execute_prepared(statement, &[json!(3), json!("ann")])
                .unwrap()
                .rows,
            vec![row(json!({"rest": 4})), row(json!({"rest": 9}))]
        );
        assert_eq!(
            engine
                .execute_sql(
                    "SELECT DISTINCT p.score + 1 AS x FROM posts p JOIN users u ON u.id = p.user_id",
                    &[],
                )
                .unwrap_err()
                .code,
            "UNSUPPORTED_SQL"
        );
        assert_eq!(
            engine
                .execute_sql(
                    "SELECT u.name + 1 AS x FROM posts p JOIN users u ON u.id = p.user_id",
                    &[],
                )
                .unwrap_err()
                .code,
            "TYPE_MISMATCH"
        );
    }

    #[test]
    fn in_takes_the_values_a_subquery_returns() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);\
                 CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER, score INTEGER);\
                 CREATE TABLE likes (post_id INTEGER, who TEXT, PRIMARY KEY (post_id, who));\
                 INSERT INTO users VALUES (1, 'ann'), (2, 'bob'), (3, 'cy');\
                 INSERT INTO posts VALUES (10, 1, 7), (11, 2, 3), (12, 1, 9), (13, NULL, 1);\
                 INSERT INTO likes VALUES (11, 'ann'), (12, 'bob');",
            )
            .unwrap();
        let ids = |engine: &mut PagedEngine<MemoryPageDevice>, sql: &str, params: &[Value]| {
            engine
                .execute_sql(sql, params)
                .unwrap()
                .rows
                .into_iter()
                .map(|row| row.values().next().unwrap().as_i64().unwrap())
                .collect::<Vec<_>>()
        };
        for (sql, params, expected) in [
            (
                "SELECT id FROM users WHERE id IN (SELECT user_id FROM posts WHERE score > 5)",
                vec![],
                vec![1],
            ),
            (
                "SELECT id FROM users WHERE users.id IN (SELECT p.user_id FROM posts p \
                 WHERE p.score < $1) ORDER BY id",
                vec![json!(8)],
                vec![1, 2],
            ),
            // A NULL among the values leaves NOT IN unknown, as in PostgreSQL.
            (
                "SELECT id FROM users WHERE id NOT IN (SELECT user_id FROM posts)",
                vec![],
                vec![],
            ),
            (
                "SELECT id FROM users WHERE id NOT IN (SELECT user_id FROM posts \
                 WHERE user_id IS NOT NULL)",
                vec![],
                vec![3],
            ),
            (
                "SELECT id FROM users WHERE id IN (SELECT user_id FROM posts WHERE score > 99)",
                vec![],
                vec![],
            ),
            // A subquery may group, join, or hold a subquery itself.
            (
                "SELECT id FROM users WHERE id IN (SELECT user_id FROM posts GROUP BY user_id) \
                 ORDER BY id",
                vec![],
                vec![1, 2],
            ),
            (
                "SELECT id FROM posts WHERE id IN (SELECT l.post_id AS id FROM likes l \
                 JOIN users u ON u.name = l.who WHERE u.id = 1)",
                vec![],
                vec![11],
            ),
            (
                "SELECT id FROM users WHERE id IN (SELECT user_id FROM posts \
                 WHERE id IN (SELECT post_id FROM likes)) ORDER BY id",
                vec![],
                vec![1, 2],
            ),
            // The statement around it may aggregate or join, and is still limited by its own.
            (
                "SELECT COUNT(*) FROM posts WHERE user_id IN (SELECT id FROM users \
                 WHERE name = 'ann')",
                vec![],
                vec![2],
            ),
            (
                "SELECT p.id AS id FROM posts p JOIN users u ON u.id = p.user_id \
                 WHERE p.id IN (SELECT post_id FROM likes) ORDER BY id",
                vec![],
                vec![11, 12],
            ),
            (
                "SELECT id FROM posts WHERE user_id IN (SELECT id FROM users) ORDER BY id \
                 LIMIT $1",
                vec![json!(1)],
                vec![10],
            ),
        ] {
            assert_eq!(ids(&mut engine, sql, &params), expected, "{sql}");
        }

        // Prepared, the subquery's parameters are bound with each execution.
        let statement = engine
            .prepare_sql(
                "SELECT id FROM users WHERE id IN (SELECT user_id FROM posts WHERE score > $1) \
                 ORDER BY id LIMIT $2",
            )
            .unwrap();
        for (score, expected) in [(8, vec![1]), (2, vec![1, 2])] {
            assert_eq!(
                engine
                    .execute_prepared(statement, &[json!(score), json!(5)])
                    .unwrap()
                    .rows
                    .into_iter()
                    .map(|row| row["id"].as_i64().unwrap())
                    .collect::<Vec<_>>(),
                expected
            );
        }

        // Writes, and a transaction's staged rows, are read the same way.
        engine.begin_transaction().unwrap();
        engine
            .execute_sql("INSERT INTO posts VALUES (14, 3, 50)", &[])
            .unwrap();
        assert_eq!(
            ids(
                &mut engine,
                "UPDATE users SET name = name || '*' WHERE id IN (SELECT user_id FROM posts \
                 WHERE score > 20) RETURNING id",
                &[],
            ),
            [3]
        );
        engine.commit_transaction().unwrap();
        assert_eq!(
            ids(
                &mut engine,
                "DELETE FROM posts WHERE user_id IN (SELECT id FROM users WHERE name = 'cy*') \
                 RETURNING id",
                &[],
            ),
            [14]
        );

        for batch in (0..1025).collect::<Vec<_>>().chunks(200) {
            let rows = batch
                .iter()
                .map(|id| format!("({}, 'x{id}')", id + 100))
                .collect::<Vec<_>>()
                .join(", ");
            engine
                .execute_sql(&format!("INSERT INTO users VALUES {rows}"), &[])
                .unwrap();
        }
        let nested = "id IN (SELECT id FROM users WHERE ".repeat(17) + "id = 1" + &")".repeat(17);
        for (sql, code) in [
            (
                "SELECT id FROM users WHERE id IN (SELECT id, name FROM users)",
                "INVALID_QUERY",
            ),
            (
                "SELECT id FROM users WHERE id IN (SELECT * FROM posts)",
                "INVALID_QUERY",
            ),
            (
                "SELECT id FROM users WHERE id IN (SELECT user_id FROM posts ORDER BY id)",
                "UNSUPPORTED_SQL",
            ),
            (
                "SELECT id FROM users WHERE id IN (SELECT user_id FROM posts LIMIT 1)",
                "UNSUPPORTED_SQL",
            ),
            (
                "SELECT id FROM users WHERE id IN (SELECT user_id FROM posts \
                 WHERE posts.user_id = users.id)",
                "COLUMN_NOT_FOUND",
            ),
            (
                "SELECT id FROM users WHERE name IN (SELECT score FROM posts)",
                "TYPE_MISMATCH",
            ),
            (
                "SELECT id FROM users WHERE id IN (SELECT id FROM users)",
                "QUERY_WORK_LIMIT_EXCEEDED",
            ),
            (
                &format!("SELECT id FROM users WHERE {nested}"),
                "INVALID_QUERY",
            ),
        ] {
            assert_eq!(
                engine.execute_sql(sql, &[]).unwrap_err().code,
                code,
                "{sql}"
            );
        }
    }

    #[test]
    fn the_schema_lists_each_table_with_its_columns_key_and_indexes() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        assert!(engine.schema().unwrap().is_empty());
        engine
            .exec_sql(
                "CREATE TABLE \"Zebra\" (id TEXT PRIMARY KEY);\
                 CREATE TABLE notes (owner INTEGER, id INTEGER, body TEXT NOT NULL DEFAULT 'x', \
                   rating FLOAT, doc JSON DEFAULT NULL, PRIMARY KEY (owner, id));\
                 CREATE UNIQUE INDEX notes_body ON notes (body, owner);\
                 CREATE INDEX a_notes_rating ON notes (id);\
                 ALTER TABLE notes ADD COLUMN done BOOLEAN NOT NULL DEFAULT false;",
            )
            .unwrap();
        let schema = engine.schema().unwrap();
        // Tables and indexes come in name order, columns as declared, and keys in key order.
        let names = schema
            .iter()
            .map(|(table, _)| table.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["Zebra", "notes"]);
        let (notes, indexes) = &schema[1];
        assert_eq!(notes.primary_key, ["owner", "id"]);
        let columns = notes
            .columns
            .iter()
            .map(|column| {
                (
                    column.name.as_str(),
                    column.data_type,
                    column.nullable,
                    column.default.clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            columns,
            [
                ("owner", ColumnType::Integer, false, None),
                ("id", ColumnType::Integer, false, None),
                ("body", ColumnType::Text, false, Some(json!("x"))),
                ("rating", ColumnType::Float, true, None),
                ("doc", ColumnType::Json, true, Some(Value::Null)),
                ("done", ColumnType::Boolean, false, Some(json!(false))),
            ]
        );
        let indexes = indexes
            .iter()
            .map(|index| (index.name.as_str(), index.columns.clone(), index.unique))
            .collect::<Vec<_>>();
        assert_eq!(
            indexes,
            [
                ("a_notes_rating", vec!["id".to_owned()], false),
                (
                    "notes_body",
                    vec!["body".to_owned(), "owner".to_owned()],
                    true
                ),
            ]
        );

        // A transaction reads the committed schema, which its statements cannot change, and a
        // dropped index or table leaves it.
        engine.begin_transaction().unwrap();
        assert_eq!(engine.schema().unwrap(), schema);
        engine.rollback_transaction().unwrap();
        engine
            .exec_sql("DROP INDEX a_notes_rating; DROP TABLE \"Zebra\";")
            .unwrap();
        let schema = engine.schema().unwrap();
        assert_eq!(schema.len(), 1);
        assert_eq!(schema[0].1.len(), 1);
        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        assert_eq!(reopened.schema().unwrap(), schema);
    }

    #[test]
    fn constraints_index_methods_and_json_casts_read_as_drizzle_kit_writes_them() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        // The statements of a migration that `drizzle-kit generate` wrote, but its foreign key.
        engine
            .exec_sql(
                r#"CREATE TABLE "post_tags" (
                    "post_id" integer NOT NULL,
                    "tag" text NOT NULL,
                    CONSTRAINT "post_tags_post_id_tag_pk" PRIMARY KEY("post_id","tag")
                );
                --> statement-breakpoint
                CREATE TABLE "users" (
                    "id" text PRIMARY KEY NOT NULL,
                    "name" text NOT NULL,
                    "email" varchar,
                    "meta" jsonb DEFAULT '{"tags":[]}'::jsonb,
                    CONSTRAINT "users_email_unique" UNIQUE("email")
                );
                --> statement-breakpoint
                CREATE INDEX "users_name_idx" ON "users" USING btree ("name");
                CREATE TABLE plain (id INTEGER PRIMARY KEY, code TEXT UNIQUE, a INTEGER,
                    b INTEGER, UNIQUE (a, b));"#,
            )
            .unwrap();
        let schema = engine.schema().unwrap();
        let names = |table: usize| {
            schema[table]
                .1
                .iter()
                .map(|index| (index.name.clone(), index.columns.clone(), index.unique))
                .collect::<Vec<_>>()
        };
        assert_eq!(schema[0].0.name, "plain");
        // An unnamed unique constraint is named for its table and columns, as PostgreSQL names it.
        assert_eq!(
            names(0),
            [
                ("plain_a_b_key".to_owned(), vec!["a".to_owned(), "b".to_owned()], true),
                ("plain_code_key".to_owned(), vec!["code".to_owned()], true),
            ]
        );
        assert_eq!(schema[1].0.primary_key, ["post_id", "tag"]);
        assert!(schema[1].1.is_empty());
        assert_eq!(
            names(2),
            [
                ("users_email_unique".to_owned(), vec!["email".to_owned()], true),
                ("users_name_idx".to_owned(), vec!["name".to_owned()], false),
            ]
        );
        assert_eq!(schema[2].0.columns[3].default, Some(json!({"tags": []})));
        engine
            .execute_sql("INSERT INTO users (id, name, email) VALUES ('a', 'Ann', 'a@x')", &[])
            .unwrap();
        assert_eq!(
            engine
                .execute_sql("INSERT INTO users (id, name, email) VALUES ('b', 'Bo', 'a@x')", &[])
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        assert_eq!(
            engine
                .execute_sql("SELECT meta FROM users", &[])
                .unwrap()
                .rows,
            [Row::from_iter([("meta".to_owned(), json!({"tags": []}))])]
        );

        // The statements of a later migration: a unique constraint added and dropped, a table
        // dropped with CASCADE after the row security it never had is disabled.
        engine
            .exec_sql(
                r#"ALTER TABLE "users" ADD CONSTRAINT "users_name_unique" UNIQUE("name");
                ALTER TABLE "users" DROP CONSTRAINT "users_email_unique";
                ALTER TABLE "post_tags" DISABLE ROW LEVEL SECURITY;
                DROP TABLE "post_tags" CASCADE;
                DROP INDEX "users_name_idx" RESTRICT;"#,
            )
            .unwrap();
        let schema = engine.schema().unwrap();
        assert_eq!(schema.len(), 2);
        assert_eq!(
            schema[1]
                .1
                .iter()
                .map(|index| index.name.as_str())
                .collect::<Vec<_>>(),
            ["users_name_unique"]
        );
        engine
            .exec_sql(r#"ALTER TABLE users DROP CONSTRAINT IF EXISTS "users_email_unique""#)
            .unwrap();
        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        assert_eq!(reopened.schema().unwrap(), schema);
    }

    #[test]
    fn altered_columns_keep_every_row_as_it_was() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        let rows = |engine: &mut PagedEngine<MemoryPageDevice>, sql: &str| {
            engine
                .execute_sql(sql, &[])
                .unwrap()
                .rows
                .into_iter()
                .map(Value::Object)
                .collect::<Vec<_>>()
        };
        engine
            .exec_sql(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER DEFAULT 5, c TEXT DEFAULT 'x');\
                 INSERT INTO t (id, a) VALUES (1, 'a1');\
                 INSERT INTO t VALUES (2, 'a2', 7, 'y'), (3, NULL, 5, 'z');\
                 ALTER TABLE t ADD COLUMN d INTEGER NOT NULL DEFAULT 9;\
                 CREATE INDEX t_b ON t (b);\
                 CREATE UNIQUE INDEX t_bc ON t (b, c);",
            )
            .unwrap();

        // Rows that left a column out as its default keep the default they had.
        engine
            .exec_sql(
                "ALTER TABLE t ALTER COLUMN d SET DEFAULT 10;\
                 ALTER TABLE t ALTER COLUMN c DROP DEFAULT;\
                 INSERT INTO t (id, a) VALUES (4, 'a4');",
            )
            .unwrap();
        assert_eq!(
            rows(&mut engine, "SELECT id, b, c, d FROM t ORDER BY id"),
            [
                json!({"id": 1, "b": 5, "c": "x", "d": 9}),
                json!({"id": 2, "b": 7, "c": "y", "d": 9}),
                json!({"id": 3, "b": 5, "c": "z", "d": 9}),
                json!({"id": 4, "b": 5, "c": null, "d": 10}),
            ]
        );

        // NOT NULL is refused while a row holds NULL, and then holds.
        assert_eq!(
            engine
                .execute_sql("ALTER TABLE t ALTER COLUMN a SET NOT NULL", &[])
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        engine
            .exec_sql(
                "UPDATE t SET a = 'a3' WHERE id = 3;\
                 ALTER TABLE t ALTER COLUMN a SET NOT NULL;",
            )
            .unwrap();
        assert_eq!(
            engine
                .execute_sql("INSERT INTO t (id) VALUES (5)", &[])
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        engine
            .exec_sql(
                "ALTER TABLE t ALTER COLUMN a DROP NOT NULL;\
                 INSERT INTO t (id) VALUES (5);",
            )
            .unwrap();

        // A dropped column takes its indexes with it, and the others still find rows.
        engine
            .exec_sql(
                r#"ALTER TABLE "t" DROP COLUMN "c";
                ALTER TABLE t DROP COLUMN IF EXISTS c;
                ALTER TABLE t RENAME COLUMN b TO beta;
                ALTER TABLE t RENAME id TO ident;
                ALTER TABLE t ALTER COLUMN beta SET DATA TYPE bigint;
                ALTER TABLE t ALTER beta TYPE int4;
                ALTER TABLE t RENAME TO tee;"#,
            )
            .unwrap();
        let schema = engine.schema().unwrap();
        assert_eq!(schema.len(), 1);
        let (tee, indexes) = &schema[0];
        assert_eq!(tee.name, "tee");
        assert_eq!(tee.primary_key, ["ident"]);
        assert_eq!(
            tee.columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["ident", "a", "beta", "d"]
        );
        assert_eq!(indexes.len(), 1);
        assert_eq!(indexes[0].name, "t_b");
        assert_eq!(indexes[0].table, "tee");
        assert_eq!(indexes[0].columns, ["beta"]);
        assert_eq!(
            rows(&mut engine, "SELECT ident, a FROM tee WHERE beta = 7"),
            [json!({"ident": 2, "a": "a2"})]
        );
        assert_eq!(
            rows(&mut engine, "SELECT * FROM tee WHERE ident = 5"),
            [json!({"ident": 5, "a": null, "beta": 5, "d": 10})]
        );
        assert_eq!(
            engine.execute_sql("SELECT * FROM t", &[]).unwrap_err().code,
            "TABLE_NOT_FOUND"
        );
        engine.check().unwrap();
        let mut engine = PagedEngine::open(engine.into_device()).unwrap();
        assert_eq!(engine.schema().unwrap(), schema);
        assert_eq!(rows(&mut engine, "SELECT ident FROM tee ORDER BY ident").len(), 5);
        engine.check().unwrap();
    }

    #[test]
    fn a_varchar_holds_at_most_its_length_in_characters() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql(
                "CREATE TABLE v (id INTEGER PRIMARY KEY, code VARCHAR(3), \
                   name CHARACTER VARYING(5) DEFAULT 'ab', note VARCHAR);\
                 INSERT INTO v (id, code, name) VALUES (1, 'abc', 'naïve'), (2, 'ab ', NULL);",
            )
            .unwrap();
        let lengths = |engine: &PagedEngine<MemoryPageDevice>| {
            engine.schema().unwrap()[0]
                .0
                .columns
                .iter()
                .map(|column| column.max_length)
                .collect::<Vec<_>>()
        };
        assert_eq!(lengths(&engine), [None, Some(3), Some(5), None]);
        for sql in [
            "INSERT INTO v (id, code) VALUES (3, 'abcd')",
            // PostgreSQL would trim the excess spaces; TinyJoin refuses them.
            "INSERT INTO v (id, code) VALUES (3, 'abc ')",
            "UPDATE v SET code = code || 'x' WHERE id = 1",
            "UPDATE v SET name = 'too long'",
            "INSERT INTO v (id, code) VALUES (1, 'abc') ON CONFLICT (id) DO UPDATE SET code = 'abcd'",
            "INSERT INTO v (id, code) VALUES (1, 'abcd') ON CONFLICT (id) DO UPDATE SET code = EXCLUDED.code",
            "ALTER TABLE v ALTER COLUMN code TYPE varchar(2)",
        ] {
            let error = engine.execute_sql(sql, &[]).unwrap_err();
            assert_eq!(error.code, "CONSTRAINT_VIOLATION", "{sql}: {}", error.message);
        }
        for (sql, code) in [
            (
                "CREATE TABLE w (id INTEGER PRIMARY KEY, c VARCHAR(2) DEFAULT 'abc')",
                "INVALID_SCHEMA",
            ),
            ("CREATE TABLE w (id INTEGER PRIMARY KEY, c VARCHAR(0))", "INVALID_SCHEMA"),
            (
                "CREATE TABLE w (id INTEGER PRIMARY KEY, c VARCHAR(10485761))",
                "INVALID_SCHEMA",
            ),
            ("CREATE TABLE w (id INTEGER PRIMARY KEY, c INTEGER(5))", "UNSUPPORTED_SQL"),
            ("CREATE TABLE w (id INTEGER PRIMARY KEY, c VARCHAR(n))", "UNSUPPORTED_SQL"),
            ("ALTER TABLE v ALTER COLUMN id TYPE varchar(5)", "UNSUPPORTED_SQL"),
        ] {
            let error = engine.execute_sql(sql, &[]).unwrap_err();
            assert_eq!(error.code, code, "{sql}: {}", error.message);
        }

        // A longer limit, or none, changes only the catalog; a shorter one checks every row.
        engine
            .exec_sql(
                "ALTER TABLE v ALTER COLUMN code TYPE varchar(10);\
                 INSERT INTO v (id, code) VALUES (5, 'abcdefghij');",
            )
            .unwrap();
        assert_eq!(
            engine
                .execute_sql("ALTER TABLE v ALTER COLUMN code SET DATA TYPE varchar(3)", &[])
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        engine
            .exec_sql(
                "DELETE FROM v WHERE id = 5;\
                 ALTER TABLE v ALTER COLUMN code TYPE varchar(3);\
                 ALTER TABLE v ALTER COLUMN note TYPE varchar(4);\
                 ALTER TABLE v ALTER COLUMN name TYPE text;",
            )
            .unwrap();
        assert_eq!(lengths(&engine), [None, Some(3), None, Some(4)]);
        engine.check().unwrap();
        let engine = PagedEngine::open(engine.into_device()).unwrap();
        assert_eq!(lengths(&engine), [None, Some(3), None, Some(4)]);
    }

    #[test]
    fn a_table_too_large_for_one_chunk_is_rebuilt_whole() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .execute_sql(
                "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT, extra TEXT, n INTEGER)",
                &[],
            )
            .unwrap();
        for start in (0..600).step_by(100) {
            let values = (start..start + 100)
                .map(|id| format!("({id}, 'body {id} {}', 'extra', {id})", "x".repeat(100)))
                .collect::<Vec<_>>()
                .join(", ");
            engine
                .execute_sql(&format!("INSERT INTO notes VALUES {values}"), &[])
                .unwrap();
        }
        engine
            .exec_sql("CREATE INDEX notes_n ON notes (n); ALTER TABLE notes DROP COLUMN extra;")
            .unwrap();
        let result = engine
            .execute_sql("SELECT count(*) AS rows, sum(n) AS total FROM notes", &[])
            .unwrap();
        assert_eq!(
            Value::Object(result.rows[0].clone()),
            json!({"rows": 600, "total": 179_700})
        );
        assert_eq!(
            Value::Object(
                engine
                    .execute_sql("SELECT * FROM notes WHERE n = 599", &[])
                    .unwrap()
                    .rows[0]
                    .clone()
            ),
            json!({"id": 599, "body": format!("body 599 {}", "x".repeat(100)), "n": 599})
        );
        engine.check().unwrap();
    }

    #[test]
    fn alterations_tinyjoin_cannot_make_are_refused() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER);\
                 CREATE TABLE app.items (id INTEGER PRIMARY KEY);\
                 CREATE TABLE u (id INTEGER PRIMARY KEY);",
            )
            .unwrap();
        for (sql, code) in [
            ("ALTER TABLE t DROP COLUMN id", "UNSUPPORTED_SQL"),
            ("ALTER TABLE t DROP COLUMN missing", "COLUMN_NOT_FOUND"),
            ("ALTER TABLE t RENAME COLUMN a TO b", "COLUMN_ALREADY_EXISTS"),
            ("ALTER TABLE t RENAME COLUMN missing TO c", "COLUMN_NOT_FOUND"),
            ("ALTER TABLE t RENAME TO u", "TABLE_ALREADY_EXISTS"),
            ("ALTER TABLE t ALTER COLUMN b SET DEFAULT 'x'", "INVALID_SCHEMA"),
            ("ALTER TABLE t ALTER COLUMN id DROP NOT NULL", "INVALID_SCHEMA"),
            ("ALTER TABLE t ALTER COLUMN b TYPE text", "UNSUPPORTED_SQL"),
            ("ALTER TABLE t ALTER COLUMN b TYPE text USING b::text", "UNSUPPORTED_SQL"),
            ("ALTER TABLE t ALTER COLUMN b SET STATISTICS 100", "SQL_PARSE_ERROR"),
            ("ALTER TABLE t OWNER TO someone", "UNSUPPORTED_SQL"),
        ] {
            let error = engine.exec_sql(sql).unwrap_err();
            assert_eq!(error.code, code, "{sql}: {}", error.message);
        }
        // A table named with a schema keeps its schema when it is renamed.
        engine
            .exec_sql("ALTER TABLE app.items RENAME TO things")
            .unwrap();
        assert!(engine.execute_sql("SELECT * FROM app.things", &[]).is_ok());

        engine.begin_transaction().unwrap();
        assert_eq!(
            engine
                .exec_sql("ALTER TABLE t RENAME TO v")
                .unwrap_err()
                .code,
            "UNSUPPORTED_SQL"
        );
        engine.rollback_transaction().unwrap();
    }

    #[test]
    fn constraints_and_index_methods_tinyjoin_cannot_enforce_are_refused() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql(
                "CREATE TABLE users (id TEXT PRIMARY KEY, name TEXT);\
                 CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id TEXT);",
            )
            .unwrap();
        for (sql, code, message) in [
            (
                "CREATE TABLE a (id INTEGER PRIMARY KEY, user_id TEXT REFERENCES users (id))",
                "UNSUPPORTED_SQL",
                "Foreign keys are not supported",
            ),
            (
                "CREATE TABLE a (id INTEGER PRIMARY KEY, user_id TEXT, \
                 FOREIGN KEY (user_id) REFERENCES users (id))",
                "UNSUPPORTED_SQL",
                "Foreign keys are not supported",
            ),
            (
                r#"ALTER TABLE "posts" ADD CONSTRAINT "posts_user_id_users_id_fk" FOREIGN KEY ("user_id") REFERENCES "public"."users"("id") ON DELETE no action ON UPDATE no action"#,
                "UNSUPPORTED_SQL",
                "Foreign keys are not supported",
            ),
            (
                "CREATE TABLE a (id INTEGER PRIMARY KEY CHECK (id > 0))",
                "UNSUPPORTED_SQL",
                "CHECK constraints",
            ),
            (
                "CREATE TABLE a (id INTEGER PRIMARY KEY, CONSTRAINT positive CHECK (id > 0))",
                "UNSUPPORTED_SQL",
                "CHECK constraints",
            ),
            (
                "CREATE INDEX posts_user ON posts USING hash (user_id)",
                "UNSUPPORTED_SQL",
                "Index method `hash`",
            ),
            (
                "CREATE TABLE a (id INTEGER PRIMARY KEY, b TEXT DEFAULT 'x'::text)",
                "UNSUPPORTED_SQL",
                "cast only a string to JSON",
            ),
            (
                "CREATE TABLE a (id INTEGER PRIMARY KEY, b JSONB DEFAULT '{'::jsonb)",
                "INVALID_SCHEMA",
                "JSON text",
            ),
            (
                "CREATE TABLE a (id INTEGER PRIMARY KEY, b FLOAT UNIQUE)",
                "UNSUPPORTED_SQL",
                "Index `a_b_key` cannot use float column `b`",
            ),
            (
                "ALTER TABLE posts ADD COLUMN code TEXT UNIQUE",
                "UNSUPPORTED_SQL",
                "cannot add a primary key or UNIQUE constraint",
            ),
            (
                "ALTER TABLE posts ADD CONSTRAINT posts_pk PRIMARY KEY (user_id)",
                "UNSUPPORTED_SQL",
                "primary key",
            ),
            (
                "ALTER TABLE posts DROP CONSTRAINT posts_missing",
                "INDEX_NOT_FOUND",
                "no unique constraint `posts_missing`",
            ),
            (
                "ALTER TABLE missing DISABLE ROW LEVEL SECURITY",
                "TABLE_NOT_FOUND",
                "",
            ),
        ] {
            let error = engine.exec_sql(sql).unwrap_err();
            assert_eq!(error.code, code, "{sql}: {}", error.message);
            assert!(error.message.contains(message), "{sql}: {}", error.message);
        }
        // A table whose unique constraint names an index that exists is not created at all.
        engine.exec_sql("CREATE INDEX taken ON posts (user_id)").unwrap();
        assert_eq!(
            engine
                .exec_sql("CREATE TABLE a (id INTEGER PRIMARY KEY, CONSTRAINT taken UNIQUE (id))")
                .unwrap_err()
                .code,
            "INDEX_ALREADY_EXISTS"
        );
        assert_eq!(
            engine
                .exec_sql("ALTER TABLE posts DROP CONSTRAINT taken")
                .unwrap_err()
                .code,
            "INDEX_NOT_FOUND"
        );
        assert_eq!(engine.schema().unwrap().len(), 2);
        // Words that name constraints are still column names where a column is expected.
        engine
            .exec_sql("CREATE TABLE words (unique INTEGER PRIMARY KEY, constraint TEXT, check TEXT)")
            .unwrap();
    }

    #[test]
    fn a_quoted_name_holding_a_dot_is_one_column_not_a_qualified_one() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql(
                "CREATE TABLE extra (id INTEGER PRIMARY KEY, value TEXT, \"extra.value\" TEXT);\
                 INSERT INTO extra VALUES (1, 'plain', 'dotted');",
            )
            .unwrap();
        let rows = engine
            .execute_sql(
                "SELECT extra.value, \"extra.value\" FROM extra \
                 WHERE extra.value = 'plain' AND \"extra.value\" = 'dotted' \
                 ORDER BY \"extra.value\", extra.\"extra.value\"",
                &[],
            )
            .unwrap()
            .rows;
        assert_eq!(
            rows,
            vec![row(json!({"value": "plain", "extra.value": "dotted"}))]
        );
    }
}
