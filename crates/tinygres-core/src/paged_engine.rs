use serde_json::Value;

use crate::{
    EngineError, ExecuteResult, InMemoryStorage, PageDevice, PagedStorage, QueryPlan, QueryResult,
    Result, StorageReader,
    statement::{PlannedDml, Statement},
};

/// A SQL engine which publishes row mutations directly through the crash-safe page store.
///
/// This first page-native engine slice supports reads plus standalone `INSERT`, `UPDATE`, and
/// `DELETE` statements. Schemas must currently be imported with [`Self::from_in_memory`] (or by
/// constructing a [`PagedStorage`] directly); page-native DDL and explicit transactions are not
/// part of this surface yet.
pub struct PagedEngine<D: PageDevice> {
    storage: PagedStorage<D>,
}

impl<D: PageDevice> PagedEngine<D> {
    pub fn new(storage: PagedStorage<D>) -> Self {
        Self { storage }
    }

    /// Opens a previously published paged database.
    pub fn open(device: D) -> Result<Self> {
        PagedStorage::open(device).map(Self::new)
    }

    /// Imports an in-memory catalog and its rows as one paged generation.
    pub fn from_in_memory(device: D, source: &InMemoryStorage) -> Result<Self> {
        PagedStorage::from_in_memory(device, source).map(Self::new)
    }

    pub fn query(&self, plan: &QueryPlan) -> Result<QueryResult> {
        crate::query::execute(&self.storage, plan)
    }

    pub fn query_sql(&self, sql: &str, params: &[Value]) -> Result<QueryResult> {
        match crate::statement::parse(sql, params)? {
            Statement::Select(plan) => self.query(&plan),
            Statement::Aggregate(plan) => crate::aggregate::execute(&self.storage, &plan),
            Statement::Join(plan) => crate::join::execute(&self.storage, &plan),
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
        match crate::statement::parse(sql, params)? {
            Statement::Select(plan) => {
                execute_query_result(crate::query::execute(&self.storage, &plan)?)
            }
            Statement::Aggregate(plan) => {
                execute_query_result(crate::aggregate::execute(&self.storage, &plan)?)
            }
            Statement::Join(plan) => {
                execute_query_result(crate::join::execute(&self.storage, &plan)?)
            }
            Statement::Write(statement) => {
                let PlannedDml { outcome, changes } =
                    crate::statement::plan_dml(&self.storage, &statement)?;
                debug_assert_eq!(outcome.mutated, !changes.is_empty());
                let revision = if outcome.mutated {
                    let write_set = self.storage.prepare_row_write_set(&changes)?;
                    self.storage.commit_row_write_set(write_set)?.revision
                } else {
                    self.storage.revision()
                };
                Ok(ExecuteResult {
                    command: outcome.command.to_owned(),
                    revision,
                    row_count: outcome.row_count,
                    rows: outcome.rows,
                    tables: outcome.tables,
                })
            }
        }
    }

    pub fn revision(&self) -> u64 {
        self.storage.revision()
    }

    pub fn into_storage(self) -> PagedStorage<D> {
        self.storage
    }

    pub fn into_device(self) -> D {
        self.storage.into_device()
    }
}

fn execute_query_result(result: QueryResult) -> Result<ExecuteResult> {
    Ok(ExecuteResult {
        command: "SELECT".to_owned(),
        revision: result.revision,
        row_count: result.rows.len(),
        rows: result.rows,
        tables: vec![],
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::{Engine, MemoryPageDevice, Row};

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

    #[test]
    fn page_native_dml_matches_in_memory_and_reopens() {
        let source = source();
        let mut expected = Engine::new(source.clone());
        let mut actual =
            PagedEngine::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();

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
        let source = source();
        let mut engine =
            PagedEngine::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
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

        let error = engine
            .execute_sql("CREATE TABLE later (id INTEGER PRIMARY KEY)", &[])
            .unwrap_err();
        assert_eq!(error.code, "UNSUPPORTED_SQL");
        assert!(error.message.contains("import the schema"));
        assert_eq!(engine.revision(), revision);

        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        assert_eq!(reopened.revision(), revision);
        assert_eq!(
            reopened
                .query_sql("SELECT * FROM accounts ORDER BY id", &[])
                .unwrap(),
            rows
        );
    }

    #[test]
    fn query_sql_rejects_mutations_without_publishing() {
        let source = source();
        let engine =
            PagedEngine::from_in_memory(MemoryPageDevice::new(0).unwrap(), &source).unwrap();
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
}
