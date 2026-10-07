use std::cell::Cell;
use std::rc::Rc;

use serde_json::Value;

use crate::{
    ApplyOutcome, ChangedKeys, EngineError, ExecuteResult, IndexDefinition, PageDevice,
    PagedStorage, PreparedStatementId, QueryResult, Result, ResultField, SchemaDefinition,
    StorageReader, TableDefinition,
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
    value_rows: bool,
    /// How the last prepared statement to run was planned, or `None` if it failed before that
    /// was settled.
    #[cfg(test)]
    last_path: Cell<Option<StatementPath>>,
}

/// How a prepared statement was planned, which the engine records in test builds only, for the
/// test that holds each point statement to the path it is meant to take.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StatementPath {
    /// From its point template, on a table no foreign key involves.
    PointPlain,
    /// From its point template, on a table a foreign key involves, whose rows are then checked.
    PointEnforced,
    /// Bound and planned in full: the statement has no point template, ran outside a
    /// transaction, or was declined by its template's planner.
    General,
}

impl<D: PageDevice> PagedEngine<D> {
    /// Opens a previously published paged database.
    pub fn open(device: D) -> Result<Self> {
        PagedStorage::open(device).map(|storage| Self {
            storage,
            transaction: None,
            prepared_statements: PreparedStatementRegistry::default(),
            value_rows: false,
            #[cfg(test)]
            last_path: Cell::new(None),
        })
    }

    /// Has statements return a single-table query's rows that stream in key order as
    /// [`crate::ValueRows`], in [`ExecuteResult::values`], leaving [`ExecuteResult::rows`] empty:
    /// quicker for a caller that writes each row's values out in field order, as the
    /// WebAssembly bridge does.
    pub fn set_value_rows(&mut self, enabled: bool) {
        self.value_rows = enabled;
    }

    /// The fingerprint of every row in this database.
    #[cfg(test)]
    pub(crate) fn database_hash(&self) -> u64 {
        self.storage.database_hash()
    }

    #[cfg(test)]
    pub(crate) fn query_sql(&self, sql: &str, params: &[Value]) -> Result<QueryResult> {
        crate::statement::run_query(&self.read_view(), crate::statement::parse(sql, params)?)
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
        statement.set_value_rows(self.value_rows);
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
        #[cfg(test)]
        self.last_path.set(None);
        if self.transaction.is_some()
            && let Some(result) = self.execute_point(id, params)?
        {
            return Ok(result);
        }
        #[cfg(test)]
        self.last_path.set(Some(StatementPath::General));
        let mut statement = self.prepared_statements.bind(id, params)?;
        if array_rows {
            statement.position_outputs()?;
        }
        statement.set_value_rows(self.value_rows);
        self.execute_parsed_statement(statement)
    }

    /// Releases a retained statement. Closing an already closed issued ID is idempotent.
    pub fn close_prepared(&mut self, id: PreparedStatementId) -> Result<()> {
        self.prepared_statements.close(id)
    }

    /// Executes a prepared statement of a point template's shape inside a transaction, from the
    /// template and `params` directly, or returns `None`, having changed nothing, when the
    /// statement must be bound and planned in full.
    fn execute_point(
        &mut self,
        id: PreparedStatementId,
        params: &[Value],
    ) -> Result<Option<ExecuteResult>> {
        let Some(template) = self.prepared_statements.point(id, params)? else {
            return Ok(None);
        };
        self.storage.ensure_readiness()?;
        let (planned, keys) = {
            let view = self.read_view();
            let Some(mut planned) = crate::statement::plan_point(&view, template, params)? else {
                return Ok(None);
            };
            #[cfg(test)]
            self.last_path.set(Some(
                if crate::foreign_key::involves(&view, point_table(template)) {
                    StatementPath::PointEnforced
                } else {
                    StatementPath::PointPlain
                },
            ));
            // Foreign keys are checked as the statement ends, with the rows their actions change.
            crate::foreign_key::enforce(&view, &mut planned)?;
            let keys = crate::statement::changed_keys(&view, &planned.changes, &planned.previous)?;
            (planned, keys)
        };
        self.finish_transaction_write(planned, keys, Vec::new())
            .map(Some)
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
        let mut statements = Vec::with_capacity(script.len());
        for sql in script {
            statements.push(crate::statement::parse(sql, &[])?);
        }
        for statement in &mut statements {
            if array_rows {
                statement.position_outputs()?;
            }
            statement.set_value_rows(self.value_rows);
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
            Statement::Write(statement) => self.execute_transaction_write(&statement, work),
            statement => execute_query_result(crate::statement::run_query(
                &self.read_view_with_work(work),
                statement,
            )?),
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

    /// The version an application last gave the schema, or zero.
    pub fn schema_version(&self) -> u64 {
        self.storage.schema_version()
    }

    /// Makes the database's tables, columns, keys, and indexes match `target`, as one change that
    /// publishes whole or not at all, and reports the tables it changed. A schema the database
    /// already has changes nothing and advances no revision.
    ///
    /// Tables and columns are renamed from the names `target` says they had, while they still
    /// have them; a table or column `target` leaves out is dropped only if `drop` is set. A
    /// change no DDL statement could make, such as to a primary key or a column's runtime type,
    /// is refused, as is a `target` older than the database's schema version.
    pub fn set_schema(&mut self, target: &SchemaDefinition, drop: bool) -> Result<ApplyOutcome> {
        self.storage.ensure_readiness()?;
        if self.in_transaction() {
            return Err(EngineError::transaction_active());
        }
        let statements = crate::schema::schema_statements(
            &self.storage.schema()?,
            self.storage.schema_version(),
            target,
            drop,
        )?;
        let mut tables = Vec::<String>::new();
        for result in self
            .storage
            .execute_script(statements.into_iter().map(Statement::Write).collect())?
        {
            for table in result.tables {
                if !tables.contains(&table) {
                    tables.push(table);
                }
            }
        }
        Ok(ApplyOutcome {
            revision: self.storage.revision(),
            tables,
            keys: ChangedKeys::default(),
        })
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

    /// What the active transaction holds, as [`PagedTransaction::fingerprint`] writes it.
    #[cfg(test)]
    pub(crate) fn transaction_fingerprint(&self) -> Option<String> {
        self.transaction.as_ref().map(PagedTransaction::fingerprint)
    }

    /// What a script's candidate plans for the row-changing statement `sql`, without applying it.
    #[cfg(test)]
    pub(crate) fn plan_script_dml(&self, sql: &str) -> Result<PlannedDml> {
        match crate::statement::parse(sql, &[])? {
            Statement::Write(statement) => self.storage.plan_script_dml(&statement),
            _ => Err(EngineError::invalid_query("A read changes no rows")),
        }
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
        let (planned, keys) = {
            let view = self.read_view_with_work(work);
            let planned = crate::statement::plan_dml(&view, statement)?;
            let keys = crate::statement::changed_keys(&view, &planned.changes, &planned.previous)?;
            (planned, keys)
        };
        let fields =
            crate::statement::write_result_fields(&self.read_view_with_work(work), statement)?;
        self.finish_transaction_write(planned, keys, fields)
    }

    /// Stages what a transaction's statement planned, and reports the statement's result.
    fn finish_transaction_write(
        &mut self,
        planned: PlannedDml,
        keys: ChangedKeys,
        fields: Vec<ResultField>,
    ) -> Result<ExecuteResult> {
        let PlannedDml {
            outcome,
            changes,
            previous,
            ..
        } = planned;
        if outcome.mutated {
            self.transaction
                .as_mut()
                .expect("the transaction branch was selected above")
                .stage(&self.storage, changes, previous)?;
        }
        Ok(ExecuteResult {
            command: outcome.command,
            revision: self.storage.revision(),
            row_count: outcome.row_count,
            fields,
            rows: outcome.rows,
            values: None,
            tables: outcome.tables,
            // Staged work publishes at commit, so these are reported for symmetry with `tables`
            // and ignored by subscribers until the transaction commits.
            keys,
        })
    }
}

/// The table a point template's statement names.
#[cfg(test)]
fn point_table(template: &crate::statement::PointTemplate) -> &str {
    use crate::statement::PointTemplate;
    match template {
        PointTemplate::Insert { table, .. }
        | PointTemplate::Update { table, .. }
        | PointTemplate::Delete { table, .. } => table,
    }
}

fn execute_query_result(result: QueryResult) -> Result<ExecuteResult> {
    Ok(ExecuteResult {
        command: "SELECT",
        revision: result.revision,
        row_count: result.row_count(),
        fields: result.fields,
        rows: result.rows,
        values: result.values,
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

    /// A result's rows as objects, built from its value rows where it has them.
    fn objects(result: &ExecuteResult) -> Vec<Row> {
        let Some(values) = &result.values else {
            return result.rows.clone();
        };
        assert!(result.rows.is_empty());
        values
            .rows
            .iter()
            .map(|row| {
                assert_eq!(row.len(), result.fields.len());
                result
                    .fields
                    .iter()
                    .map(|field| field.name.clone())
                    .zip(row.iter().cloned())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn value_rows_hold_what_object_rows_hold() {
        let setup = [
            "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, f FLOAT, s TEXT, b BOOLEAN, j JSON)",
            "CREATE INDEX t_n ON t (n)",
            r#"INSERT INTO t (id, n, f, s, b, j) VALUES (1, 10, 1.5, 'one', true, '{"a":[1,2]}'),
                (2, NULL, -0.0, 'tw"o', false, 'null'), (3, 30, NULL, NULL, NULL, '["x"]'),
                (4, 10, 2.25, 'four
lines', true, '7')"#,
            "ALTER TABLE t ADD COLUMN d TEXT DEFAULT 'later'",
            "INSERT INTO t (id, n, s) VALUES (5, 50, 'five')",
            "CREATE TABLE k (a TEXT, b INTEGER, v TEXT, PRIMARY KEY (a, b))",
            "INSERT INTO k (a, b, v) VALUES ('x', 2, 'x2'), ('x', 1, 'x1'), ('y', 1, 'y1')",
        ];
        // Each query, and whether its rows stream in key order, which only then are value rows,
        // where that does not depend on how the rows are stored.
        let queries = [
            ("SELECT * FROM t", Some(true)),
            ("SELECT * FROM t WHERE id = 2", Some(true)),
            ("SELECT * FROM t WHERE id = 99", Some(true)),
            ("SELECT * FROM t ORDER BY id DESC", None),
            ("SELECT * FROM t WHERE n = 10", Some(true)),
            ("SELECT * FROM t WHERE n >= 10 AND n < 40 ORDER BY id", None),
            ("SELECT id, s FROM t WHERE s IS NOT NULL", Some(true)),
            ("SELECT s AS label, id FROM t LIMIT 2 OFFSET 1", Some(true)),
            (
                "SELECT id, n * 2 + 1 AS twice, s || '!' AS loud FROM t",
                Some(true),
            ),
            ("SELECT t.id, t.d FROM t", Some(true)),
            ("SELECT *, id AS again FROM t WHERE id > 3", Some(true)),
            ("SELECT * FROM t LIMIT 0", Some(true)),
            ("SELECT * FROM t ORDER BY s, id", Some(false)),
            ("SELECT * FROM k", Some(true)),
            ("SELECT * FROM k WHERE a = 'x'", Some(true)),
            ("SELECT v FROM k WHERE a = 'x' AND b = 2", Some(true)),
            ("SELECT count(*) AS n FROM t", Some(false)),
        ];
        for in_transaction in [false, true] {
            let mut objects_engine = database_with(&setup);
            let mut values_engine = database_with(&setup);
            values_engine.set_value_rows(true);
            if in_transaction {
                for engine in [&mut objects_engine, &mut values_engine] {
                    engine.begin_transaction().unwrap();
                    for sql in [
                        "UPDATE t SET s = 'staged' WHERE id = 1",
                        "INSERT INTO t (id, n) VALUES (6, 60)",
                        "DELETE FROM t WHERE id = 3",
                    ] {
                        engine.execute_sql(sql, &[]).unwrap();
                    }
                }
            }
            for (sql, streams) in queries {
                for array_rows in [false, true] {
                    let expected = objects_engine
                        .execute_sql_rows(sql, &[], array_rows)
                        .unwrap();
                    let actual = values_engine
                        .execute_sql_rows(sql, &[], array_rows)
                        .unwrap();
                    assert!(expected.values.is_none(), "{sql}");
                    if let Some(streams) = streams {
                        assert_eq!(actual.values.is_some(), streams, "{sql}");
                    }
                    assert_eq!(actual.fields, expected.fields, "{sql}");
                    assert_eq!(actual.row_count, expected.row_count, "{sql}");
                    assert_eq!(objects(&actual), expected.rows, "{sql}");
                }
            }
            // Prepared statements and scripts return value rows too.
            let point = values_engine
                .prepare_sql("SELECT * FROM t WHERE id = $1")
                .unwrap();
            let prepared = values_engine
                .execute_prepared_rows(point, &[json!(4)], false)
                .unwrap();
            assert_eq!(
                objects(&prepared),
                objects_engine
                    .execute_sql("SELECT * FROM t WHERE id = 4", &[])
                    .unwrap()
                    .rows
            );
            let script = values_engine
                .exec_sql_rows("SELECT id FROM t WHERE id < 3; SELECT * FROM k", false)
                .unwrap();
            assert!(script.iter().all(|result| result.values.is_some()));
            assert_eq!(objects(&script[1]).len(), 3);
        }
    }

    /// What the equivalence test expects of one execution in its prepared engine: that it is
    /// planned from the statement's point template and changes a row, or is planned from it and
    /// finds no row to change; that it is bound and planned in full; or that it is refused with
    /// an error of this code. `Unprepared` is a statement both engines run as SQL, which has
    /// only to succeed.
    #[derive(Clone, Copy, Debug)]
    enum Expected {
        Point,
        Unchanged,
        Full,
        Fails(&'static str),
        Unprepared,
    }
    use Expected::{Fails, Full, Point, Unchanged, Unprepared};

    /// One execution in the equivalence test: the table its statement names, the statement, its
    /// parameters, and how it should end.
    struct PointCase {
        table: &'static str,
        sql: String,
        params: Vec<Value>,
        expected: Expected,
    }

    /// A case on `table`, with its parameters as a JSON array.
    fn case_on(table: &'static str, sql: &str, params: Value, expected: Expected) -> PointCase {
        let Value::Array(params) = params else {
            unreachable!("a case's parameters are written as an array")
        };
        PointCase {
            table,
            sql: sql.to_owned(),
            params,
            expected,
        }
    }

    fn on_items(sql: &str, params: Value, expected: Expected) -> PointCase {
        case_on("items", sql, params, expected)
    }

    /// The catalog a round of the equivalence test runs against, where it decides what a
    /// statement does or how it is planned.
    #[derive(Clone, Copy, Debug)]
    struct PointRound {
        number: usize,
        /// Whether the setup gave its tables indexes, of which `items_name` makes each name
        /// unique in the `items` it was created on.
        indexed: bool,
        /// Whether `items` and the tables of [`WIDENED`] have the column a round adds.
        widened: bool,
        /// Whether there is a table `tags`, and whether its `item` references `items`.
        tags: bool,
        references: bool,
        /// Whether `aa_children` references `items`, cascading deletes and updates.
        cascades: bool,
        /// Whether `zz_holds` references `items`, with no action.
        holds: bool,
        /// Whether `label_children` references `labels`.
        labelled: bool,
        /// Whether `items` is the table a round creates in place of the one it drops, and
        /// whether a round has since renamed that table's `label` as `note`.
        recreated: bool,
        renamed: bool,
    }

    impl PointRound {
        /// Whether a foreign key involves `table`, which the test knows from the catalog changes
        /// it made, not from the engine.
        fn involves(&self, table: &str) -> bool {
            match table {
                "items" => (self.tags && self.references) || self.cascades || self.holds,
                "tags" => self.tags && self.references,
                "labels" => self.labelled,
                _ => false,
            }
        }

        /// The id and the name the round gives a row of `items` or `tags` that its lists number
        /// and name, so that no two rounds write the same row or, where names are unique, the
        /// same name.
        fn rows(self) -> (impl Fn(i64) -> i64, impl Fn(&str) -> String) {
            let number = self.number;
            (
                move |id| 1000 * number as i64 + id,
                move |name| format!("{name} {number}"),
            )
        }

        /// A case on `tags`, which fails once a round has dropped the table.
        fn on_tags(&self, sql: &str, params: Value, expected: Expected) -> PointCase {
            let expected = if self.tags {
                expected
            } else {
                Fails("TABLE_NOT_FOUND")
            };
            case_on("tags", sql, params, expected)
        }
    }

    /// Two engines over the same database. One runs each statement prepared, which inside a
    /// transaction plans it from its point template where it has one; the other runs it as SQL
    /// with the same parameters, which plans it in full.
    struct PointPair {
        prepared: PagedEngine<MemoryPageDevice>,
        plain: PagedEngine<MemoryPageDevice>,
        /// Each distinct statement the prepared engine has run, prepared once and kept through
        /// every commit and catalog change.
        ids: Vec<(String, PreparedStatementId)>,
        round: PointRound,
        /// The setup, for a failure's message.
        setup: String,
    }

    impl PointPair {
        fn open(setup: String, script: &str, round: PointRound) -> Self {
            let open = || {
                let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
                engine.exec_sql(script).unwrap();
                engine
            };
            Self {
                prepared: open(),
                plain: open(),
                ids: Vec::new(),
                round,
                setup,
            }
        }

        /// The pair with each engine closed and opened again over what it stored, which
        /// forgets every statement the prepared one had prepared.
        fn reopened(self) -> Self {
            let reopen = |engine: PagedEngine<MemoryPageDevice>| {
                PagedEngine::open(engine.into_device()).unwrap()
            };
            Self {
                prepared: reopen(self.prepared),
                plain: reopen(self.plain),
                ids: Vec::new(),
                ..self
            }
        }

        /// Runs a script in both engines, outside a transaction.
        fn exec(&mut self, script: &str) {
            for engine in [&mut self.prepared, &mut self.plain] {
                if let Err(error) = engine.exec_sql(script) {
                    panic!("{} {script}: {error:?}", self.setup);
                }
            }
        }

        /// Runs `cases` in one transaction of each engine. The two must then publish the same
        /// changes and hold the same rows.
        fn transact(&mut self, cases: &[PointCase]) {
            self.prepared.begin_transaction().unwrap();
            self.plain.begin_transaction().unwrap();
            for case in cases {
                self.run(case);
            }
            let context = format!("{} round {}", self.setup, self.round.number);
            assert_eq!(
                self.prepared.commit_transaction().unwrap(),
                self.plain.commit_transaction().unwrap(),
                "{context}"
            );
            assert_eq!(
                self.prepared.database_hash(),
                self.plain.database_hash(),
                "{context}"
            );
        }

        /// Runs one execution in both engines, which must return the same result or error and
        /// then hold the same staged state, and holds the prepared engine to the outcome and
        /// the path the case expects, so that a statement that quietly stops being planned from
        /// its template fails here.
        fn run(&mut self, case: &PointCase) {
            let PointCase {
                table,
                sql,
                params,
                expected,
            } = case;
            let fast = if let Unprepared = expected {
                self.prepared.execute_sql(sql, params)
            } else {
                let id = match self.ids.iter().find(|(text, _)| text == sql) {
                    Some((_, id)) => *id,
                    None => {
                        let id = self.prepared.prepare_sql(sql).unwrap();
                        self.ids.push((sql.clone(), id));
                        // No statement is ever closed, and the registry holds 128 at most.
                        assert!(self.ids.len() < 128, "{sql}");
                        id
                    }
                };
                self.prepared.execute_prepared(id, params)
            };
            let full = self.plain.execute_sql(sql, params);
            // Written only for a failure, and without the bulk of a large parameter.
            let context = || {
                let mut shown = format!("{params:?}");
                if shown.len() > 400 {
                    shown = format!("{} parameters of {} bytes", params.len(), shown.len());
                }
                format!("{} round {}: {sql} {shown}", self.setup, self.round.number)
            };
            assert_eq!(fast, full, "{}", context());
            assert_eq!(
                self.prepared.transaction_fingerprint(),
                self.plain.transaction_fingerprint(),
                "{}",
                context()
            );
            let path = self.prepared.last_path.get();
            match (expected, &fast) {
                (Fails(code), Err(error)) => assert_eq!(error.code, *code, "{}", context()),
                (Unprepared, Ok(_)) => {}
                // Only a transaction plans a statement from its template.
                (Point | Unchanged, Ok(result)) if self.prepared.in_transaction() => {
                    let planned = if self.round.involves(table) {
                        StatementPath::PointEnforced
                    } else {
                        StatementPath::PointPlain
                    };
                    assert_eq!(path, Some(planned), "{}", context());
                    // A statement that stopped finding its row would still agree with the
                    // other engine's, and would no longer compare what it is here to compare.
                    let unchanged = matches!(expected, Unchanged);
                    assert_eq!(result.row_count == 0, unchanged, "{}", context());
                }
                (Full, Ok(_)) => assert_eq!(path, Some(StatementPath::General), "{}", context()),
                _ => panic!("{}: expected {expected:?}, not {fast:?}", context()),
            }
        }
    }

    /// A column of a [`ShapedTable`]: its name, its SQL type, and the rest of its definition.
    struct ShapedColumn {
        name: String,
        kind: &'static str,
        rest: &'static str,
    }

    /// A table the equivalence test writes statements for from its columns, so that every key
    /// shape and width runs the same shapes of statement: its columns in schema order, and its
    /// primary key's columns in key order, which need not be the schema's.
    struct ShapedTable {
        name: &'static str,
        columns: Vec<ShapedColumn>,
        key: Vec<&'static str>,
        /// Whether the table runs every shape of statement, or only those that name one row by
        /// its whole key: an insert of it, an update, a delete and an upsert.
        every_shape: bool,
    }

    /// The shaped tables a round widens with `ALTER TABLE ... ADD COLUMN`.
    const WIDENED: [&str; 2] = ["pairs", "wide17"];

    impl ShapedTable {
        fn new(
            name: &'static str,
            columns: &[(&str, &'static str, &'static str)],
            key: &[&'static str],
            every_shape: bool,
        ) -> Self {
            Self {
                name,
                columns: columns
                    .iter()
                    .map(|(name, kind, rest)| ShapedColumn {
                        name: (*name).to_owned(),
                        kind,
                        rest,
                    })
                    .collect(),
                key: key.to_vec(),
                every_shape,
            }
        }

        /// A table of `count` columns keyed by its first, the others taking each type in turn
        /// and being in turn nullable with no default, nullable with one, and required with
        /// one, so that every type has a default of each kind.
        fn wide(name: &'static str, count: usize) -> Self {
            let mut table = Self::new(name, &[("id", "INTEGER", "")], &["id"], true);
            for n in 1..count {
                table.columns.push(ShapedColumn {
                    name: format!("c{n}"),
                    kind: ["INTEGER", "TEXT", "BOOLEAN", "FLOAT"][n % 4],
                    rest: match (n % 3, n % 4) {
                        (0, _) => "",
                        (1, 0) => "DEFAULT 40",
                        (1, 1) => "DEFAULT 'one'",
                        (1, 2) => "DEFAULT true",
                        (1, _) => "DEFAULT 2.5",
                        (_, 0) => "NOT NULL DEFAULT -4",
                        (_, 1) => "NOT NULL DEFAULT ''",
                        (_, 2) => "NOT NULL DEFAULT false",
                        (_, _) => "NOT NULL DEFAULT 3",
                    },
                });
            }
            table
        }

        fn create(&self) -> String {
            let columns = self
                .columns
                .iter()
                .map(|column| format!("{} {} {}", column.name, column.kind, column.rest))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "CREATE TABLE {} ({columns}, PRIMARY KEY ({}))",
                self.name,
                self.key.join(", ")
            )
        }

        fn is_key(&self, column: &ShapedColumn) -> bool {
            self.key.contains(&column.name.as_str())
        }

        /// Every column, in schema order.
        fn all(&self) -> Vec<&ShapedColumn> {
            self.columns.iter().collect()
        }

        /// The key's columns, in key order.
        fn keys(&self) -> Vec<&ShapedColumn> {
            let column = |name| self.columns.iter().find(|column| column.name == name);
            self.key.iter().map(|name| column(*name).unwrap()).collect()
        }

        /// The columns outside the key, in schema order.
        fn rest(&self) -> Vec<&ShapedColumn> {
            self.columns
                .iter()
                .filter(|column| !self.is_key(column))
                .collect()
        }

        fn names(columns: &[&ShapedColumn]) -> String {
            let names = columns.iter().map(|column| column.name.as_str());
            names.collect::<Vec<_>>().join(", ")
        }

        /// `count` parameter markers, numbered from `from`.
        fn markers(from: usize, count: usize) -> String {
            let markers = (from..from + count).map(|number| format!("${number}"));
            markers.collect::<Vec<_>>().join(", ")
        }

        /// Each of `columns` set equal to a parameter, numbered from `from`.
        fn equalities(columns: &[&ShapedColumn], from: usize) -> String {
            let equalities = columns
                .iter()
                .enumerate()
                .map(|(index, column)| format!("{} = ${}", column.name, from + index));
            equalities.collect::<Vec<_>>().join(" AND ")
        }

        /// A value for `column` in row `row`: the same for a key column whenever the row is,
        /// and for any other column a different one for each `version`.
        fn value(&self, column: &ShapedColumn, row: i64, version: i64) -> Value {
            let name = &column.name;
            match (column.kind, self.is_key(column)) {
                ("INTEGER", true) => json!(row),
                ("TEXT", true) => json!(format!("{name} {row}")),
                ("BOOLEAN", true) => json!(row % 2 != 0),
                // A FLOAT key is written as an integer in every other row, and read as a float.
                ("FLOAT", true) if row % 2 != 0 => json!(row),
                ("FLOAT", true) => json!(row as f64 + 0.5),
                ("INTEGER", false) => json!(row * 100 + version),
                ("TEXT", false) => json!(format!("{name} {row}.{version}")),
                ("BOOLEAN", false) => json!((row + version) % 2 == 0),
                ("FLOAT", false) => json!((row * 4 + version) as f64 / 4.0),
                _ => unreachable!("a shaped column has one of four types"),
            }
        }

        fn values(&self, columns: &[&ShapedColumn], row: i64, version: i64) -> Vec<Value> {
            let value = |column: &&ShapedColumn| self.value(column, row, version);
            columns.iter().map(value).collect()
        }

        /// The key of row `row`, in key order.
        fn key_of(&self, row: i64) -> Vec<Value> {
            self.values(&self.keys(), row, 0)
        }

        /// How a statement that finds a row by its key is planned, where a row has the key and
        /// where none has: from its template only where a lookup finds the key exactly, which it
        /// cannot for a FLOAT.
        fn keyed(&self) -> (Expected, Expected) {
            if self.keys().iter().any(|column| column.kind == "FLOAT") {
                (Full, Full)
            } else {
                (Point, Unchanged)
            }
        }

        fn case(&self, sql: String, params: Vec<Value>, expected: Expected) -> PointCase {
            case_on(self.name, &sql, Value::Array(params), expected)
        }

        /// `INSERT` of row `row`, listing every column in schema order.
        fn insert_all(&self, row: i64, version: i64, expected: Expected) -> PointCase {
            let all = self.all();
            let (names, markers) = (Self::names(&all), Self::markers(1, all.len()));
            self.case(
                format!("INSERT INTO {} ({names}) VALUES ({markers})", self.name),
                self.values(&all, row, version),
                expected,
            )
        }

        /// `INSERT` of row `row` with its values in schema order, listing no column.
        fn insert_bare(&self, row: i64, version: i64, expected: Expected) -> PointCase {
            let all = self.all();
            let markers = Self::markers(1, all.len());
            self.case(
                format!("INSERT INTO {} VALUES ({markers})", self.name),
                self.values(&all, row, version),
                expected,
            )
        }

        /// `INSERT` of the key of row `row` and one other column, the rest taking their
        /// defaults.
        fn insert_key(&self, row: i64, version: i64) -> PointCase {
            let columns = [self.keys(), vec![self.rest()[0]]].concat();
            let (names, markers) = (Self::names(&columns), Self::markers(1, columns.len()));
            self.case(
                format!("INSERT INTO {} ({names}) VALUES ({markers})", self.name),
                self.values(&columns, row, version),
                Point,
            )
        }

        /// `INSERT` of two rows, listing every column, last first.
        fn insert_rows(&self, first: i64, second: i64, expected: Expected) -> PointCase {
            let columns = self.columns.iter().rev().collect::<Vec<_>>();
            let count = columns.len();
            self.case(
                format!(
                    "INSERT INTO {} ({}) VALUES ({}), ({})",
                    self.name,
                    Self::names(&columns),
                    Self::markers(1, count),
                    Self::markers(count + 1, count)
                ),
                [
                    self.values(&columns, first, 0),
                    self.values(&columns, second, 0),
                ]
                .concat(),
                expected,
            )
        }

        /// `UPDATE` of one column of the row at `key`, the predicate naming the key's columns in
        /// key order and taking the first parameters, so that the statement's markers do not
        /// come in the order of their numbers.
        fn update_one(&self, key: Vec<Value>, value: Value, expected: Expected) -> PointCase {
            let keys = self.keys();
            self.case(
                format!(
                    "UPDATE {} SET {} = ${} WHERE {}",
                    self.name,
                    self.rest()[0].name,
                    keys.len() + 1,
                    Self::equalities(&keys, 1)
                ),
                [key, vec![value]].concat(),
                expected,
            )
        }

        /// `UPDATE` of every column outside the key of row `row`, last first and every third to
        /// its default, the predicate naming the key's columns last first.
        fn update_all(&self, row: i64, version: i64, expected: Expected) -> PointCase {
            let (mut assignments, mut params) = (Vec::new(), Vec::new());
            for (index, column) in self.rest().into_iter().rev().enumerate() {
                if index % 3 == 1 {
                    assignments.push(format!("{} = DEFAULT", column.name));
                } else {
                    params.push(self.value(column, row, version));
                    assignments.push(format!("{} = ${}", column.name, params.len()));
                }
            }
            let key = self.keys().into_iter().rev().collect::<Vec<_>>();
            let sql = format!(
                "UPDATE {} SET {} WHERE {}",
                self.name,
                assignments.join(", "),
                Self::equalities(&key, params.len() + 1)
            );
            params.extend(self.values(&key, row, 0));
            self.case(sql, params, expected)
        }

        /// `DELETE` of the row at `key`.
        fn delete(&self, key: Vec<Value>, expected: Expected) -> PointCase {
            let equalities = Self::equalities(&self.keys(), 1);
            self.case(
                format!("DELETE FROM {} WHERE {equalities}", self.name),
                key,
                expected,
            )
        }

        /// `INSERT` of a key and one other column, which a row at that key takes instead, the
        /// conflict target naming the key's columns last first.
        fn upsert_one(&self, key: Vec<Value>, value: Value, expected: Expected) -> PointCase {
            let columns = [self.keys(), vec![self.rest()[0]]].concat();
            let target = self.keys().into_iter().rev().collect::<Vec<_>>();
            self.case(
                format!(
                    "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT ({}) \
                     DO UPDATE SET {column} = EXCLUDED.{column}",
                    self.name,
                    Self::names(&columns),
                    Self::markers(1, columns.len()),
                    Self::names(&target),
                    column = self.rest()[0].name
                ),
                [key, vec![value]].concat(),
                expected,
            )
        }

        /// `INSERT` of row `row`, which a row at its key takes instead: every column outside
        /// the key from the proposed row, last first, but the first of them, which takes a
        /// constant.
        fn upsert_all(&self, row: i64, version: i64) -> PointCase {
            let (all, rest) = (self.all(), self.rest());
            let assignments = rest.iter().rev().map(|column| {
                if column.name == rest[0].name {
                    format!("{} = ${}", column.name, all.len() + 1)
                } else {
                    format!("{name} = EXCLUDED.{name}", name = column.name)
                }
            });
            let mut params = self.values(&all, row, version);
            params.push(self.value(rest[0], row, version + 1));
            self.case(
                format!(
                    "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT ({}) DO UPDATE SET {}",
                    self.name,
                    Self::names(&all),
                    Self::markers(1, all.len()),
                    Self::names(&self.keys()),
                    assignments.collect::<Vec<_>>().join(", ")
                ),
                params,
                self.keyed().0,
            )
        }

        /// The table's statements for a round: rows the round inserts, changes, deletes and
        /// inserts again, which are staged rows by then, and the rows the round before it
        /// left, which are stored ones.
        fn cases(&self, round: PointRound) -> Vec<PointCase> {
            let (keyed, absent) = self.keyed();
            let duplicate = Fails("CONSTRAINT_VIOLATION");
            // The round's rows are numbered from `base`, and those of the round before it from
            // `before`.
            let base = 10 * round.number as i64;
            let before = base - 10;
            let key = |row: i64| self.key_of(row);
            let other = |row: i64, version: i64| self.value(self.rest()[0], row, version);
            // A round's first two rows are new to it, except in a table keyed by a BOOLEAN,
            // whose two values every round writes: there the round before left the first of
            // them, and only the opening round left the second as well.
            let boolean = self.keys()[0].kind == "BOOLEAN";
            let left = |held: bool| if boolean && held { keyed } else { absent };
            let mut cases = vec![
                self.delete(key(base + 1), left(round.number > 0)),
                self.delete(key(base + 2), left(round.number == 1)),
                self.update_one(key(base + 2), other(base + 2, 1), absent),
                self.insert_all(base + 1, 0, Point),
                self.insert_all(base + 1, 1, duplicate),
                self.update_one(key(base + 1), other(base + 1, 2), keyed),
                self.upsert_one(key(base + 2), other(base + 2, 3), keyed),
                self.upsert_one(key(base + 2), other(base + 2, 4), keyed),
                self.delete(key(base + 1), keyed),
                self.upsert_one(key(base + 1), other(base + 1, 5), keyed),
            ];
            if round.number > 0 {
                cases.extend([
                    self.update_one(key(before + 1), other(before + 1, 6), keyed),
                    self.upsert_one(key(before + 1), other(before + 1, 7), keyed),
                    self.insert_all(before + 1, 8, duplicate),
                    self.delete(key(before + 2), keyed),
                ]);
            }
            if !self.every_shape {
                return cases;
            }
            // A row without a column list has a value too few once the table has another
            // column, and the row it would have put back is then not there to update.
            let (bare, restored) = if round.widened && WIDENED.contains(&self.name) {
                (Fails("INVALID_QUERY"), absent)
            } else {
                (Point, keyed)
            };
            cases.extend([
                self.insert_rows(base + 3, base + 4, Point),
                self.insert_rows(base + 4, base + 3, duplicate),
                self.insert_key(base + 5, 0),
                self.update_all(base + 3, 1, keyed),
                self.upsert_all(base + 4, 2),
                self.upsert_all(base + 6, 3),
                self.delete(key(base + 5), keyed),
                self.insert_bare(base + 5, 4, bare),
                self.update_all(base + 5, 5, restored),
            ]);
            if round.number > 0 {
                cases.extend([
                    self.update_all(before + 3, 6, keyed),
                    self.upsert_all(before + 4, 7),
                    self.insert_rows(base + 7, before + 3, duplicate),
                    self.delete(key(before + 6), keyed),
                ]);
            }
            cases
        }

        /// Statements on rows at `keys`, each a key that fits a stored key or one that does
        /// not, which assign `values`.
        fn sized_key_cases(
            &self,
            keys: Vec<(Vec<Value>, bool)>,
            values: [Value; 2],
        ) -> Vec<PointCase> {
            let [first, second] = values;
            let mut cases = Vec::new();
            for (key, fits) in keys {
                if fits {
                    cases.extend([
                        self.upsert_one(key.clone(), first.clone(), Point),
                        self.update_one(key.clone(), second.clone(), Point),
                        self.upsert_one(key.clone(), first.clone(), Point),
                        self.delete(key.clone(), Point),
                        self.upsert_one(key, second.clone(), Point),
                    ]);
                } else {
                    // No row has a key too long to store, so the general planner finds none,
                    // and refuses to write one.
                    cases.extend([
                        self.update_one(key.clone(), first.clone(), Full),
                        self.delete(key.clone(), Full),
                        self.upsert_one(key, second.clone(), Fails("INVALID_CHANGE")),
                    ]);
                }
            }
            cases
        }
    }

    /// The tables of the equivalence test with other shapes of key than `items` has, and two
    /// of many columns, whose defaults are of every type and kind.
    fn shaped_tables() -> Vec<ShapedTable> {
        vec![
            ShapedTable::new(
                "pairs",
                &[
                    ("v", "TEXT", ""),
                    ("b", "TEXT", ""),
                    ("n", "INTEGER", "NOT NULL DEFAULT 5"),
                    ("a", "INTEGER", ""),
                    ("f", "FLOAT", "DEFAULT 0.5"),
                ],
                &["a", "b"],
                true,
            ),
            ShapedTable::new(
                "spans",
                &[
                    ("s", "TEXT", ""),
                    ("live", "BOOLEAN", "NOT NULL DEFAULT true"),
                    ("n", "INTEGER", ""),
                    ("v", "TEXT", "DEFAULT 'none'"),
                ],
                &["s", "n"],
                false,
            ),
            ShapedTable::new(
                "labels",
                &[
                    ("code", "TEXT", ""),
                    ("v", "TEXT", ""),
                    ("n", "INTEGER", "NOT NULL DEFAULT 5"),
                ],
                &["code"],
                true,
            ),
            ShapedTable::new(
                "flags",
                &[("v", "TEXT", ""), ("flag", "BOOLEAN", "")],
                &["flag"],
                false,
            ),
            ShapedTable::new(
                "ratios",
                &[("ratio", "FLOAT", ""), ("v", "TEXT", "")],
                &["ratio"],
                false,
            ),
            ShapedTable::wide("wide17", 17),
            ShapedTable::wide("wide40", 40),
        ]
    }

    // The statements on `items` and `tags` that several lists of the equivalence test run. The
    // prepared engine prepares each once, however many lists and rounds run it.
    const ITEM_INSERT: &str =
        "INSERT INTO items (id, name, qty, note, price) VALUES ($1, $2, $3, $4, $5)";
    const ITEM_INSERT_NAME: &str = "INSERT INTO items (id, name) VALUES ($1, $2)";
    const ITEM_INSERT_QTY: &str = "INSERT INTO items (id, name, qty) VALUES ($1, $2, $3)";
    const ITEM_INSERT_BARE: &str = "INSERT INTO items VALUES ($1, $2, $3, $4, $5)";
    const ITEM_INSERT_DEFAULT: &str = "INSERT INTO items (id, name, qty) VALUES ($1, $2, DEFAULT)";
    const ITEM_INSERT_TWO: &str = "INSERT INTO items (id, name) VALUES ($1, $2), ($3, $4)";
    const ITEM_INSERT_TWO_QTY: &str =
        "INSERT INTO items (id, name, qty) VALUES ($1, $2, $3), ($4, $5, $6)";
    const ITEM_INSERT_THREE: &str =
        "INSERT INTO items (id, name, qty) VALUES ($1, $2, $3), ($4, $5, $6), ($7, $8, $9)";
    const ITEM_NOTE: &str = "UPDATE items SET note = $1 WHERE id = $2";
    const ITEM_NAME: &str = "UPDATE items SET name = $1 WHERE id = $2";
    const ITEM_QTY: &str = "UPDATE items SET qty = $1 WHERE id = $2";
    const ITEM_PRICE: &str = "UPDATE items SET price = $1 WHERE id = $2";
    const ITEM_ID: &str = "UPDATE items SET id = $1 WHERE id = $2";
    const ITEM_DEFAULT: &str = "UPDATE items SET qty = DEFAULT, note = $1 WHERE $2 = id";
    const ITEM_DELETE: &str = "DELETE FROM items WHERE id = $1";
    const ITEM_UPSERT_QTY: &str = "INSERT INTO items (id, name, qty) VALUES ($1, $2, $3) \
         ON CONFLICT (id) DO UPDATE SET qty = EXCLUDED.qty";
    const ITEM_UPSERT_NAME: &str = "INSERT INTO items (id, name) VALUES ($1, $2) \
         ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name";
    const ITEM_UPSERT_NOTE: &str = "INSERT INTO items (id, name, qty) VALUES ($1, $2, $3) \
         ON CONFLICT (id) DO UPDATE SET qty = EXCLUDED.qty, note = $4";
    const ITEM_UPSERT_CROSSED: &str = "INSERT INTO items (id, name, note) VALUES ($1, $2, $3) \
         ON CONFLICT (id) DO UPDATE SET qty = EXCLUDED.name, name = EXCLUDED.note";
    const ITEM_SELECT: &str = "SELECT id, qty, note FROM items WHERE id = $1";
    const ITEM_LITERAL_NOTE: &str = "UPDATE items SET note = 'lit' WHERE id = 902";
    const ITEM_LITERAL_KEY: &str = "UPDATE items SET qty = $2 WHERE id = 902";
    const ITEM_LITERAL_INSERT: &str = "INSERT INTO items (id, name, qty) VALUES (903, $1, 7)";
    const TAG_INSERT: &str = "INSERT INTO tags (id, item, tag) VALUES ($1, $2, $3)";
    const TAG_ITEM: &str = "UPDATE tags SET item = $1 WHERE id = $2";
    const TAG_DELETE: &str = "DELETE FROM tags WHERE id = $1";

    /// The rows each round finds in `items` and `tags`, which it writes outside a transaction,
    /// where a prepared statement is bound and planned in full.
    fn seed_cases(round: PointRound) -> Vec<PointCase> {
        let (n, s) = round.rows();
        let mut cases = vec![
            on_items(ITEM_INSERT, json!([n(1), s("one"), 1, null, 1.5]), Full),
            on_items(
                ITEM_INSERT,
                json!([n(2), s("two"), 2, "second", null]),
                Full,
            ),
            on_items(ITEM_INSERT, json!([n(3), s("three"), 3, null, null]), Full),
            // Rows that other tables reference, in the rounds that have those tables.
            on_items(
                ITEM_INSERT,
                json!([n(51), s("cascading"), 5, null, null]),
                Full,
            ),
            on_items(
                ITEM_INSERT,
                json!([n(52), s("followed"), 5, null, null]),
                Full,
            ),
            on_items(ITEM_INSERT, json!([n(53), s("held"), 5, null, null]), Full),
            on_items(
                ITEM_INSERT,
                json!([n(54), s("held in place"), 5, null, null]),
                Full,
            ),
            on_items(
                ITEM_INSERT,
                json!([n(55), s("tagged"), 5, null, null]),
                Full,
            ),
            // A row a round deletes and inserts again as it was, and one only a range finds.
            on_items(ITEM_INSERT, json!([n(84), s("same"), 4, "kept", 2.5]), Full),
            on_items(
                ITEM_INSERT,
                json!([n(85), s("lowest"), -9, null, null]),
                Full,
            ),
        ];
        if round.tags {
            cases.extend([
                case_on("tags", TAG_INSERT, json!([n(10), n(1), "a"]), Full),
                case_on("tags", TAG_INSERT, json!([n(11), n(3), "b"]), Full),
                case_on("tags", TAG_INSERT, json!([n(16), n(55), "c"]), Full),
            ]);
        }
        cases
    }

    /// The statements this test has always run, on the rows a round seeds, with a few more
    /// among them and statements on `tags` around them. Several fail, in both engines; several
    /// are not point statements at all.
    fn item_cases(round: PointRound) -> Vec<PointCase> {
        let (n, s) = round.rows();
        // A name another row holds is refused only where `items_name` makes names unique.
        let unique = if round.indexed {
            Fails("CONSTRAINT_VIOLATION")
        } else {
            Point
        };
        // A tag of an item that does not exist is refused only where `tags.item` references
        // `items`.
        let dangling = if round.references {
            Fails("CONSTRAINT_VIOLATION")
        } else {
            Point
        };
        // A row without a column list has a value too few once the table has another column.
        let bare = if round.widened {
            Fails("INVALID_QUERY")
        } else {
            Point
        };
        // A tag is gone with the item it referenced only where `tags.item` references `items`.
        let cascaded = if round.references { Unchanged } else { Point };
        vec![
            // A tag of a seeded item, moved to another, and to one that does not exist.
            round.on_tags(TAG_INSERT, json!([n(14), n(2), "e"]), Point),
            round.on_tags(TAG_ITEM, json!([n(1), n(14)]), Point),
            round.on_tags(TAG_ITEM, json!([n(98), n(14)]), dangling),
            round.on_tags(TAG_DELETE, json!([n(15)]), Unchanged),
            on_items(ITEM_INSERT, json!([n(4), s("four"), 4, null, 4.0]), Point),
            on_items(ITEM_INSERT_NAME, json!([n(5), s("five")]), Point),
            on_items(
                ITEM_INSERT_NAME,
                json!([n(5), s("again")]),
                Fails("CONSTRAINT_VIOLATION"),
            ),
            // That was the key of a staged row, and this is a stored row's.
            on_items(
                ITEM_INSERT_NAME,
                json!([n(2), s("stored")]),
                Fails("CONSTRAINT_VIOLATION"),
            ),
            on_items(ITEM_INSERT_NAME, json!([n(6), s("one")]), unique),
            on_items(
                ITEM_INSERT_QTY,
                json!([n(7), s("seven"), "many"]),
                Fails("TYPE_MISMATCH"),
            ),
            on_items(
                ITEM_INSERT_BARE,
                json!([n(9), s("nine"), 9, "ninth", 9.5]),
                bare,
            ),
            on_items(
                ITEM_INSERT_THREE,
                json!([
                    n(30),
                    s("thirty"),
                    3,
                    n(31),
                    s("thirty-one"),
                    1,
                    n(29),
                    s("twenty-nine"),
                    2
                ]),
                Point,
            ),
            on_items(
                ITEM_INSERT_TWO,
                json!([n(40), s("forty"), n(40), s("forty again")]),
                Fails("CONSTRAINT_VIOLATION"),
            ),
            // A third row with the key of the first, past a second whose key sorts before
            // theirs; then a second row with the key of a staged row, and of a stored one.
            on_items(
                ITEM_INSERT_THREE,
                json!([
                    n(48),
                    s("forty-eight"),
                    1,
                    n(47),
                    s("forty-seven"),
                    1,
                    n(48),
                    s("forty-eight again"),
                    1
                ]),
                Fails("CONSTRAINT_VIOLATION"),
            ),
            on_items(
                ITEM_INSERT_TWO,
                json!([n(41), s("forty-one"), n(30), s("thirty again")]),
                Fails("CONSTRAINT_VIOLATION"),
            ),
            on_items(
                ITEM_INSERT_TWO,
                json!([n(46), s("forty-six"), n(3), s("stored")]),
                Fails("CONSTRAINT_VIOLATION"),
            ),
            on_items(
                ITEM_INSERT_TWO_QTY,
                json!([n(42), s("forty-two"), 1, n(43), s("forty-three"), "lots"]),
                Fails("TYPE_MISMATCH"),
            ),
            on_items(
                ITEM_INSERT_TWO,
                json!([n(44), s("one"), n(45), s("forty-five")]),
                unique,
            ),
            on_items(ITEM_NOTE, json!(["noted", n(2)]), Point),
            on_items(ITEM_NOTE, json!(["missing", n(99)]), Unchanged),
            on_items(ITEM_DEFAULT, json!([null, n(3)]), Point),
            on_items(
                ITEM_NAME,
                json!([null, n(1)]),
                Fails("CONSTRAINT_VIOLATION"),
            ),
            on_items(ITEM_NAME, json!([s("two"), n(1)]), unique),
            on_items(ITEM_PRICE, json!([2, n(2)]), Point),
            on_items(ITEM_ID, json!([n(20), n(2)]), Full),
            on_items(ITEM_QTY, json!([3.5, n(1)]), Fails("TYPE_MISMATCH")),
            on_items(ITEM_DELETE, json!([n(5)]), Point),
            on_items(ITEM_DELETE, json!([n(5)]), Unchanged),
            // With the tag that references it, where `tags.item` references `items`.
            on_items(ITEM_DELETE, json!([n(3)]), Point),
            on_items(
                ITEM_UPSERT_NOTE,
                json!([n(1), s("uno"), 10, "upserted"]),
                Point,
            ),
            on_items(ITEM_UPSERT_QTY, json!([n(8), s("eight"), 8]), Point),
            on_items(ITEM_UPSERT_NAME, json!([n(8), s("two")]), unique),
            on_items(ITEM_UPSERT_NAME, json!([n(8), s("eighth")]), Point),
            round.on_tags(TAG_INSERT, json!([n(12), n(2), "c"]), dangling),
            round.on_tags(TAG_INSERT, json!([n(13), n(99), "d"]), dangling),
            on_items(
                "UPDATE items SET note = $1 WHERE id = $2 AND name = $3",
                json!(["x", n(1), s("uno")]),
                Full,
            ),
            on_items(ITEM_SELECT, json!([n(1)]), Full),
            // One of the two tags that reference an item by now, then the item, and then the
            // other tag.
            round.on_tags(TAG_DELETE, json!([n(14)]), Point),
            on_items(ITEM_DELETE, json!([n(1)]), Point),
            round.on_tags(TAG_DELETE, json!([n(10)]), cascaded),
        ]
    }

    /// Statements on `items` whose arguments are literals, and statements whose keys a lookup
    /// does not find exactly.
    fn key_cases(round: PointRound) -> Vec<PointCase> {
        let (n, s) = round.rows();
        // Literal and mixed arguments, on rows whose keys the statements spell out, which
        // every round therefore shares. One statement has no use for its first parameter.
        let mut cases = vec![
            on_items(
                "INSERT INTO items (id, name) VALUES (902, 'standing') \
                 ON CONFLICT (id) DO UPDATE SET note = 'kept'",
                json!([]),
                Point,
            ),
            on_items(ITEM_LITERAL_NOTE, json!([]), Point),
            on_items(ITEM_LITERAL_KEY, json!(["unused", round.number]), Point),
            on_items(ITEM_LITERAL_INSERT, json!([s("passing")]), Point),
            on_items("DELETE FROM items WHERE id = 903", json!([]), Point),
            // A listed row's `DEFAULT`, alone, and then among literals and a parameter that
            // another row takes as well.
            on_items(ITEM_INSERT_DEFAULT, json!([n(66), s("defaulted")]), Point),
            on_items(
                "INSERT INTO items (id, name, qty, note, price) \
                 VALUES ($1, $2, DEFAULT, 'listed', DEFAULT), ($3, $4, 6, $2, 1.5)",
                json!([n(67), s("listed"), n(68), s("listed too")]),
                Point,
            ),
            on_items(ITEM_INSERT_NAME, json!([n(70), s("keyed")]), Point),
        ];
        // Each key, how an upsert of it ends, and how an UPDATE and a DELETE that name it do,
        // which find the row wherever the upsert put one. The general planner compares a key
        // that is not an integer a lookup finds exactly, or refuses it.
        let mistyped = Fails("TYPE_MISMATCH");
        for (key, found, upserted) in [
            (json!(null), Full, Fails("CONSTRAINT_VIOLATION")),
            (json!(1.5), Full, mistyped),
            (json!("1"), mistyped, mistyped),
            (json!(true), mistyped, mistyped),
            (json!(9_007_199_254_740_992_u64), Full, mistyped),
            (json!(-9_007_199_254_740_992_i64), Full, mistyped),
            (json!(9_007_199_254_740_991_u64), Point, Point),
            (json!(-9_007_199_254_740_991_i64), Point, Point),
            // The key of the row inserted above, spelled as a float, which the general planner
            // finds.
            (json!(n(70) as f64), Full, mistyped),
        ] {
            cases.extend([
                on_items(ITEM_UPSERT_QTY, json!([key, s("keyed again"), 1]), upserted),
                on_items(ITEM_NOTE, json!(["keyed", key]), found),
                on_items(ITEM_DELETE, json!([key]), found),
            ]);
        }
        cases
    }

    /// Statements on `items` of shapes a template's planner declines or refuses, statements on
    /// rows other tables reference, and deletes by key among deletes of a range.
    fn declined_cases(round: PointRound) -> Vec<PointCase> {
        let (n, s) = round.rows();
        // An upsert that assigns the key its own value, and one that assigns a constant of the
        // wrong type ahead of a column the table does not have.
        const UPSERT_KEY: &str = "INSERT INTO items (id, name) VALUES ($1, $2) \
             ON CONFLICT (id) DO UPDATE SET id = EXCLUDED.id, name = EXCLUDED.name";
        const UPSERT_UNKNOWN: &str = "INSERT INTO items (id, name, qty) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO UPDATE SET qty = $4, nope = EXCLUDED.name";
        // The general planner plans a delete of a range, and both engines run it as SQL.
        const RANGE: &str = "DELETE FROM items WHERE qty < $1";
        // A row another table references with no action can neither go nor move.
        let held = |otherwise: Expected| {
            if round.holds {
                Fails("CONSTRAINT_VIOLATION")
            } else {
                otherwise
            }
        };
        // Nor can a row move that a tag references, where `tags.item` references `items`: that
        // key has an action for a delete alone.
        let tagged = if round.tags && round.references {
            Fails("CONSTRAINT_VIOLATION")
        } else {
            Full
        };
        vec![
            on_items(ITEM_INSERT_NAME, json!([n(74), s("shaped")]), Point),
            on_items(
                "UPDATE items SET nope = $1 WHERE id = $2",
                json!(["x", n(74)]),
                Fails("COLUMN_NOT_FOUND"),
            ),
            on_items(
                "UPDATE items SET note = $1 WHERE name = $2",
                json!(["by name", s("shaped")]),
                Full,
            ),
            // A conflict target that is not the key needs the unique index on it.
            on_items(
                "INSERT INTO items (id, name, qty) VALUES ($1, $2, $3) \
                 ON CONFLICT (name) DO UPDATE SET qty = EXCLUDED.qty",
                json!([n(71), s("shaped"), 40]),
                if round.indexed {
                    Full
                } else {
                    Fails("INVALID_QUERY")
                },
            ),
            // The first inserts from its template; the second finds that row, which is left to
            // the general planner.
            on_items(UPSERT_KEY, json!([n(72), s("seventy-two")]), Point),
            on_items(UPSERT_KEY, json!([n(72), s("seventy-two again")]), Full),
            on_items(ITEM_DELETE, json!([n(72)]), Point),
            on_items(
                "INSERT INTO items (id, name) VALUES ($1, $2, $3)",
                json!([n(73), s("too many"), 1]),
                Fails("INVALID_QUERY"),
            ),
            on_items(
                "INSERT INTO items (id, name, qty) VALUES ($1, $2) \
                 ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name",
                json!([n(73), s("too few")]),
                Fails("INVALID_QUERY"),
            ),
            on_items(
                "INSERT INTO items (id, id, name) VALUES ($1, $2, $3)",
                json!([n(73), n(75), s("twice")]),
                Fails("INVALID_QUERY"),
            ),
            on_items(
                "INSERT INTO items (id, nope) VALUES ($1, $2)",
                json!([n(73), s("unknown")]),
                Fails("COLUMN_NOT_FOUND"),
            ),
            // For a new row, and for a stored one.
            on_items(
                UPSERT_UNKNOWN,
                json!([n(73), s("mistyped"), 1, "text"]),
                Fails("TYPE_MISMATCH"),
            ),
            on_items(
                UPSERT_UNKNOWN,
                json!([n(74), s("mistyped"), 1, "text"]),
                Fails("TYPE_MISMATCH"),
            ),
            on_items(
                "INSERT INTO items DEFAULT VALUES",
                json!([]),
                Fails("CONSTRAINT_VIOLATION"),
            ),
            // Rows other tables reference, in the rounds that have those tables: two whose
            // references go or move with them, and three whose references hold them.
            on_items(ITEM_DELETE, json!([n(51)]), Point),
            on_items(ITEM_ID, json!([n(62), n(52)]), Full),
            on_items(ITEM_DELETE, json!([n(53)]), held(Point)),
            on_items(ITEM_ID, json!([n(64), n(54)]), held(Full)),
            on_items(ITEM_ID, json!([n(65), n(55)]), tagged),
            // Deletes by key among deletes of a range, which find a stored row and staged
            // ones, and no row that a delete by key has taken.
            on_items(
                ITEM_INSERT_THREE,
                json!([
                    n(80),
                    s("low"),
                    -1,
                    n(81),
                    s("lower"),
                    -2,
                    n(82),
                    s("lower still"),
                    -3
                ]),
                Point,
            ),
            on_items(ITEM_INSERT_QTY, json!([n(83), s("low too"), -1]), Point),
            on_items(RANGE, json!([-2]), Unprepared),
            on_items(ITEM_DELETE, json!([n(80)]), Point),
            on_items(RANGE, json!([-1]), Unprepared),
            on_items(ITEM_DELETE, json!([n(81)]), Unchanged),
            on_items(RANGE, json!([0]), Unprepared),
            // A row deleted and inserted again as it was.
            on_items(ITEM_DELETE, json!([n(84)]), Point),
            on_items(
                ITEM_INSERT,
                json!([n(84), s("same"), 4, "kept", 2.5]),
                Point,
            ),
        ]
    }

    /// Statements on `items` that are refused, most of them with two faults, where the order
    /// in which a planner looks decides which of the two it reports.
    fn refused_cases(round: PointRound) -> Vec<PointCase> {
        let (n, s) = round.rows();
        const REORDERED: &str = "INSERT INTO items (qty, name, id) VALUES ($1, $2, $3)";
        const UPDATE_TWO: &str = "UPDATE items SET qty = $1, name = $2 WHERE id = $3";
        let (mistyped, violated) = (Fails("TYPE_MISMATCH"), Fails("CONSTRAINT_VIOLATION"));
        vec![
            // A listed row's values are checked in the order of the table's columns, so a
            // null name is found before a mistyped quantity listed ahead of it.
            on_items(REORDERED, json!([5, s("reordered"), n(69)]), Point),
            on_items(REORDERED, json!(["many", null, n(60)]), violated),
            // An UPDATE's values are checked in the order it assigns them, before the row is
            // looked for, and a column it assigns twice is the general planner's to refuse.
            on_items(UPDATE_TWO, json!(["many", null, n(69)]), mistyped),
            on_items(UPDATE_TWO, json!([6, s("reordered twice"), n(69)]), Point),
            on_items(ITEM_QTY, json!([3.5, n(99)]), mistyped),
            on_items(
                "UPDATE items SET qty = $1, qty = $2 WHERE id = $3",
                json!([1, 2, n(69)]),
                Fails("INVALID_QUERY"),
            ),
            // Of two listed rows, the first's key is found taken before the second's value
            // is found mistyped.
            on_items(
                ITEM_INSERT_TWO_QTY,
                json!([n(69), s("taken"), 1, n(61), s("mistyped"), "lots"]),
                violated,
            ),
            // A key that is null, and one that is a float.
            on_items(ITEM_INSERT_NAME, json!([null, s("keyless")]), violated),
            on_items(
                ITEM_INSERT_NAME,
                json!([n(61) as f64, s("float")]),
                mistyped,
            ),
            // An upsert's constant is checked before the row it proposes, whose values are
            // checked in the order of the table's columns.
            on_items(ITEM_UPSERT_NOTE, json!([n(69), s("x"), "bad", 5]), mistyped),
            on_items(ITEM_UPSERT_QTY, json!([n(60), null, "many"]), violated),
            // What an upsert takes from the row it proposes is checked in the order of the
            // table's columns, and only where a row is there to take it.
            on_items(
                ITEM_UPSERT_CROSSED,
                json!([n(63), s("crossed"), null]),
                Point,
            ),
            on_items(
                ITEM_UPSERT_CROSSED,
                json!([n(63), s("crossed again"), null]),
                violated,
            ),
            on_items(
                ITEM_UPSERT_CROSSED,
                json!([n(63), s("crossed again"), "noted"]),
                mistyped,
            ),
        ]
    }

    /// Statements on `items` of shapes that have no point template, which only the general
    /// planner plans, among statements that have one, on the same rows.
    fn general_cases(round: PointRound) -> Vec<PointCase> {
        let (n, s) = round.rows();
        const UPSERT_SUM: &str = "INSERT INTO items (id, name, qty) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO UPDATE SET qty = items.qty + EXCLUDED.qty";
        const UPSERT_NOTHING: &str =
            "INSERT INTO items (id, name) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING";
        vec![
            on_items(
                "INSERT INTO items (id, name) VALUES ($1, $2) RETURNING *",
                json!([n(77), s("returned")]),
                Full,
            ),
            on_items(ITEM_NOTE, json!(["templated", n(77)]), Point),
            on_items(
                "UPDATE items SET note = $1 WHERE id = $2 RETURNING id, note",
                json!(["returned", n(77)]),
                Full,
            ),
            on_items(
                "UPDATE items SET qty = qty + $1 WHERE id = $2",
                json!([5, n(77)]),
                Full,
            ),
            // A stored row and a new one, for each clause.
            on_items(UPSERT_SUM, json!([n(77), s("summed"), 10]), Full),
            on_items(UPSERT_SUM, json!([n(78), s("summed"), 10]), Full),
            on_items(UPSERT_NOTHING, json!([n(78), s("ignored")]), Full),
            on_items(UPSERT_NOTHING, json!([n(79), s("new")]), Full),
            on_items(
                "UPDATE items SET note = $1 WHERE id >= $2",
                json!(["ranged", n(78)]),
                Full,
            ),
            on_items(ITEM_QTY, json!([1, n(78)]), Point),
            on_items(
                "DELETE FROM items WHERE id = $1 RETURNING name",
                json!([n(77)]),
                Full,
            ),
            on_items(ITEM_DELETE, json!([n(79)]), Point),
        ]
    }

    /// Statements on large rows, as transactions of their own, because the staged state of a
    /// large row is slow to compare: a row a template's planner can bound; two such rows,
    /// which take one parameter between them and are more than it plans in one statement; and
    /// a row too large for it to bound, which the general planner measures.
    fn large_cases(round: PointRound) -> [Vec<PointCase>; 3] {
        let (n, s) = round.rows();
        let large = "x".repeat(180_000);
        let bounded = |letter: &str| letter.repeat(90_000);
        [
            vec![
                on_items(
                    ITEM_INSERT,
                    json!([n(91), s("bounded"), 1, bounded("y"), null]),
                    Point,
                ),
                on_items(ITEM_NOTE, json!(["small", n(91)]), Point),
                on_items(ITEM_DELETE, json!([n(91)]), Point),
            ],
            vec![on_items(
                "INSERT INTO items (id, name, note) VALUES ($1, $2, $3), ($4, $5, $3)",
                json!([
                    n(92),
                    s("bounded too"),
                    bounded("z"),
                    n(93),
                    s("bounded as well")
                ]),
                Full,
            )],
            vec![
                on_items(
                    ITEM_INSERT,
                    json!([n(90), s("large"), 1, large, null]),
                    Full,
                ),
                // The row as it is stored is too large, and then the value assigned to it is.
                on_items(ITEM_NOTE, json!(["small", n(90)]), Full),
                on_items(ITEM_NOTE, json!([large, n(90)]), Full),
                on_items(ITEM_UPSERT_QTY, json!([n(90), s("large again"), 2]), Full),
                on_items(ITEM_DELETE, json!([n(90)]), Point),
                // And a row an upsert proposes is, where no row is there for it to rewrite.
                on_items(
                    ITEM_UPSERT_CROSSED,
                    json!([n(94), s("large too"), large]),
                    Full,
                ),
            ],
        ]
    }

    /// The statements of a round on the `items` a round created in place of the table every
    /// statement was prepared against, with its columns in another order, `qty` of another type
    /// and `note` under another name, until a round gives it that name again.
    fn recreated_cases(round: PointRound) -> Vec<PointCase> {
        let (n, s) = round.rows();
        let mistyped = Fails("TYPE_MISMATCH");
        // How a statement that names `note` ends once the table has a column of that name.
        let noted = |otherwise: Expected| {
            if round.renamed {
                otherwise
            } else {
                Fails("COLUMN_NOT_FOUND")
            }
        };
        vec![
            on_items(ITEM_INSERT_NAME, json!([n(1), s("one")]), Point),
            on_items(ITEM_INSERT_QTY, json!([n(2), s("two"), 2]), mistyped),
            on_items(ITEM_INSERT_QTY, json!([n(2), s("two"), "a pair"]), Point),
            // Values in the old order of the columns, and in the new.
            on_items(
                ITEM_INSERT_BARE,
                json!([n(9), s("nine"), 9, "ninth", 9.5]),
                mistyped,
            ),
            on_items(
                ITEM_INSERT_BARE,
                json!([s("nine"), n(9), 9.5, "nine", "ninth"]),
                Point,
            ),
            on_items(
                ITEM_INSERT,
                json!([n(4), s("four"), "4", null, 4.0]),
                noted(Point),
            ),
            on_items(
                ITEM_INSERT_THREE,
                json!([
                    n(30),
                    s("thirty"),
                    "three",
                    n(31),
                    s("thirty-one"),
                    null,
                    n(29),
                    s("twenty-nine"),
                    "two"
                ]),
                Point,
            ),
            on_items(ITEM_NOTE, json!(["noted", n(2)]), noted(Point)),
            on_items(ITEM_DEFAULT, json!([null, n(2)]), noted(Point)),
            on_items(ITEM_NAME, json!([s("uno"), n(1)]), Point),
            on_items(ITEM_QTY, json!([3, n(1)]), mistyped),
            on_items(ITEM_QTY, json!(["three", n(1)]), Point),
            on_items(ITEM_PRICE, json!([2, n(2)]), Point),
            on_items(ITEM_UPSERT_QTY, json!([n(2), s("deux"), "a couple"]), Point),
            on_items(ITEM_UPSERT_QTY, json!([n(8), s("eight"), "eight"]), Point),
            on_items(ITEM_UPSERT_NAME, json!([n(8), s("eighth")]), Point),
            on_items(ITEM_SELECT, json!([n(1)]), noted(Full)),
            on_items(ITEM_DELETE, json!([n(1)]), Point),
            // The row whose key two statements spell out, which the new table holds only once
            // a round has put it there.
            on_items(ITEM_LITERAL_NOTE, json!([]), noted(Point)),
            on_items(ITEM_UPSERT_NAME, json!([902, s("standing")]), Point),
            on_items(ITEM_LITERAL_KEY, json!(["unused", "none"]), Point),
            on_items(ITEM_LITERAL_INSERT, json!([s("passing")]), mistyped),
            // The new `qty` has no default.
            on_items(ITEM_INSERT_DEFAULT, json!([n(66), s("defaulted")]), Point),
            // Statements on `tags`, which find no table until a round creates one again.
            round.on_tags(TAG_INSERT, json!([n(14), n(2), "e"]), Point),
            round.on_tags(TAG_ITEM, json!([n(8), n(14)]), Point),
            round.on_tags(TAG_DELETE, json!([n(14)]), Point),
        ]
    }

    /// Statements on a table with JSON columns, which a template's planner plans only while
    /// every JSON value the row would hold is a scalar, whose text it can bound.
    fn doc_cases(round: PointRound) -> Vec<PointCase> {
        const INSERT: &str = "INSERT INTO docs (id, body, meta, title) VALUES ($1, $2, $3, $4)";
        const UPDATE: &str = "UPDATE docs SET body = $1, meta = $2, rank = $3 WHERE id = $4";
        const UPSERT: &str = "INSERT INTO docs (id, body, meta) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO UPDATE SET body = EXCLUDED.body";
        let n = |id: i64| 10 * round.number as i64 + id;
        let doc =
            |sql: &str, params: Value, expected: Expected| case_on("docs", sql, params, expected);
        vec![
            doc(INSERT, json!([n(1), 5, "plain", "first"]), Point),
            doc(INSERT, json!([n(2), {"a": [1]}, null, "second"]), Full),
            // The default of `meta` is an object.
            doc(
                "INSERT INTO docs (id, body) VALUES ($1, $2)",
                json!([n(3), "scalar"]),
                Full,
            ),
            // The row keeps its JSON columns, whose text only reading them bounds.
            doc(
                "UPDATE docs SET title = $1 WHERE id = $2",
                json!(["renamed", n(1)]),
                Full,
            ),
            doc(UPDATE, json!([7, null, "high", n(1)]), Point),
            doc(UPDATE, json!([{"b": 2}, 1, 2, n(1)]), Full),
            // A new row, and then that row, which keeps two of its JSON columns.
            doc(UPSERT, json!([n(4), 1, 2]), Point),
            doc(UPSERT, json!([n(4), 3, 4]), Full),
            doc("DELETE FROM docs WHERE id = $1", json!([n(2)]), Point),
        ]
    }

    /// `INSERT ... DEFAULT VALUES`, into a table whose key has a default, once the row the
    /// round before left is gone.
    fn single_cases(round: PointRound) -> Vec<PointCase> {
        const DEFAULTS: &str = "INSERT INTO singles DEFAULT VALUES";
        vec![
            case_on(
                "singles",
                "DELETE FROM singles WHERE id = $1",
                json!([1]),
                if round.number == 0 { Unchanged } else { Point },
            ),
            case_on("singles", DEFAULTS, json!([]), Point),
            case_on(
                "singles",
                DEFAULTS,
                json!([]),
                Fails("CONSTRAINT_VIOLATION"),
            ),
        ]
    }

    /// The statements of a round, as the transactions that run them. Each holds the statements
    /// of a table or two, a statement of each in turn, so that it stages a row and then finds
    /// it staged, and so that consecutive statements are seldom of one table. A round runs
    /// several, because every statement is compared by all that its transaction has staged.
    fn round_transactions(tables: &[ShapedTable], round: PointRound) -> Vec<Vec<PointCase>> {
        let table = |name: &str| tables.iter().find(|table| table.name == name).unwrap();
        let shaped = |name: &str| table(name).cases(round);
        let alternate = |first: Vec<PointCase>, second: Vec<PointCase>| {
            let (mut first, mut second) = (first.into_iter(), second.into_iter());
            let mut cases = Vec::new();
            loop {
                let before = cases.len();
                cases.extend(first.next());
                cases.extend(second.next());
                if cases.len() == before {
                    return cases;
                }
            }
        };
        // A text of `length` bytes that only this round writes.
        let text = |length: usize| {
            let mut text = format!("{length} bytes in round {} ", round.number);
            text.push_str(&"k".repeat(length - text.len()));
            text
        };
        // A text key takes its bytes, one more for each zero byte, and two to end it, of the
        // 1,024 a stored key has.
        let label_keys = [
            (
                format!("zero \0, 'quotes\" and né 漢 {}", round.number),
                true,
            ),
            (text(1_022), true),
            (text(1_023), false),
            (format!("\0{}", text(1_020)), true),
            (format!("\0{}", text(1_021)), false),
        ]
        .map(|(key, fits)| (vec![json!(key)], fits));
        // An integer takes eight more, after a text or before one, so every one of these
        // texts but the shortest passes the limit with it, and the longest does so alone.
        let span_keys = [1_014, 1_015, 1_020, 1_021, 1_022, 1_023].map(|length| {
            (
                vec![json!(text(length)), json!(round.number)],
                length == 1_014,
            )
        });
        let pair_keys = [1_014, 1_015].map(|length| {
            (
                vec![json!(round.number), json!(text(length))],
                length == 1_014,
            )
        });
        let values = || [json!("first"), json!("second")];
        let mut sized_cases = table("labels").sized_key_cases(label_keys.into(), values());
        // A listed row with a key too long to store, and one with a mistyped value as well,
        // which is found first.
        const LABEL_INSERT: &str = "INSERT INTO labels (code, v, n) VALUES ($1, $2, $3)";
        sized_cases.extend([
            case_on(
                "labels",
                LABEL_INSERT,
                json!([text(1_023), "v", 1]),
                Fails("INVALID_CHANGE"),
            ),
            case_on(
                "labels",
                LABEL_INSERT,
                json!([text(1_023), "v", "many"]),
                Fails("TYPE_MISMATCH"),
            ),
        ]);
        // Where `pairs` has an index, a key that long leaves no room in the index's entries.
        if !round.indexed {
            sized_cases.extend(table("pairs").sized_key_cases(pair_keys.into(), values()));
        }
        let spans = table("spans");
        let mut span_cases = spans.sized_key_cases(span_keys.into(), [json!(true), json!(false)]);
        // A row found by a predicate that names the key's columns last first, and then half of
        // a key, which finds the rows of this round that hold it.
        let base = 10 * round.number as i64;
        let (s, n) = (format!("s {}", base + 7), base + 7);
        span_cases.extend([
            spans.upsert_one(spans.key_of(base + 7), json!(true), Point),
            case_on(
                "spans",
                "UPDATE spans SET v = $1 WHERE n = $2 AND s = $3",
                json!(["last first", n, s]),
                Point,
            ),
            case_on(
                "spans",
                "DELETE FROM spans WHERE n = $1 AND s = $2",
                json!([n, s]),
                Point,
            ),
            case_on(
                "spans",
                "UPDATE spans SET v = $1 WHERE s = $2",
                json!(["half a key", format!("s {}", base + 1)]),
                Full,
            ),
            case_on(
                "spans",
                "DELETE FROM spans WHERE s = $1",
                json!(["no such row"]),
                Full,
            ),
        ]);
        let [items, keys, declined, others] = if round.recreated {
            [recreated_cases(round), Vec::new(), Vec::new(), Vec::new()]
        } else {
            let mut others = refused_cases(round);
            others.extend(general_cases(round));
            [
                item_cases(round),
                key_cases(round),
                declined_cases(round),
                others,
            ]
        };
        let mut transactions = vec![
            alternate(items, doc_cases(round)),
            alternate(keys, shaped("flags")),
            alternate(declined, shaped("ratios")),
            alternate(shaped("pairs"), shaped("spans")),
            alternate(shaped("wide17"), others),
            alternate(shaped("wide40"), shaped("labels")),
            alternate(sized_cases, single_cases(round)),
            span_cases,
        ];
        // Only the first round has the large rows.
        if round.number == 0 {
            transactions.extend(large_cases(round));
        }
        transactions
    }

    #[test]
    fn point_statements_stage_what_their_statements_stage() {
        let tables = shaped_tables();
        // Today's tables with their foreign key and indexes, then without the foreign key, then
        // without the indexes either, which is the shape of table the benchmarks write.
        for (references, indexes) in [(true, true), (false, true), (false, false)] {
            let mut script = vec![
                "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL, \
                 qty INTEGER NOT NULL DEFAULT 1, note TEXT, price FLOAT)"
                    .to_owned(),
                format!(
                    "CREATE TABLE tags (id INTEGER PRIMARY KEY, item INTEGER NOT NULL{}, \
                     tag TEXT NOT NULL)",
                    if references {
                        " REFERENCES items (id) ON DELETE CASCADE"
                    } else {
                        ""
                    }
                ),
                "CREATE TABLE docs (id INTEGER PRIMARY KEY, body JSON, \
                 meta JSON DEFAULT '{\"kind\": \"none\"}'::jsonb, rank JSON DEFAULT '3'::jsonb, \
                 title TEXT DEFAULT 'untitled')"
                    .to_owned(),
                "CREATE TABLE singles (id INTEGER PRIMARY KEY DEFAULT 1, v TEXT DEFAULT 'x')"
                    .to_owned(),
            ];
            script.extend(tables.iter().map(ShapedTable::create));
            if indexes {
                script.extend(
                    [
                        "CREATE UNIQUE INDEX items_name ON items (name)",
                        "CREATE INDEX items_qty ON items (qty)",
                        "CREATE INDEX pairs_v ON pairs (v)",
                        "CREATE INDEX wide17_c1 ON wide17 (c1)",
                        "CREATE INDEX wide40_c1 ON wide40 (c1)",
                    ]
                    .map(str::to_owned),
                );
            }
            let mut round = PointRound {
                number: 0,
                indexed: indexes,
                widened: false,
                tags: true,
                references,
                cascades: false,
                holds: false,
                labelled: false,
                recreated: false,
                renamed: false,
            };
            let mut pair = PointPair::open(
                format!("references={references} indexes={indexes}"),
                &script.join(";"),
                round,
            );
            for number in 0..9 {
                round.number = number;
                // Between rounds the catalog changes, outside a transaction, and the prepared
                // engine keeps every statement it has prepared.
                match number {
                    // Indexes come, which leaves each table's schema as it was.
                    1 => pair.exec(
                        "CREATE INDEX items_names ON items (name, qty);\
                         CREATE INDEX flags_v ON flags (v)",
                    ),
                    // They go, tables gain a column, and a column takes another default.
                    2 => {
                        pair.exec(
                            "DROP INDEX items_names;\
                             DROP INDEX flags_v;\
                             ALTER TABLE items ADD COLUMN origin TEXT DEFAULT 'added';\
                             ALTER TABLE pairs ADD COLUMN extra INTEGER NOT NULL DEFAULT 7;\
                             ALTER TABLE wide17 ADD COLUMN c17 TEXT;\
                             ALTER TABLE items ALTER COLUMN qty SET DEFAULT 4",
                        );
                        round.widened = true;
                    }
                    // New tables reference tables that statements prepared long before
                    // change: first with actions that follow a referenced row, from either
                    // side of those tables in the catalog's order.
                    3 => {
                        pair.exec(
                            "CREATE TABLE aa_children (id INTEGER PRIMARY KEY, item INTEGER \
                             REFERENCES items (id) ON DELETE CASCADE ON UPDATE CASCADE);\
                             CREATE TABLE label_children (id INTEGER PRIMARY KEY, code TEXT \
                             REFERENCES labels (code) ON DELETE CASCADE)",
                        );
                        round.cascades = true;
                        round.labelled = true;
                    }
                    // Then with none, which holds a referenced row where it is.
                    4 => {
                        pair.exec(
                            "CREATE TABLE zz_holds (id INTEGER PRIMARY KEY, \
                             item INTEGER REFERENCES items (id))",
                        );
                        round.holds = true;
                    }
                    // And an existing table gains the key it was set up without, once it has
                    // no row the key would refuse.
                    5 if !references => {
                        pair.exec(
                            "DELETE FROM tags;\
                             ALTER TABLE tags ADD CONSTRAINT tags_item FOREIGN KEY (item) \
                             REFERENCES items (id) ON DELETE CASCADE",
                        );
                        round.references = true;
                    }
                    // Every referencing table goes, and with them every foreign key. Nothing
                    // replaces `tags` for two rounds, in which its statements find no table.
                    6 => {
                        pair.exec(
                            "DROP TABLE aa_children;\
                             DROP TABLE label_children;\
                             DROP TABLE zz_holds;\
                             DROP TABLE tags",
                        );
                        round.cascades = false;
                        round.labelled = false;
                        round.holds = false;
                        round.tags = false;
                        round.references = false;
                    }
                    // A table of the same name replaces `items`, with its columns in another
                    // order, one of another type and one renamed.
                    7 => {
                        pair.exec(
                            "DROP TABLE items;\
                             CREATE TABLE items (name TEXT NOT NULL, id INTEGER PRIMARY KEY, \
                             price FLOAT, qty TEXT, label TEXT)",
                        );
                        round.recreated = true;
                    }
                    // The renamed column takes its old name, in the table as it stands, and
                    // there is a table `tags` again, so that statements refused for a round or
                    // two find their column and their table.
                    8 => {
                        pair.exec(
                            "ALTER TABLE items RENAME COLUMN label TO note;\
                             CREATE TABLE tags (id INTEGER PRIMARY KEY, item INTEGER NOT NULL, \
                             tag TEXT NOT NULL)",
                        );
                        round.renamed = true;
                        round.tags = true;
                    }
                    _ => {}
                }
                pair.round = round;
                if !round.recreated {
                    for case in seed_cases(round) {
                        pair.run(&case);
                    }
                }
                // The rows of the referencing tables, which only the actions of their keys
                // change afterwards.
                let (n, _) = round.rows();
                if round.cascades {
                    pair.exec(&format!(
                        "INSERT INTO aa_children (id, item) VALUES ({0}, {0}), ({1}, {1})",
                        n(51),
                        n(52)
                    ));
                }
                if round.holds {
                    pair.exec(&format!(
                        "INSERT INTO zz_holds (id, item) VALUES ({0}, {0}), ({1}, {1})",
                        n(53),
                        n(54)
                    ));
                }
                if round.labelled {
                    // The rows of `labels` that the round before left, one of which this round
                    // deletes.
                    let before = 10 * number as i64 - 10;
                    pair.exec(&format!(
                        "INSERT INTO label_children (id, code) VALUES \
                         ({}, 'code {}'), ({}, 'code {}')",
                        n(1),
                        before + 1,
                        n(2),
                        before + 2
                    ));
                }
                // The first transactions find each database as opening it leaves it, with
                // whatever foreign key it was set up with and nothing committed to it since.
                if number == 0 {
                    pair = pair.reopened();
                }
                for cases in round_transactions(&tables, round) {
                    pair.transact(&cases);
                }
            }
            pair.prepared.check().unwrap();
            pair.plain.check().unwrap();
        }
    }

    /// The statements that create a table of entries whose notes name their rows' numbers in
    /// words, so that a `LIKE` pattern matches a known share of its rows, and insert `count` of
    /// them in batches within the SQL token limit. Every seventh note is `NULL`; the rest name
    /// the row's number in words, as "entry fifty three of the ledger", in at least 27 bytes.
    fn entries_seed(count: usize) -> Vec<String> {
        let mut statements = vec![
            "CREATE TABLE entries (id INTEGER PRIMARY KEY, amount INTEGER NOT NULL, \
             note TEXT, flag BOOLEAN NOT NULL DEFAULT false)"
                .to_owned(),
        ];
        statements.extend(worded_rows("entries", count, false));
        statements
    }

    /// The statements that insert the rows [`entries_seed`] describes into `table`, whose
    /// columns are `id`, `amount` and `note`, and with `json` a column `meta` as well, which
    /// holds the row's number and its note's words in every third row and NULL in the rest, so
    /// that measuring a row parses JSON.
    fn worded_rows(table: &str, count: usize, json: bool) -> Vec<String> {
        const TENS: [&str; 10] = [
            "zero", "ten", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty",
            "ninety",
        ];
        const ONES: [&str; 10] = [
            "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
        ];
        let columns = if json {
            "id, amount, note, meta"
        } else {
            "id, amount, note"
        };
        (0..count)
            .collect::<Vec<_>>()
            .chunks(200)
            .map(|batch| {
                let rows = batch
                    .iter()
                    .map(|id| {
                        let (tens, ones) = (TENS[(id / 10) % 10], ONES[id % 10]);
                        let note = if id % 7 == 0 {
                            "NULL".to_owned()
                        } else {
                            format!("'entry {tens} {ones} of the ledger'")
                        };
                        let meta = match json {
                            true if id % 3 == 0 => {
                                format!(r#", '{{"n": {id}, "words": ["{tens}", "{ones}"]}}'"#)
                            }
                            true => ", NULL".to_owned(),
                            false => String::new(),
                        };
                        format!("({id}, {}, {note}{meta})", (id * 37) % 10_000)
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("INSERT INTO {table} ({columns}) VALUES {rows}")
            })
            .collect()
    }

    #[test]
    fn script_dml_filters_rows_from_their_records() {
        // A standalone DELETE or UPDATE runs as a one-statement script, whose candidate reads
        // its table through the tree reader that tests each stored record against the filter
        // before presenting a row, as a read and a transaction do. Each statement here runs
        // standalone in one database and in a transaction of its own in another, both seeded
        // with enough rows to fill many leaves, and in the in-memory engine, which decodes and
        // judges every row; all three must report the same result, and the two databases must
        // hold the same rows afterwards.
        const ROWS: usize = 3_000;
        // The notes alone, of at least 27 bytes in six rows of seven, fill more pages than this,
        // so the rows span many leaves.
        const NOTE_PAGES: u64 = (ROWS * 6 / 7 * 27 / PAGE_SIZE) as u64;
        let statements = entries_seed(ROWS);
        let seed: Vec<&str> = statements.iter().map(String::as_str).collect();
        let (seeded, mut transaction) = (database_with(&seed), database_with(&seed));
        let mut oracle = Engine::default();
        for sql in &seed {
            oracle.execute_sql(sql, &[]).unwrap();
        }
        let device = seeded.into_device();
        assert!(
            device.page_count() > NOTE_PAGES,
            "{} pages hold the rows",
            device.page_count()
        );
        let mut script = PagedEngine::open(device).unwrap();
        // The in-memory engine holds rows under their keys' text, and returns them in that order,
        // so its rows are compared sorted by id, which every RETURNING here lists; the two paged
        // paths must agree on the order as well.
        let describe = |result: &Result<ExecuteResult>, sorted: bool| match result {
            Ok(result) => {
                let mut rows = result.rows.clone();
                if sorted {
                    rows.sort_by_key(|row| row["id"].as_i64());
                }
                format!(
                    "ok {} {} {:?} {:?} {:?}",
                    result.command, result.row_count, result.tables, result.keys, rows
                )
            }
            Err(error) => format!("err {} {}", error.code, error.message),
        };
        // The one predicate whose outcome the record tests change: an expression that divides
        // by zero at the fifth row, beside a LIKE no note matches. The reader's test rejects
        // every row from its record, so the expression judges none, and the script and the
        // transaction delete nothing; the in-memory engine, which judges every row in source
        // order, as the script did before its candidate tested records, fails at that row.
        let dividing = "DELETE FROM entries WHERE amount / (id - 5) > 0 AND note LIKE '%nothing%'";
        let standalone = script.execute_sql(dividing, &[]);
        transaction.begin_transaction().unwrap();
        let staged = transaction.execute_sql(dividing, &[]);
        transaction.commit_transaction().unwrap();
        assert_eq!(describe(&standalone, false), describe(&staged, false));
        assert_eq!(standalone.unwrap().row_count, 0);
        assert_eq!(
            oracle.execute_sql(dividing, &[]).unwrap_err().code,
            "DIVISION_BY_ZERO"
        );
        // Each predicate, and whether the reader's record tests decide it: a LIKE, a range of a
        // stored integer column, and IS NULL they do; a range of the key column, which records
        // do not hold, an OR, and a key term beside a LIKE leave the filter to judge the rows the
        // pass presents. The notes of three hundred rows hold "fifty", bar every seventh, which
        // is NULL.
        let fifties = (0..ROWS)
            .filter(|id| (id / 10) % 10 == 5 && id % 7 != 0)
            .count();
        let statements = [
            (
                "DELETE FROM entries WHERE note LIKE '%fifty%' RETURNING id, note",
                Some(fifties),
            ),
            (
                "UPDATE entries SET flag = true WHERE note LIKE 'entry twenty%' RETURNING id",
                None,
            ),
            (
                "UPDATE entries SET amount = amount + 1 WHERE amount >= 2000 AND amount < 4000",
                None,
            ),
            (
                "DELETE FROM entries WHERE id >= 1000 AND id < 1900 RETURNING id",
                None,
            ),
            (
                "UPDATE entries SET note = 'unnamed' WHERE note IS NULL RETURNING id",
                None,
            ),
            (
                "DELETE FROM entries WHERE note LIKE '%seven%' OR amount < 300 RETURNING id, amount",
                None,
            ),
            (
                "UPDATE entries SET flag = true WHERE id >= 2000 AND note LIKE '%three%' RETURNING id",
                None,
            ),
            ("DELETE FROM entries WHERE note LIKE '%nothing%'", Some(0)),
            (
                "DELETE FROM entries WHERE amount >= 1000 AND amount < 9000",
                None,
            ),
        ];
        for (sql, matched) in statements {
            let standalone = script.execute_sql(sql, &[]);
            transaction.begin_transaction().unwrap();
            let staged = transaction.execute_sql(sql, &[]);
            let expected = oracle.execute_sql(sql, &[]);
            assert_eq!(
                describe(&standalone, false),
                describe(&staged, false),
                "{sql}"
            );
            assert_eq!(
                describe(&standalone, true),
                describe(&expected, true),
                "{sql}"
            );
            transaction.commit_transaction().unwrap();
            assert_eq!(script.database_hash(), transaction.database_hash(), "{sql}");
            let row_count = standalone.unwrap().row_count;
            match matched {
                Some(matched) => assert_eq!(row_count, matched, "{sql}"),
                None => assert!(row_count > 0, "{sql}"),
            }
        }
        script.check().unwrap();
        transaction.check().unwrap();
        let remaining = "SELECT id, amount, note, flag FROM entries ORDER BY id";
        let rows = script.query_sql(remaining, &[]).unwrap().rows;
        assert_eq!(rows, transaction.query_sql(remaining, &[]).unwrap().rows);
        assert_eq!(rows, oracle.query_sql(remaining, &[]).unwrap().rows);
        assert!(!rows.is_empty());
    }

    #[test]
    fn script_dml_scans_are_bounded_by_the_scripts_operations() {
        // A script's DML scan is bounded by the operations the script may require, charged for
        // every row the reader examines, whether or not the filter accepts it. Over 4,000 rows,
        // 250 deletes that match nothing scan exactly the budget's 1,000,000 rows and succeed;
        // one more fails the whole script, which publishes nothing. A script inside a
        // transaction has the same budget, and fails the same way, staging nothing. The counts
        // pin a rejected row's cost at exactly one operation; a charge per statement would move
        // the boundary, and these counts with it.
        let statements = entries_seed(4_000);
        let seed: Vec<&str> = statements.iter().map(String::as_str).collect();
        let mut engine = database_with(&seed);
        let revision = engine.revision();
        let hash = engine.database_hash();
        let fruitless = "DELETE FROM entries WHERE note LIKE '%absent%';".repeat(250);
        let results = engine.exec_sql(&fruitless).unwrap();
        assert_eq!(results.len(), 250);
        assert!(results.iter().all(|result| result.row_count == 0));
        assert_eq!(engine.revision(), revision);
        let over = format!("{fruitless}DELETE FROM entries WHERE note LIKE '%fifty%';");
        let failure = |error: EngineError| (error.code, error.message);
        let too_large = (
            "TRANSACTION_TOO_LARGE".to_owned(),
            "A SQL script cannot require more than 1000000 row, index, and join operations"
                .to_owned(),
        );
        assert_eq!(failure(engine.exec_sql(&over).unwrap_err()), too_large);
        assert_eq!(engine.revision(), revision);
        assert_eq!(engine.database_hash(), hash);

        engine.begin_transaction().unwrap();
        let untouched = engine.transaction_fingerprint();
        assert_eq!(engine.exec_sql(&fruitless).unwrap().len(), 250);
        assert_eq!(failure(engine.exec_sql(&over).unwrap_err()), too_large);
        assert_eq!(engine.transaction_fingerprint(), untouched);
        engine.rollback_transaction().unwrap();
        assert_eq!(engine.database_hash(), hash);
        assert_eq!(
            engine
                .query_sql(
                    "SELECT COUNT(*) AS n FROM entries WHERE note LIKE '%fifty%'",
                    &[]
                )
                .unwrap()
                .rows,
            vec![row(json!({"n": 343}))]
        );
    }

    /// The statements that create a `ledger` table of `count` rows, with the columns
    /// [`worded_rows`] fills and `columns` more, shaped as `shape` says: `bare`, with an
    /// `indexed` or a `unique` index on its amounts, `referenced` by a table of tags that cascade
    /// its deletes, or `referencing` a table of books from its first hundred rows.
    fn ledger_seed(count: usize, shape: &str) -> Vec<String> {
        let ledger = |columns: &str| {
            format!(
                "CREATE TABLE ledger (id INTEGER PRIMARY KEY, amount INTEGER NOT NULL, \
                 note TEXT, meta JSON{columns})"
            )
        };
        let (before, after): (Vec<String>, Vec<String>) = match shape {
            "bare" => (vec![ledger("")], vec![]),
            "indexed" => (
                vec![
                    ledger(""),
                    "CREATE INDEX ledger_amount ON ledger (amount)".to_owned(),
                ],
                vec![],
            ),
            "unique" => (
                vec![
                    ledger(""),
                    "CREATE UNIQUE INDEX ledger_amount ON ledger (amount)".to_owned(),
                ],
                vec![],
            ),
            "referenced" => {
                // A tag for every fiftieth row, and for the row the tests delete by its key.
                let tags = (0..count)
                    .step_by(50)
                    .chain((count > 1234).then_some(1234))
                    .map(|id| format!("({id}, {id}, 'tag {id}')"))
                    .collect::<Vec<_>>()
                    .join(", ");
                (
                    vec![ledger("")],
                    vec![
                        "CREATE TABLE tags (id INTEGER PRIMARY KEY, entry INTEGER NOT NULL \
                         REFERENCES ledger (id) ON DELETE CASCADE, tag TEXT NOT NULL)"
                            .to_owned(),
                        format!("INSERT INTO tags (id, entry, tag) VALUES {tags}"),
                    ],
                )
            }
            "referencing" => (
                vec![
                    "CREATE TABLE books (id INTEGER PRIMARY KEY, title TEXT)".to_owned(),
                    "INSERT INTO books (id, title) VALUES (1, 'the ledger')".to_owned(),
                    ledger(", book INTEGER REFERENCES books (id)"),
                ],
                vec!["UPDATE ledger SET book = 1 WHERE id < 100".to_owned()],
            ),
            _ => unreachable!("an unknown shape"),
        };
        let mut statements = before;
        statements.extend(worded_rows("ledger", count, true));
        statements.extend(after);
        statements
    }

    #[test]
    fn script_delete_measures_rows_it_does_not_hold() {
        // A standalone DELETE from a table with no index and no foreign key keeps nothing of the
        // rows it deletes but what each is charged as, measured as it is planned, where a table
        // with an index, or one a foreign key involves, has every deleted row held for its
        // writer. Each DELETE here runs standalone in one database and in a transaction of its
        // own in another, both seeded with enough rows, with text and JSON columns, to fill many
        // leaves, for each shape of table: the two must report the same result and leave the
        // same rows. The delete of one row by its key runs standalone as any delete is planned,
        // not from a prepared statement's point template.
        const ROWS: usize = 3_000;
        // The notes alone, of at least 27 bytes in six rows of seven, fill more pages than this.
        const NOTE_PAGES: u64 = (ROWS * 6 / 7 * 27 / PAGE_SIZE) as u64;
        let fifties = (0..ROWS)
            .filter(|id| (id / 10) % 10 == 5 && id % 7 != 0)
            .count();
        let statements = [
            (
                "DELETE FROM ledger WHERE id = 1234 RETURNING id, note, meta",
                Some(1),
            ),
            ("DELETE FROM ledger WHERE id = 1234", Some(0)),
            (
                "DELETE FROM ledger WHERE note LIKE '%fifty%' RETURNING id, note, meta",
                Some(fifties),
            ),
            (
                "DELETE FROM ledger WHERE amount >= 1000 AND amount < 3000",
                None,
            ),
            (
                "DELETE FROM ledger WHERE id >= 2000 AND id < 2400 RETURNING id",
                None,
            ),
            ("DELETE FROM ledger WHERE meta IS NULL AND id < 600", None),
            ("DELETE FROM ledger", None),
        ];
        let describe = |result: &Result<ExecuteResult>| match result {
            Ok(result) => format!(
                "ok {} {} {:?} {:?} {:?}",
                result.command, result.row_count, result.tables, result.keys, result.rows
            ),
            Err(error) => format!("err {} {}", error.code, error.message),
        };
        for shape in ["bare", "indexed", "unique", "referenced", "referencing"] {
            let seed = ledger_seed(ROWS, shape);
            let seed: Vec<&str> = seed.iter().map(String::as_str).collect();
            let device = database_with(&seed).into_device();
            assert!(
                device.page_count() > NOTE_PAGES,
                "{shape}: {} pages hold the rows",
                device.page_count()
            );
            let mut script = PagedEngine::open(device).unwrap();
            let mut transaction = database_with(&seed);
            for (sql, matched) in statements {
                let standalone = script.execute_sql(sql, &[]);
                transaction.begin_transaction().unwrap();
                let staged = transaction.execute_sql(sql, &[]);
                transaction.commit_transaction().unwrap();
                assert_eq!(describe(&standalone), describe(&staged), "{shape}: {sql}");
                assert_eq!(
                    script.database_hash(),
                    transaction.database_hash(),
                    "{shape}: {sql}"
                );
                let row_count = standalone.unwrap().row_count;
                match matched {
                    Some(matched) => assert_eq!(row_count, matched, "{shape}: {sql}"),
                    None => assert!(row_count > 0, "{shape}: {sql}"),
                }
            }
            script.check().unwrap();
            transaction.check().unwrap();
            let remaining = "SELECT count(*) AS n FROM ledger";
            let rows = script.query_sql(remaining, &[]).unwrap().rows;
            assert_eq!(rows, transaction.query_sql(remaining, &[]).unwrap().rows);
            assert_eq!(rows, vec![row(json!({"n": 0}))], "{shape}");
            if shape == "referenced" {
                let tags = "SELECT id, entry, tag FROM tags ORDER BY id";
                let rows = script.query_sql(tags, &[]).unwrap().rows;
                assert_eq!(rows, transaction.query_sql(tags, &[]).unwrap().rows);
                assert!(rows.is_empty());
            }
        }
    }

    #[test]
    fn plan_delete_measures_the_rows_a_script_does_not_hold() {
        use crate::{row::HeldRow, statement::PreviousRow, storage::estimated_row_bytes};

        // What a script's candidate plans for a DELETE from a table with no index and no foreign
        // key keeps, for each deleted row, only the row's estimated bytes, which are those of the
        // map the row decodes to; with an index, or a foreign key referencing the table, it keeps
        // each row's stored entry.
        const ROWS: usize = 700;
        let seed = ledger_seed(ROWS, "bare");
        let seed: Vec<&str> = seed.iter().map(String::as_str).collect();
        let bare = database_with(&seed);
        let rows = bare
            .query_sql("SELECT * FROM ledger ORDER BY id", &[])
            .unwrap()
            .rows;
        assert_eq!(rows.len(), ROWS);
        // Each DELETE, and the rows it deletes: a LIKE over the table, one row by its key, which
        // a standalone statement plans as it plans any delete, and every row.
        type DeletesRow = fn(&Row) -> bool;
        let deletes: [(&str, DeletesRow); 3] = [
            ("DELETE FROM ledger WHERE note LIKE '%fifty%'", |row| {
                row["note"]
                    .as_str()
                    .is_some_and(|note| note.contains("fifty"))
            }),
            ("DELETE FROM ledger WHERE id = 123", |row| row["id"] == 123),
            ("DELETE FROM ledger", |_| true),
        ];
        for (sql, deletes_row) in &deletes {
            let planned = bare.plan_script_dml(sql).unwrap();
            let expected: Vec<&Row> = rows.iter().filter(|row| deletes_row(row)).collect();
            assert!(!expected.is_empty(), "{sql}");
            assert_eq!(planned.changes.len(), expected.len(), "{sql}");
            assert_eq!(planned.previous.len(), expected.len(), "{sql}");
            for (previous, row) in planned.previous.iter().zip(expected) {
                match previous {
                    PreviousRow::Read(Some(HeldRow::Measured(bytes))) => {
                        assert_eq!(*bytes, estimated_row_bytes(row).unwrap(), "{sql}");
                    }
                    other => panic!("{sql} keeps {other:?}"),
                }
            }
        }
        for shape in ["indexed", "unique", "referenced", "referencing"] {
            let seed = ledger_seed(ROWS, shape);
            let seed: Vec<&str> = seed.iter().map(String::as_str).collect();
            let engine = database_with(&seed);
            for (sql, _) in &deletes {
                let planned = engine.plan_script_dml(sql).unwrap();
                // The statement's own rows come first; the rows a foreign key's action deletes
                // with them follow, which planning does not read.
                let own = planned.outcome.row_count;
                assert!(own > 0, "{shape}: {sql}");
                assert_eq!(
                    planned.previous.len(),
                    planned.changes.len(),
                    "{shape}: {sql}"
                );
                for (position, previous) in planned.previous.iter().enumerate() {
                    let held = if position < own {
                        matches!(previous, PreviousRow::Read(Some(HeldRow::Stored(_))))
                    } else {
                        matches!(previous, PreviousRow::Unread)
                    };
                    assert!(held, "{shape}: {sql} keeps {previous:?} at {position}");
                }
            }
        }
    }

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
    fn point_statements_find_rows_by_keys_of_every_shape() {
        use crate::{RowChange, row::HeldRow, statement::PreviousRow};
        const SETUP: &[&str] = &[
            "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
            "INSERT INTO items (id, name) VALUES (1, 'one'), (2, 'two'), (3, 'three')",
            "CREATE TABLE notes (slug TEXT PRIMARY KEY, body TEXT NOT NULL)",
            "INSERT INTO notes (slug, body) VALUES ('a', 'first'), ('b', 'second')",
            "CREATE TABLE pairs (s TEXT, n INTEGER, v TEXT NOT NULL, PRIMARY KEY (s, n))",
            "INSERT INTO pairs (s, n, v) VALUES ('x', 1, 'x1'), ('x', 2, 'x2'), ('y', 1, 'y1')",
            "CREATE TABLE flags (lit BOOLEAN PRIMARY KEY, label TEXT NOT NULL)",
            "INSERT INTO flags (lit, label) VALUES (true, 'lit')",
            "CREATE TABLE ratios (r FLOAT PRIMARY KEY, label TEXT NOT NULL)",
            "INSERT INTO ratios (r, label) VALUES (1.5, 'half'), (2, 'whole')",
        ];
        let open = || {
            let mut engine = database_with(SETUP);
            engine.begin_transaction().unwrap();
            engine
        };
        // Each statement runs prepared in one engine and as SQL with the same parameters in the
        // other, as in `point_statements_stage_what_their_statements_stage`, whose keys are all
        // integers that a lookup finds. `point` says whether the statement's key is one the
        // point planner looks up, which it must then plan from, and otherwise must not.
        let (mut prepared, mut plain) = (open(), open());
        let mut ids: Vec<(&str, PreparedStatementId)> = Vec::new();
        // How many planned statements changed a row, and how many found none to change.
        let (mut changed, mut unchanged) = (0, 0);
        let mut run = |sql: &'static str, params: Vec<Value>, point: bool| {
            let id = match ids.iter().find(|(prepared, _)| *prepared == sql) {
                Some((_, id)) => *id,
                None => {
                    let id = prepared.prepare_sql(sql).unwrap();
                    ids.push((sql, id));
                    id
                }
            };
            let shown = params
                .iter()
                .map(|value| match value {
                    Value::String(text) if text.len() > 40 => format!("{} bytes", text.len()),
                    value => value.to_string(),
                })
                .collect::<Vec<_>>()
                .join(", ");
            let planned = {
                let view = prepared.read_view();
                let planned = match prepared.prepared_statements.point(id, &params) {
                    Ok(Some(template)) => {
                        crate::statement::plan_point(&view, template, &params).unwrap_or(None)
                    }
                    Ok(None) | Err(_) => None,
                };
                // A planned change comes with what the planner read at its key, so that staging
                // it reads nothing again: no row, or the stored row, whose entry begins with the
                // key the change is planned under.
                if let Some(planned) = &planned {
                    assert_eq!(planned.changes.len(), planned.previous.len());
                    for change in planned.changes.iter().zip(&planned.previous) {
                        match change {
                            (
                                RowChange::Put { key, .. },
                                PreviousRow::Read(Some(HeldRow::Stored(entry))),
                            ) => assert_eq!(entry.key(), key, "{sql} [{shown}]"),
                            (RowChange::Put { .. }, PreviousRow::Read(None))
                            | (
                                RowChange::Delete { .. },
                                PreviousRow::Read(Some(HeldRow::Stored(_))),
                            ) => {}
                            other => panic!("{sql} [{shown}] planned {other:?}"),
                        }
                    }
                }
                planned.is_some()
            };
            assert_eq!(planned, point, "{sql} [{shown}]");
            let fast = prepared.execute_prepared(id, &params);
            let full = plain.execute_sql(sql, &params);
            let describe = |result: &Result<ExecuteResult>| match result {
                Ok(result) => format!(
                    "ok {} {} {:?} {:?} {:?}",
                    result.command, result.row_count, result.tables, result.keys, result.rows
                ),
                Err(error) => format!("err {} {}", error.code, error.message),
            };
            assert_eq!(describe(&fast), describe(&full), "{sql} [{shown}]");
            assert_eq!(
                prepared.transaction_fingerprint(),
                plain.transaction_fingerprint(),
                "{sql} [{shown}]"
            );
            if point {
                match fast.unwrap().row_count {
                    0 => unchanged += 1,
                    _ => changed += 1,
                }
            }
        };
        // A key's statements: the row is updated, upserted, updated where the upsert staged it,
        // deleted, deleted again where nothing is left, upserted over the staged delete, and
        // updated once more.
        const ORDER: [usize; 7] = [0, 2, 0, 1, 1, 2, 0];
        let long = |length: usize| json!("k".repeat(length));

        // An INTEGER key: values of other types, numbers that are not integers or not safe ones,
        // and the largest and smallest safe integers.
        let statements = [
            "UPDATE items SET name = $2 WHERE id = $1",
            "DELETE FROM items WHERE id = $1",
            "INSERT INTO items (id, name) VALUES ($1, $2) \
             ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name",
        ];
        for (key, point) in [
            (json!(null), false),
            (json!(1.5), false),
            (json!(1.0), false),
            (json!("1"), false),
            (json!(true), false),
            (json!(9_007_199_254_740_992_u64), false),
            (json!(9_007_199_254_740_991_u64), true),
            (json!(-9_007_199_254_740_991_i64), true),
            (json!(2), true),
            (json!(77), true),
            (json!(0), true),
            (json!(-1), true),
        ] {
            for (step, statement) in ORDER.into_iter().enumerate() {
                let mut params = vec![key.clone()];
                if statement != 1 {
                    params.push(json!(format!("item at step {step}")));
                }
                run(statements[statement], params, point);
            }
        }

        // A TEXT key: empty, holding a zero byte, which its encoding escapes, and quotes; of
        // 1,022 bytes, which with its terminator is exactly the longest key, and of more.
        let statements = [
            "UPDATE notes SET body = $2 WHERE slug = $1",
            "DELETE FROM notes WHERE slug = $1",
            "INSERT INTO notes (slug, body) VALUES ($1, $2) \
             ON CONFLICT (slug) DO UPDATE SET body = EXCLUDED.body",
        ];
        for (key, point) in [
            (json!("a"), true),
            (json!("missing"), true),
            (json!(""), true),
            (json!("zero\u{0}byte"), true),
            (json!("\u{0}"), true),
            (json!("it's \"quoted\""), true),
            (long(1_021), true),
            (long(1_022), true),
            (long(1_023), false),
            (long(2_000), false),
            (json!(null), false),
            (json!(7), false),
            (json!(true), false),
        ] {
            for (step, statement) in ORDER.into_iter().enumerate() {
                let mut params = vec![key.clone()];
                if statement != 1 {
                    params.push(json!(format!("note at step {step}")));
                }
                run(statements[statement], params, point);
            }
        }

        // A key of a TEXT and an INTEGER column, named by the predicate and by the conflict
        // target in both orders. A first component of 1,014 bytes leaves exactly the room the
        // second needs; one of 1,022 fills a key by itself, and one of 1,023 passes it.
        let statements = [
            [
                "UPDATE pairs SET v = $3 WHERE s = $1 AND n = $2",
                "DELETE FROM pairs WHERE s = $1 AND n = $2",
                "INSERT INTO pairs (s, n, v) VALUES ($1, $2, $3) \
                 ON CONFLICT (s, n) DO UPDATE SET v = EXCLUDED.v",
            ],
            [
                "UPDATE pairs SET v = $3 WHERE n = $2 AND s = $1",
                "DELETE FROM pairs WHERE n = $2 AND s = $1",
                "INSERT INTO pairs (n, v, s) VALUES ($2, $3, $1) \
                 ON CONFLICT (n, s) DO UPDATE SET v = EXCLUDED.v",
            ],
        ];
        for (index, (s, n, point)) in [
            (json!("x"), json!(1), true),
            (json!("x"), json!(9), true),
            (json!("y"), json!(1), true),
            (json!(""), json!(0), true),
            (json!("zero\u{0}"), json!(-3), true),
            (long(1_014), json!(1), true),
            (long(1_015), json!(1), false),
            (long(1_020), json!(1), false),
            (long(1_021), json!(1), false),
            (long(1_022), json!(1), false),
            (long(1_023), json!(1), false),
            (json!("x"), json!(null), false),
            (json!(null), json!(1), false),
            (json!("x"), json!(1.5), false),
            (json!("x"), json!("1"), false),
            (json!(1), json!(1), false),
        ]
        .into_iter()
        .enumerate()
        {
            for (step, statement) in ORDER.into_iter().enumerate() {
                let mut params = vec![s.clone(), n.clone()];
                if statement != 1 {
                    params.push(json!(format!("pair at step {step}")));
                }
                run(statements[(index + step) % 2][statement], params, point);
            }
            // A predicate that names one of the two columns is not a lookup by key.
            run(
                "UPDATE pairs SET v = $2 WHERE s = $1",
                vec![s.clone(), json!("by half a key")],
                false,
            );
            run(
                "DELETE FROM pairs WHERE n = $1 AND v = $2",
                vec![n, s],
                false,
            );
        }

        // A BOOLEAN key.
        let statements = [
            "UPDATE flags SET label = $2 WHERE lit = $1",
            "DELETE FROM flags WHERE lit = $1",
            "INSERT INTO flags (lit, label) VALUES ($1, $2) \
             ON CONFLICT (lit) DO UPDATE SET label = EXCLUDED.label",
        ];
        for (key, point) in [
            (json!(true), true),
            (json!(false), true),
            (json!(null), false),
            (json!(1), false),
            (json!("true"), false),
        ] {
            for (step, statement) in ORDER.into_iter().enumerate() {
                let mut params = vec![key.clone()];
                if statement != 1 {
                    params.push(json!(format!("flag at step {step}")));
                }
                run(statements[statement], params, point);
            }
        }

        // A FLOAT key is never one the point planner looks up: a float's key is the same for
        // several spellings of its number, which the general planner reconciles.
        let statements = [
            "UPDATE ratios SET label = $2 WHERE r = $1",
            "DELETE FROM ratios WHERE r = $1",
            "INSERT INTO ratios (r, label) VALUES ($1, $2) \
             ON CONFLICT (r) DO UPDATE SET label = EXCLUDED.label",
        ];
        for key in [
            json!(1.5),
            json!(2),
            json!(2.0),
            json!(0),
            json!(-0.0),
            json!(null),
            json!("1.5"),
        ] {
            for (step, statement) in ORDER.into_iter().enumerate() {
                let mut params = vec![key.clone()];
                if statement != 1 {
                    params.push(json!(format!("ratio at step {step}")));
                }
                run(statements[statement], params, false);
            }
        }

        // The point planner looked up 22 keys, 5 of which held a row at the start. Five of a
        // key's seven statements change a row whatever it held, the second delete finds none,
        // and the first update finds one only where one was.
        assert_eq!((changed, unchanged), (22 * 5 + 5, 22 + 17));
        let fast = prepared.commit_transaction().unwrap();
        let full = plain.commit_transaction().unwrap();
        assert_eq!(format!("{fast:?}"), format!("{full:?}"));
        assert_eq!(prepared.database_hash(), plain.database_hash());
        // Both hold the rows the statements left. The row whose `id` was 1 is not among them:
        // a delete by the float 1.0 is not a lookup the point planner makes, and the general
        // planner, which both engines then ran, finds the row whose integer equals it.
        for (table, rows) in [
            ("items", 7),
            ("notes", 9),
            ("pairs", 7),
            ("flags", 2),
            ("ratios", 3),
        ] {
            let count = |engine: &mut PagedEngine<MemoryPageDevice>| {
                objects(
                    &engine
                        .execute_sql(&format!("SELECT * FROM {table}"), &[])
                        .unwrap(),
                )
                .len()
            };
            assert_eq!(count(&mut prepared), rows, "{table}");
            assert_eq!(count(&mut plain), rows, "{table}");
        }
    }

    #[test]
    fn a_database_that_needs_recovery_holds_no_committed_row_for_a_point_statement() {
        use crate::{paged_codec::PrimaryKey, row::HeldRow};
        let device = DurableDevice::default();
        let control = device.clone();
        let mut engine = page_native_fixture(device).unwrap();
        let update = engine
            .prepare_sql("UPDATE accounts SET active = $1 WHERE id = $2")
            .unwrap();
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
        // The transaction stays open over a database whose last commit may or may not have
        // happened, and its view reads no committed row, by the route a point statement takes
        // or by a visit of the key, though both still find the row the transaction staged.
        assert!(engine.in_transaction());
        let view = engine.read_view();
        let schema = Rc::clone(&engine.storage.table("accounts").unwrap().schema);
        for (id, staged) in [(1, false), (3, true), (9, false)] {
            let value = json!(id);
            let key = PrimaryKey::Values(&[&value]).encode(&schema).unwrap();
            let held = view.held_encoded_key("accounts", &key);
            let mut visited = None;
            let visit = view.visit_primary_key_values("accounts", &schema, &[&value], &mut |row| {
                visited = Some(row.hold()?);
                Ok(crate::VisitControl::Stop)
            });
            if staged {
                assert_eq!(visit.unwrap(), crate::VisitOutcome::Stopped);
                let Some(HeldRow::Stored(visited)) = visited else {
                    panic!("the staged row is visited as a stored entry");
                };
                let held = held.unwrap().unwrap();
                assert_eq!((held.key(), held.value()), (visited.key(), visited.value()));
                assert_eq!(held.key(), key);
            } else {
                let refusal = held.unwrap_err();
                assert_eq!(refusal, visit.unwrap_err());
                assert_eq!(refusal.code, "RECOVERY_REQUIRED");
            }
        }
        for id in [1, 3, 9] {
            assert_eq!(
                engine
                    .execute_prepared(update, &[json!(true), json!(id)])
                    .unwrap_err()
                    .code,
                "RECOVERY_REQUIRED"
            );
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
                values: None,
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
        // Stored rows take an added column's default, so a NOT NULL one needs a default that is
        // not NULL, unless the table has no rows.
        for (sql, code) in [
            (
                "ALTER TABLE accounts ADD COLUMN rank INTEGER NOT NULL",
                "CONSTRAINT_VIOLATION",
            ),
            (
                "ALTER TABLE accounts ADD COLUMN rank INTEGER NOT NULL DEFAULT NULL",
                "INVALID_SCHEMA",
            ),
        ] {
            let error = actual.execute_sql(sql, &[]).unwrap_err();
            assert_eq!(error.code, code);
            assert_eq!(error, expected.execute_sql(sql, &[]).unwrap_err());
        }
        assert_eq!(actual.revision(), revision);
        let mut empty = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        empty
            .exec_sql(
                "CREATE TABLE empty (id INTEGER PRIMARY KEY);\
                 ALTER TABLE empty ADD COLUMN rank INTEGER NOT NULL;",
            )
            .unwrap();

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
                .map(|result| result.command)
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
        assert_eq!(keys.columns(), ["team", "id"]);
        assert_eq!(keys.values, [json!("a"), json!(2), json!("b"), json!(1)]);

        // A transaction's statements and its commit list them the same way.
        engine.begin_transaction().unwrap();
        let updated = engine
            .execute_sql("UPDATE members SET role = 'z' WHERE team = 'b'", &[])
            .unwrap();
        assert_eq!(
            updated.keys.get("members").unwrap().columns(),
            ["team", "id"]
        );
        let outcome = engine.commit_transaction().unwrap();
        let keys = outcome.keys.get("members").unwrap();
        assert_eq!(keys.columns(), ["team", "id"]);
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
    fn a_star_over_a_join_lists_every_table_or_one() {
        let mut engine = users_engine();
        engine
            .exec_sql(
                "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, \
                   title TEXT NOT NULL);\
                 INSERT INTO posts VALUES (10, 1, 'first'), (11, 3, 'second');",
            )
            .unwrap();
        let rows = |result: ExecuteResult| {
            result
                .rows
                .into_iter()
                .map(Value::Object)
                .collect::<Vec<_>>()
        };

        // One table's columns, beside another's, read by name.
        let posts = "SELECT p.*, u.name FROM posts p JOIN users u ON u.id = p.user_id \
                     ORDER BY title DESC";
        assert_eq!(
            rows(engine.execute_sql(posts, &[]).unwrap()),
            [
                json!({"id": 11, "user_id": 3, "title": "second", "name": "bob"}),
                json!({"id": 10, "user_id": 1, "title": "first", "name": "cy"}),
            ]
        );
        // A table a left join extends with NULLs lists them.
        assert_eq!(
            rows(
                engine
                    .execute_sql(
                        "SELECT users.name, posts.* FROM users \
                         LEFT JOIN posts ON posts.user_id = users.id WHERE users.id = 2",
                        &[],
                    )
                    .unwrap()
            ),
            [json!({"name": "ann", "id": null, "user_id": null, "title": null})]
        );

        // Every table's columns repeat `id`, which only array rows can hold.
        let every = "SELECT * FROM posts JOIN users ON users.id = posts.user_id ORDER BY posts.id";
        assert_eq!(
            engine.execute_sql(every, &[]).unwrap_err().code,
            "INVALID_QUERY"
        );
        let result = engine.execute_sql_rows(every, &[], true).unwrap();
        assert_eq!(
            result
                .fields
                .iter()
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>(),
            ["id", "user_id", "title", "id", "name", "email", "active"]
        );
        assert_eq!(
            rows(result)[0],
            json!({"000id": 10, "001user_id": 1, "002title": "first", "003id": 1,
                "004name": "cy", "005email": "c@example.com", "006active": true})
        );
        let statement = engine.prepare_sql(every).unwrap();
        assert_eq!(
            engine
                .execute_prepared_rows(statement, &[], true)
                .unwrap()
                .rows
                .len(),
            2
        );

        assert_eq!(
            engine
                .execute_sql(
                    "SELECT x.* FROM posts p JOIN users u ON u.id = p.user_id",
                    &[],
                )
                .unwrap_err()
                .code,
            "INVALID_QUERY"
        );

        // A star beside other outputs of one table lists its columns in its place.
        assert_eq!(
            rows(
                engine
                    .execute_sql("SELECT *, id * 2 AS double FROM users WHERE id = 1", &[])
                    .unwrap()
            ),
            [
                json!({"id": 1, "name": "cy", "email": "c@example.com", "active": true,
                "double": 2})
            ]
        );
        let repeated = "SELECT u.id, u.* FROM users u WHERE u.id = 2";
        assert_eq!(
            engine.execute_sql(repeated, &[]).unwrap_err().code,
            "INVALID_QUERY"
        );
        assert_eq!(
            rows(engine.execute_sql_rows(repeated, &[], true).unwrap()),
            [
                json!({"000id": 2, "001id": 2, "002name": "ann", "003email": null,
                "004active": false})
            ]
        );
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
                (
                    "plain_a_b_key".to_owned(),
                    vec!["a".to_owned(), "b".to_owned()],
                    true
                ),
                ("plain_code_key".to_owned(), vec!["code".to_owned()], true),
            ]
        );
        assert_eq!(schema[1].0.primary_key, ["post_id", "tag"]);
        assert!(schema[1].1.is_empty());
        assert_eq!(
            names(2),
            [
                (
                    "users_email_unique".to_owned(),
                    vec!["email".to_owned()],
                    true
                ),
                ("users_name_idx".to_owned(), vec!["name".to_owned()], false),
            ]
        );
        assert_eq!(schema[2].0.columns[3].default, Some(json!({"tags": []})));
        engine
            .execute_sql(
                "INSERT INTO users (id, name, email) VALUES ('a', 'Ann', 'a@x')",
                &[],
            )
            .unwrap();
        assert_eq!(
            engine
                .execute_sql(
                    "INSERT INTO users (id, name, email) VALUES ('b', 'Bo', 'a@x')",
                    &[]
                )
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
        assert_eq!(
            rows(&mut engine, "SELECT ident FROM tee ORDER BY ident").len(),
            5
        );
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
            assert_eq!(
                error.code, "CONSTRAINT_VIOLATION",
                "{sql}: {}",
                error.message
            );
        }
        for (sql, code) in [
            (
                "CREATE TABLE w (id INTEGER PRIMARY KEY, c VARCHAR(2) DEFAULT 'abc')",
                "INVALID_SCHEMA",
            ),
            (
                "CREATE TABLE w (id INTEGER PRIMARY KEY, c VARCHAR(0))",
                "INVALID_SCHEMA",
            ),
            (
                "CREATE TABLE w (id INTEGER PRIMARY KEY, c VARCHAR(10485761))",
                "INVALID_SCHEMA",
            ),
            (
                "CREATE TABLE w (id INTEGER PRIMARY KEY, c INTEGER(5))",
                "UNSUPPORTED_SQL",
            ),
            (
                "CREATE TABLE w (id INTEGER PRIMARY KEY, c VARCHAR(n))",
                "UNSUPPORTED_SQL",
            ),
            (
                "ALTER TABLE v ALTER COLUMN id TYPE varchar(5)",
                "UNSUPPORTED_SQL",
            ),
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
                .execute_sql(
                    "ALTER TABLE v ALTER COLUMN code SET DATA TYPE varchar(3)",
                    &[]
                )
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

    fn query_values(engine: &PagedEngine<MemoryPageDevice>, sql: &str) -> Vec<Value> {
        engine
            .query_sql(sql, &[])
            .unwrap()
            .rows
            .into_iter()
            .map(Value::Object)
            .collect()
    }

    #[test]
    fn foreign_keys_hold_references_to_rows_that_exist() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql(
                r#"CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT UNIQUE);
                CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users,
                    author TEXT, CONSTRAINT posts_author_fk FOREIGN KEY (author)
                    REFERENCES "public"."users" (email));
                INSERT INTO users VALUES (1, 'a@x'), (2, 'b@x');
                INSERT INTO posts VALUES (10, 1, 'a@x'), (11, NULL, NULL);"#,
            )
            .unwrap();
        let schema = engine.schema().unwrap();
        let keys = &schema[0].0.foreign_keys;
        assert_eq!(
            keys.iter()
                .map(|key| (
                    key.name.as_str(),
                    key.columns.clone(),
                    key.references.as_str(),
                    key.referenced_columns.clone()
                ))
                .collect::<Vec<_>>(),
            [
                (
                    "posts_user_id_fkey",
                    vec!["user_id".to_owned()],
                    "users",
                    vec!["id".to_owned()]
                ),
                (
                    "posts_author_fk",
                    vec!["author".to_owned()],
                    "users",
                    vec!["email".to_owned()]
                ),
            ]
        );

        // A reference to a row that does not exist, or that a statement leaves without one, fails
        // and changes nothing.
        let revision = engine.revision();
        for sql in [
            "INSERT INTO posts VALUES (12, 3, NULL)",
            "INSERT INTO posts VALUES (12, NULL, 'c@x')",
            "UPDATE posts SET user_id = 9 WHERE id = 10",
            "DELETE FROM users WHERE id = 1",
            "UPDATE users SET id = 5 WHERE id = 1",
            "UPDATE users SET email = 'z@x' WHERE id = 1",
            "INSERT INTO users VALUES (1, 'q@x') ON CONFLICT (id) DO UPDATE SET email = EXCLUDED.email",
        ] {
            let error = engine.execute_sql(sql, &[]).unwrap_err();
            assert_eq!(
                error.code, "CONSTRAINT_VIOLATION",
                "{sql}: {}",
                error.message
            );
        }
        assert_eq!(engine.revision(), revision);
        // A row no reference names can go, and a reference can name a row its own statement
        // writes, as one statement's rows are checked as it ends.
        engine
            .exec_sql(
                "DELETE FROM users WHERE id = 2;\
                 INSERT INTO users (id) VALUES (3);\
                 INSERT INTO posts VALUES (12, 3, NULL);",
            )
            .unwrap();
        // A referenced value cannot change while a row references it.
        assert_eq!(
            engine
                .execute_sql("UPDATE users SET email = 'b@x' WHERE id = 1", &[])
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        engine.check().unwrap();

        // A transaction checks each statement against the rows it has staged.
        engine.begin_transaction().unwrap();
        engine
            .execute_sql("INSERT INTO users (id) VALUES (4)", &[])
            .unwrap();
        engine
            .execute_sql("INSERT INTO posts VALUES (13, 4, NULL)", &[])
            .unwrap();
        assert_eq!(
            engine
                .execute_sql("DELETE FROM users WHERE id = 4", &[])
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        engine.commit_transaction().unwrap();
        assert_eq!(
            query_values(&engine, "SELECT id FROM posts ORDER BY id"),
            [
                json!({"id": 10}),
                json!({"id": 11}),
                json!({"id": 12}),
                json!({"id": 13})
            ]
        );
        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        assert_eq!(reopened.schema().unwrap(), schema);
    }

    #[test]
    fn foreign_key_actions_change_the_rows_that_reference_a_changed_row() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql(
                "CREATE TABLE users (id INTEGER PRIMARY KEY);\
                 CREATE TABLE posts (id INTEGER PRIMARY KEY, \
                     user_id INTEGER REFERENCES users ON DELETE CASCADE ON UPDATE CASCADE, \
                     editor_id INTEGER REFERENCES users ON DELETE SET NULL, \
                     reviewer_id INTEGER DEFAULT 1 REFERENCES users ON DELETE SET DEFAULT);\
                 CREATE TABLE comments (id INTEGER PRIMARY KEY, \
                     post_id INTEGER NOT NULL REFERENCES posts ON DELETE CASCADE);\
                 CREATE TABLE nodes (id INTEGER PRIMARY KEY, \
                     parent_id INTEGER REFERENCES nodes ON DELETE CASCADE);\
                 INSERT INTO users VALUES (1), (2), (3);\
                 INSERT INTO posts VALUES (10, 2, 3, 3), (11, 3, 2, 2), (12, 1, 2, NULL);\
                 INSERT INTO comments VALUES (100, 10), (101, 10), (102, 11);\
                 INSERT INTO nodes VALUES (1, NULL), (2, 1), (3, 2), (4, NULL), (5, 4);",
            )
            .unwrap();

        // Deleting user 2 deletes post 10 and its comments, and empties or resets the others'
        // references.
        let result = engine
            .execute_sql("DELETE FROM users WHERE id = 2", &[])
            .unwrap();
        assert_eq!(result.row_count, 1);
        assert_eq!(result.tables, ["users", "posts", "comments"]);
        assert_eq!(
            query_values(&engine, "SELECT * FROM posts ORDER BY id"),
            [
                json!({"id": 11, "user_id": 3, "editor_id": null, "reviewer_id": 1}),
                json!({"id": 12, "user_id": 1, "editor_id": null, "reviewer_id": null}),
            ]
        );
        assert_eq!(
            query_values(&engine, "SELECT id FROM comments"),
            [json!({"id": 102})]
        );

        // An update of a referenced key carries to the rows that reference it.
        engine
            .execute_sql("UPDATE users SET id = 30 WHERE id = 3", &[])
            .unwrap();
        assert_eq!(
            query_values(&engine, "SELECT id, user_id FROM posts ORDER BY id"),
            [
                json!({"id": 11, "user_id": 30}),
                json!({"id": 12, "user_id": 1})
            ]
        );

        // A table can reference itself, and a cascade follows it down.
        engine
            .execute_sql("DELETE FROM nodes WHERE id = 1", &[])
            .unwrap();
        assert_eq!(
            query_values(&engine, "SELECT id FROM nodes ORDER BY id"),
            [json!({"id": 4}), json!({"id": 5})]
        );

        // An action the rows refuse refuses the statement: SET NULL into a NOT NULL column.
        engine
            .exec_sql(
                "CREATE TABLE tags (id INTEGER PRIMARY KEY, \
                     post_id INTEGER NOT NULL REFERENCES posts ON DELETE SET NULL);\
                 INSERT INTO tags VALUES (1, 11);",
            )
            .unwrap();
        assert_eq!(
            engine
                .execute_sql("DELETE FROM users WHERE id = 30", &[])
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );

        // A transaction stages what the actions change, and a rollback undoes it.
        engine.begin_transaction().unwrap();
        engine
            .execute_sql("DELETE FROM nodes WHERE id = 4", &[])
            .unwrap();
        assert!(query_values(&engine, "SELECT id FROM nodes").is_empty());
        engine.rollback_transaction().unwrap();
        assert_eq!(query_values(&engine, "SELECT id FROM nodes").len(), 2);
        engine.check().unwrap();
    }

    #[test]
    fn foreign_keys_follow_the_schema_they_belong_to() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT);\
                 CREATE UNIQUE INDEX users_email ON users (email);\
                 CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER, email TEXT);\
                 INSERT INTO users VALUES (1, 'a@x');\
                 INSERT INTO posts VALUES (10, 1, 'a@x'), (11, 2, NULL);",
            )
            .unwrap();
        for (sql, code) in [
            (
                "ALTER TABLE posts ADD CONSTRAINT posts_user FOREIGN KEY (user_id) REFERENCES users",
                "CONSTRAINT_VIOLATION",
            ),
            (
                "ALTER TABLE posts ADD FOREIGN KEY (user_id) REFERENCES missing",
                "TABLE_NOT_FOUND",
            ),
            (
                "ALTER TABLE posts ADD FOREIGN KEY (email) REFERENCES users (id)",
                "TYPE_MISMATCH",
            ),
            (
                "ALTER TABLE posts ADD FOREIGN KEY (user_id) REFERENCES posts (user_id)",
                "INVALID_SCHEMA",
            ),
            (
                "CREATE TABLE a (id INTEGER PRIMARY KEY REFERENCES users DEFERRABLE)",
                "UNSUPPORTED_SQL",
            ),
        ] {
            let error = engine.execute_sql(sql, &[]).unwrap_err();
            assert_eq!(error.code, code, "{sql}: {}", error.message);
        }
        // The SQL Drizzle Kit writes for a foreign key.
        engine
            .exec_sql(
                r#"DELETE FROM posts WHERE id = 11;
                ALTER TABLE "posts" ADD CONSTRAINT "posts_user_id_users_id_fk" FOREIGN KEY ("user_id") REFERENCES "public"."users"("id") ON DELETE cascade ON UPDATE no action;
                ALTER TABLE posts ADD CONSTRAINT posts_email FOREIGN KEY (email) REFERENCES users (email);"#,
            )
            .unwrap();

        // What a foreign key needs cannot be dropped without CASCADE, which drops the key.
        for sql in [
            "DROP TABLE users",
            "DROP INDEX users_email",
            "ALTER TABLE users DROP COLUMN email",
        ] {
            assert_eq!(
                engine.execute_sql(sql, &[]).unwrap_err().code,
                "INVALID_SCHEMA",
                "{sql}"
            );
        }
        engine
            .exec_sql(
                "ALTER TABLE users RENAME COLUMN id TO user_key;\
                 ALTER TABLE users RENAME TO people;\
                 ALTER TABLE posts RENAME COLUMN user_id TO person;",
            )
            .unwrap();
        let schema = engine.schema().unwrap();
        let posts = &schema[1].0;
        assert_eq!(posts.name, "posts");
        assert_eq!(posts.foreign_keys[0].columns, ["person"]);
        assert_eq!(posts.foreign_keys[0].references, "people");
        assert_eq!(posts.foreign_keys[0].referenced_columns, ["user_key"]);
        engine
            .execute_sql("DELETE FROM people WHERE user_key = 1", &[])
            .unwrap();
        assert!(query_values(&engine, "SELECT id FROM posts").is_empty());

        engine
            .exec_sql(
                "DROP INDEX users_email CASCADE;\
                 ALTER TABLE posts DROP CONSTRAINT posts_user_id_users_id_fk;",
            )
            .unwrap();
        assert!(engine.schema().unwrap()[1].0.foreign_keys.is_empty());
        engine
            .exec_sql(
                "ALTER TABLE posts ADD FOREIGN KEY (person) REFERENCES people;\
                 DROP TABLE people CASCADE;",
            )
            .unwrap();
        let schema = engine.schema().unwrap();
        assert_eq!(schema.len(), 1);
        assert!(schema[0].0.foreign_keys.is_empty());
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
            (
                "ALTER TABLE t RENAME COLUMN a TO b",
                "COLUMN_ALREADY_EXISTS",
            ),
            (
                "ALTER TABLE t RENAME COLUMN missing TO c",
                "COLUMN_NOT_FOUND",
            ),
            ("ALTER TABLE t RENAME TO u", "TABLE_ALREADY_EXISTS"),
            (
                "ALTER TABLE t ALTER COLUMN b SET DEFAULT 'x'",
                "INVALID_SCHEMA",
            ),
            (
                "ALTER TABLE t ALTER COLUMN id DROP NOT NULL",
                "INVALID_SCHEMA",
            ),
            ("ALTER TABLE t ALTER COLUMN b TYPE text", "UNSUPPORTED_SQL"),
            (
                "ALTER TABLE t ALTER COLUMN b TYPE text USING b::text",
                "UNSUPPORTED_SQL",
            ),
            (
                "ALTER TABLE t ALTER COLUMN b SET STATISTICS 100",
                "SQL_PARSE_ERROR",
            ),
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
    fn constraints_and_index_methods_tinyjoin_cannot_hold_are_refused() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql(
                "CREATE TABLE users (id TEXT PRIMARY KEY, name TEXT);\
                 CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id TEXT);",
            )
            .unwrap();
        for (sql, code, message) in [
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
                "cannot add a constraint",
            ),
            (
                "ALTER TABLE posts ADD CONSTRAINT posts_pk PRIMARY KEY (user_id)",
                "UNSUPPORTED_SQL",
                "primary key",
            ),
            (
                "ALTER TABLE posts DROP CONSTRAINT posts_missing",
                "INDEX_NOT_FOUND",
                "no constraint `posts_missing`",
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
        engine
            .exec_sql("CREATE INDEX taken ON posts (user_id)")
            .unwrap();
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
            .exec_sql(
                "CREATE TABLE words (unique INTEGER PRIMARY KEY, constraint TEXT, check TEXT)",
            )
            .unwrap();
    }

    fn schema_target(value: Value) -> crate::SchemaDefinition {
        crate::SchemaDefinition::from_json(&value).unwrap()
    }

    /// The schema as getSchema() writes it, which set_schema reads back.
    fn schema_json(engine: &PagedEngine<MemoryPageDevice>) -> Value {
        let tables = engine
            .schema()
            .unwrap()
            .iter()
            .map(|(table, indexes)| {
                json!({
                    "name": table.name,
                    "columns": table.columns.iter().map(|column| {
                        let mut value = json!({
                            "name": column.name,
                            "type": match column.data_type {
                                ColumnType::Boolean => "boolean",
                                ColumnType::Integer => "integer",
                                ColumnType::Float => "float",
                                ColumnType::Text => "text",
                                ColumnType::Json => "json",
                            },
                            "nullable": column.nullable,
                        });
                        if let Some(default) = &column.default {
                            value["default"] = default.clone();
                        }
                        if let Some(length) = column.max_length {
                            value["maxLength"] = json!(length);
                        }
                        value
                    }).collect::<Vec<_>>(),
                    "primaryKey": table.primary_key,
                    "indexes": indexes.iter().map(|index| json!({
                        "name": index.name, "columns": index.columns, "unique": index.unique,
                    })).collect::<Vec<_>>(),
                    "foreignKeys": table.foreign_keys.iter().map(|key| json!({
                        "name": key.name, "columns": key.columns, "references": key.references,
                        "referencedColumns": key.referenced_columns,
                        "onDelete": key.on_delete, "onUpdate": key.on_update,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();
        json!({"version": engine.schema_version(), "tables": tables})
    }

    #[test]
    fn set_schema_makes_the_database_match_and_keeps_its_rows() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        let first = json!({"version": 1, "tables": [{
            "name": "tasks",
            "columns": [
                {"name": "id", "type": "text", "nullable": false},
                {"name": "title", "type": "text", "nullable": false, "maxLength": 200},
                {"name": "done", "type": "boolean", "nullable": false, "default": false},
            ],
            "primaryKey": ["id"],
            "indexes": [{"name": "tasks_done", "columns": ["done"], "unique": false}],
            "foreignKeys": [],
        }]});
        let outcome = engine
            .set_schema(&schema_target(first.clone()), false)
            .unwrap();
        assert_eq!(outcome.tables, ["tasks"]);
        assert_eq!(schema_json(&engine), first);
        // A schema the database already has changes nothing.
        let revision = engine.revision();
        let outcome = engine
            .set_schema(&schema_target(first.clone()), false)
            .unwrap();
        assert!(outcome.tables.is_empty());
        assert_eq!(outcome.revision, revision);
        engine
            .execute_sql(
                "INSERT INTO tasks (id, title) VALUES ('a', 'First'), ('b', 'Second')",
                &[],
            )
            .unwrap();

        // Renames, a new column, new defaults, a new index, and a new table, all at once.
        let second = json!({"version": 2, "tables": [
            {
                "name": "todos",
                "renamedFrom": "tasks",
                "columns": [
                    {"name": "id", "type": "text", "nullable": false},
                    {"name": "name", "renamedFrom": "title", "type": "text", "nullable": false,
                        "maxLength": 100},
                    {"name": "done", "type": "boolean", "nullable": false, "default": true},
                    {"name": "priority", "type": "integer", "nullable": false, "default": 0},
                ],
                "primaryKey": ["id"],
                "indexes": [
                    {"name": "todos_name", "columns": ["name"], "unique": true},
                    {"name": "todos_priority", "columns": ["priority", "done"], "unique": false},
                ],
                "foreignKeys": [],
            },
            {
                "name": "notes",
                "columns": [{"name": "id", "type": "integer", "nullable": false}],
                "primaryKey": ["id"],
                "indexes": [],
                "foreignKeys": [],
            },
        ]});
        let outcome = engine
            .set_schema(&schema_target(second.clone()), false)
            .unwrap();
        assert_eq!(outcome.tables, ["tasks", "todos", "notes"]);
        let mut expected = second.clone();
        expected["tables"][0]
            .as_object_mut()
            .unwrap()
            .remove("renamedFrom");
        expected["tables"][0]["columns"][1]
            .as_object_mut()
            .unwrap()
            .remove("renamedFrom");
        let tables = expected["tables"].as_array_mut().unwrap();
        tables.reverse();
        assert_eq!(schema_json(&engine), expected);
        assert_eq!(
            engine
                .execute_sql(
                    "SELECT id, name, done, priority FROM todos ORDER BY id",
                    &[]
                )
                .unwrap()
                .rows
                .into_iter()
                .map(Value::Object)
                .collect::<Vec<_>>(),
            [
                json!({"id": "a", "name": "First", "done": false, "priority": 0}),
                json!({"id": "b", "name": "Second", "done": false, "priority": 0}),
            ]
        );

        // An older schema is refused, and so is a change that fails partway: nothing changes.
        assert_eq!(
            engine
                .set_schema(&schema_target(first.clone()), false)
                .unwrap_err()
                .code,
            "SCHEMA_OUTDATED"
        );
        let mut failing = expected.clone();
        failing["version"] = json!(3);
        failing["tables"][1]["columns"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name": "owner", "type": "text", "nullable": false}));
        failing["tables"][0]["columns"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name": "body", "type": "text", "nullable": true}));
        assert_eq!(
            engine
                .set_schema(&schema_target(failing), false)
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        assert_eq!(schema_json(&engine), expected);

        // What the schema leaves out stays, unless it is dropped.
        let third = json!({"version": 3, "tables": [{
            "name": "todos",
            "columns": [
                {"name": "id", "type": "text", "nullable": false},
                {"name": "name", "type": "text", "nullable": false, "maxLength": 100},
            ],
            "primaryKey": ["id"],
            "indexes": [{"name": "todos_name", "columns": ["name"], "unique": true}],
            "foreignKeys": [],
        }]});
        engine
            .set_schema(&schema_target(third.clone()), false)
            .unwrap();
        assert_eq!(engine.schema().unwrap().len(), 2);
        assert_eq!(engine.schema().unwrap()[1].0.columns.len(), 4);
        engine
            .set_schema(&schema_target(third.clone()), true)
            .unwrap();
        assert_eq!(schema_json(&engine), third);
        engine.check().unwrap();
        let engine = PagedEngine::open(engine.into_device()).unwrap();
        assert_eq!(schema_json(&engine), third);
        assert_eq!(engine.schema_version(), 3);
    }

    #[test]
    fn set_schema_refuses_what_no_statement_could_change() {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql("CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b INTEGER)")
            .unwrap();
        let table = |columns: Value, key: Value| {
            schema_target(json!({"version": 0, "tables": [{
                "name": "t", "columns": columns, "primaryKey": key, "indexes": [],
                "foreignKeys": [],
            }]}))
        };
        for (target, code) in [
            (
                table(
                    json!([{"name": "id", "type": "text", "nullable": false}]),
                    json!(["id"]),
                ),
                "UNSUPPORTED_SQL",
            ),
            (
                table(
                    json!([
                        {"name": "id", "type": "integer", "nullable": false},
                        {"name": "a", "type": "text", "nullable": false},
                    ]),
                    json!(["id", "a"]),
                ),
                "UNSUPPORTED_SQL",
            ),
            (
                table(
                    json!([{"name": "id", "type": "integer", "nullable": false},
                        {"name": "a", "type": "integer", "nullable": true}]),
                    json!(["id"]),
                ),
                "UNSUPPORTED_SQL",
            ),
        ] {
            assert_eq!(engine.set_schema(&target, false).unwrap_err().code, code);
        }
        for value in [
            json!({"version": 0, "tables": [], "extra": 1}),
            json!({"version": -1, "tables": []}),
            json!({"version": 0, "tables": [{"name": "t", "columns": [
                {"name": "id", "type": "bigint", "nullable": false}],
                "primaryKey": ["id"], "indexes": [], "foreignKeys": []}]}),
            json!({"version": 0, "tables": [{"name": "t", "columns": [
                {"name": "id", "type": "integer", "nullable": false, "maxLength": 3}],
                "primaryKey": ["id"], "indexes": [], "foreignKeys": []}]}),
            json!({"version": 0, "tables": [{"name": "t", "columns": [
                {"name": "id", "type": "integer"}], "primaryKey": ["id"], "indexes": [],
                "foreignKeys": []}]}),
        ] {
            assert_eq!(
                crate::SchemaDefinition::from_json(&value).unwrap_err().code,
                "INVALID_SCHEMA"
            );
        }
        engine.begin_transaction().unwrap();
        assert_eq!(
            engine
                .set_schema(&schema_target(json!({"version": 0, "tables": []})), true)
                .unwrap_err()
                .code,
            "TRANSACTION_ACTIVE"
        );
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

    fn nested_engine() -> PagedEngine<MemoryPageDevice> {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .exec_sql(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
                CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER, title TEXT);
                CREATE INDEX posts_user_id ON posts (user_id);
                CREATE TABLE comments (id INTEGER PRIMARY KEY, post_id INTEGER, body TEXT);
                INSERT INTO users VALUES (1, 'Ann'), (2, 'Bo'), (3, 'Cy');
                INSERT INTO posts VALUES (10, 1, 'First'), (11, 1, 'Second'), (12, 2, 'Third');
                INSERT INTO comments VALUES (100, 11, 'Nice'), (101, 11, 'Agreed');",
            )
            .unwrap();
        engine
    }

    #[test]
    fn lateral_joins_gather_related_rows_as_drizzle_writes_them() {
        let mut engine = nested_engine();
        // Drizzle's `with: {posts: true}`.
        assert_eq!(
            query_values(
                &engine,
                r#"select "users"."id", "users"."name", "users_posts"."data" as "posts"
                from "users" "users" left join lateral (select coalesce(json_agg(json_build_array(
                "users_posts"."id", "users_posts"."user_id", "users_posts"."title")), '[]'::json)
                as "data" from "posts" "users_posts" where "users_posts"."user_id" = "users"."id")
                "users_posts" on true"#
            ),
            [
                json!({"id": 1, "name": "Ann", "posts": [[10, 1, "First"], [11, 1, "Second"]]}),
                json!({"id": 2, "name": "Bo", "posts": [[12, 2, "Third"]]}),
                json!({"id": 3, "name": "Cy", "posts": []}),
            ]
        );

        // A relation to one row, which is NULL where there is none, from a query with no other
        // outputs.
        let one = r#"select "posts_user"."data" as "user" from "posts" "posts" left join lateral
            (select json_build_array("posts_user"."id", "posts_user"."name") as "data" from
            (select * from "users" "posts_user" where "posts_user"."id" = "posts"."user_id"
            limit $1) "posts_user") "posts_user" on true"#;
        engine
            .exec_sql("INSERT INTO posts VALUES (13, NULL, 'Orphan')")
            .unwrap();
        let rows = engine.query_sql(one, &[json!(1)]).unwrap().rows;
        assert_eq!(
            rows.into_iter().map(Value::Object).collect::<Vec<_>>(),
            [
                json!({"user": [1, "Ann"]}),
                json!({"user": [1, "Ann"]}),
                json!({"user": [2, "Bo"]}),
                json!({"user": null}),
            ]
        );

        // Ordered and limited relations, nested in each other, under an ordered and limited query.
        let nested = r#"select "users"."id", "users_posts"."data" as "posts" from "users" "users"
            left join lateral (select coalesce(json_agg(json_build_array("users_posts"."title",
            "users_posts_comments"."data") order by "users_posts"."id" desc), '[]'::json) as "data"
            from (select * from "posts" "users_posts" where ("users_posts"."user_id" = "users"."id"
            and "users_posts"."id" > $1) order by "users_posts"."id" desc limit $2) "users_posts"
            left join lateral (select coalesce(json_agg(json_build_array(
            "users_posts_comments"."body") order by "users_posts_comments"."id" desc nulls last),
            '[]'::json) as "data" from "comments" "users_posts_comments" where
            "users_posts_comments"."post_id" = "users_posts"."id") "users_posts_comments" on true)
            "users_posts" on true order by "users"."id" limit $3 offset $4"#;
        let rows = engine
            .execute_sql_rows(nested, &[json!(0), json!(1), json!(2), json!(0)], true)
            .unwrap();
        assert_eq!(
            rows.fields
                .iter()
                .map(|field| (field.name.as_str(), field.data_type_id))
                .collect::<Vec<_>>(),
            [
                ("id", ColumnType::Integer.postgres_oid()),
                ("posts", ColumnType::Json.postgres_oid())
            ]
        );
        assert_eq!(
            rows.rows.into_iter().map(Value::Object).collect::<Vec<_>>(),
            [
                json!({"id": 1, "posts": [["Second", [["Agreed"], ["Nice"]]]]}),
                json!({"id": 2, "posts": [["Third", []]]}),
            ]
        );
    }

    #[test]
    fn subqueries_in_a_select_list_read_its_rows_as_kysely_writes_them() {
        let mut engine = nested_engine();
        // Kysely's `jsonArrayFrom`.
        assert_eq!(
            query_values(
                &engine,
                r#"select "id", (select coalesce(json_agg(agg), '[]') from (select "posts"."id",
                "posts"."title" from "posts" where "posts"."user_id" = "users"."id" order by
                "posts"."id" desc) as agg) as "posts" from "users" where "id" < 3"#
            ),
            [
                json!({"id": 1, "posts": [{"id": 11, "title": "Second"}, {"id": 10, "title": "First"}]}),
                json!({"id": 2, "posts": [{"id": 12, "title": "Third"}]}),
            ]
        );
        // Kysely's `jsonObjectFrom`, with a value each row reads.
        assert_eq!(
            query_values(
                &engine,
                r#"select "id", (select to_json(obj) from (select "users"."name" from "users"
                where "users"."id" = "posts"."user_id") as obj) as "user", (select count(*) from
                comments where comments.post_id = posts.id) as "comments" from "posts" order by
                "id" desc limit 2"#
            ),
            [
                json!({"id": 12, "user": {"name": "Bo"}, "comments": 0}),
                json!({"id": 11, "user": {"name": "Ann"}, "comments": 2}),
            ]
        );

        // A query that joins tables can nest one that reads any of them.
        assert_eq!(
            query_values(
                &engine,
                "SELECT p.id, u.name, (SELECT count(*) FROM comments c WHERE c.post_id = p.id \
                 AND c.body <> u.name) AS n FROM posts p JOIN users u ON u.id = p.user_id \
                 WHERE u.id = 1 ORDER BY p.id DESC"
            ),
            [
                json!({"id": 11, "name": "Ann", "n": 2}),
                json!({"id": 10, "name": "Ann", "n": 0}),
            ]
        );

        // A prepared statement binds its values each time it runs.
        let prepared = engine
            .prepare_sql(
                r#"select "name", (select coalesce(json_agg(p.title), '[]')
                from posts p where p.user_id = users.id and p.id > $1) as titles from users
                where id = $2"#,
            )
            .unwrap();
        for (after, titles) in [(0, json!(["First", "Second"])), (10, json!(["Second"]))] {
            let rows = engine
                .execute_prepared(prepared, &[json!(after), json!(1)])
                .unwrap()
                .rows;
            assert_eq!(rows, [row(json!({"name": "Ann", "titles": titles}))]);
        }

        // A subquery giving more than one value for a row fails, as do what is not gathered.
        for (sql, code) in [
            (
                "SELECT id, (SELECT title FROM posts WHERE posts.user_id = users.id) FROM users",
                "INVALID_QUERY",
            ),
            (
                "SELECT id, (SELECT id, title FROM posts WHERE posts.id = users.id) FROM users",
                "INVALID_QUERY",
            ),
            (
                "SELECT id, (SELECT json_agg(posts.title || '!') FROM posts) AS t FROM users",
                "UNSUPPORTED_SQL",
            ),
            (
                "SELECT u.id, l.n FROM users u CROSS JOIN LATERAL (SELECT 1 AS n FROM posts) l",
                "UNSUPPORTED_SQL",
            ),
            (
                "SELECT id, (SELECT json_agg(p.nope) FROM posts p WHERE p.user_id = users.id) \
                 AS t FROM users",
                "COLUMN_NOT_FOUND",
            ),
            (
                "SELECT id, (SELECT count(*) FROM posts p WHERE p.user_id = users.nope) AS t \
                 FROM users",
                "COLUMN_NOT_FOUND",
            ),
            (
                "SELECT id FROM users WHERE id IN (SELECT (SELECT 1 FROM posts) FROM posts)",
                "UNSUPPORTED_SQL",
            ),
        ] {
            assert_eq!(engine.query_sql(sql, &[]).unwrap_err().code, code, "{sql}");
        }
    }
}
