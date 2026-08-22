use std::cell::Cell;

use serde_json::Value;

use crate::{
    ApplyOutcome, EngineError, ExecuteResult, PageDevice, PagedStorage, PreparedStatementId,
    QueryResult, Result, StorageReader,
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
        let statement = crate::statement::parse(sql, params)?;
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
        let statement = self.prepared_statements.bind(id, params)?;
        self.execute_parsed_statement(statement)
    }

    /// Releases a retained statement. Closing an already closed issued ID is idempotent.
    pub fn close_prepared(&mut self, id: PreparedStatementId) -> Result<()> {
        self.prepared_statements.close(id)
    }

    fn execute_parsed_statement(&mut self, statement: Statement) -> Result<ExecuteResult> {
        if self.transaction.is_none() {
            return self
                .storage
                .execute_script(vec![statement])?
                .pop()
                .ok_or_else(|| {
                    EngineError::new("INTERNAL_ERROR", "SQL statement produced no result")
                });
        }
        self.execute_statement(statement, None)
    }

    /// Executes a semicolon-delimited SQL script as one atomic operation.
    ///
    /// Standalone scripts publish all DDL and DML in one pager generation. Within an explicit
    /// transaction, scripts are limited to reads and row DML and install their cloned overlay only
    /// after every statement succeeds.
    pub fn exec_sql(&mut self, sql: &str) -> Result<Vec<ExecuteResult>> {
        let script = crate::sql_script::split(sql)?;
        if script.is_empty() {
            return Err(EngineError::invalid_query(
                "exec SQL must contain at least one statement",
            ));
        }
        let statements = script
            .into_iter()
            .map(|sql| crate::statement::parse(sql, &[]))
            .collect::<Result<Vec<_>>>()?;
        if self.transaction.is_none() {
            return self.storage.execute_script(statements);
        }
        if statements.iter().any(|statement| {
            matches!(
                statement,
                Statement::Write(
                    WriteStatement::CreateTable { .. }
                        | WriteStatement::CreateIndex { .. }
                        | WriteStatement::DropTable { .. }
                        | WriteStatement::DropIndex { .. }
                        | WriteStatement::AddColumn { .. }
                )
            )
        }) {
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
            });
        }
        let changes = transaction.changes();
        let touched_tables = transaction.touched_tables();
        let outcome = self
            .storage
            .commit_transaction_changes(&changes, touched_tables)?;
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
        let PlannedDml { outcome, changes } = {
            let view = self.read_view_with_work(work);
            crate::statement::plan_dml(&view, statement)?
        };
        let fields =
            crate::statement::write_result_fields(&self.read_view_with_work(work), statement)?;
        if outcome.mutated {
            self.transaction
                .as_mut()
                .expect("the transaction branch was selected above")
                .stage(&self.storage, changes)?;
        }
        Ok(ExecuteResult {
            command: outcome.command.to_owned(),
            revision: self.storage.revision(),
            row_count: outcome.row_count,
            fields,
            rows: outcome.rows,
            tables: outcome.tables,
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
    })
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use serde_json::{Value, json};

    use super::*;
    use crate::{Engine, InMemoryStorage, MemoryPageDevice, PAGE_SIZE, PageId, Row};

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
    fn failed_and_no_match_dml_do_not_publish() {
        let mut engine = page_native_fixture(MemoryPageDevice::new(0).unwrap()).unwrap();
        let revision = engine.revision();
        let rows = engine
            .query_sql("SELECT * FROM accounts ORDER BY id", &[])
            .unwrap();

        for sql in [
            "UPDATE accounts SET id = 1 WHERE id = 2",
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
                tables: vec![]
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
        control.arm_after_flush(1);
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
        control.arm_after_flush(3);
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
        control.arm_after_flush(3);
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
            vec![row(json!({"id": 2, "label": "prior"}))]
        );
        let committed = engine.commit_transaction().unwrap();
        assert_eq!(committed.revision, revision + 1);
        assert_eq!(committed.tables, vec!["aliases"]);

        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        assert_eq!(
            reopened
                .query_sql("SELECT id, label FROM aliases ORDER BY id", &[])
                .unwrap()
                .rows,
            vec![row(json!({"id": 2, "label": "prior"}))]
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
}
