use std::collections::BTreeSet;

use serde_json::Value;

use crate::storage::StorageReader;
use crate::{
    ApplyOutcome, ChangeBatch, EngineError, ExecuteResult, InMemoryStorage, PreparedCommit,
    QueryPlan, QueryResult, Result, Row, StorageDriver, TableSchema,
};

#[derive(Clone, Debug)]
pub struct Engine<S = InMemoryStorage> {
    storage: S,
    transaction: Option<Transaction<S>>,
    prepared: Option<PendingCommit<S>>,
}

#[derive(Clone, Debug)]
struct Transaction<S> {
    storage: S,
    tables: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct PendingCommit<S> {
    commit: PreparedCommit,
    storage: S,
}

#[derive(Clone, Debug)]
pub struct Prepared<T> {
    pub result: T,
    pub commit: Option<PreparedCommit>,
}

impl Default for Engine<InMemoryStorage> {
    fn default() -> Self {
        Self::new(InMemoryStorage::default())
    }
}

impl Engine<InMemoryStorage> {
    /// Encodes the complete engine state into a versioned, self-checking snapshot.
    pub fn export_snapshot(&self) -> Result<Vec<u8>> {
        self.storage.export_snapshot()
    }

    /// Atomically replaces the complete engine state from a validated snapshot.
    pub fn import_snapshot(&mut self, bytes: &[u8]) -> Result<()> {
        self.ensure_no_prepared()?;
        self.ensure_no_transaction()?;
        self.storage.import_snapshot(bytes)
    }

    pub fn prepare_define_table(&mut self, schema: TableSchema) -> Result<Prepared<()>> {
        self.prepare_define_tables(vec![schema])
    }

    pub fn prepare_define_tables(&mut self, schemas: Vec<TableSchema>) -> Result<Prepared<()>> {
        self.ensure_can_prepare()?;
        let mut candidate = self.storage.clone();
        let existing = self.storage.table_names().collect::<BTreeSet<_>>();
        let tables = schemas
            .iter()
            .filter(|schema| !existing.contains(schema.name.as_str()))
            .map(|schema| schema.name.clone())
            .collect::<Vec<_>>();
        for schema in schemas {
            candidate.define_table(schema)?;
        }
        let commit = self.stage_candidate(candidate, &tables)?;
        Ok(Prepared { result: (), commit })
    }

    pub fn prepare_replace_table_snapshot(
        &mut self,
        schema: TableSchema,
        rows: Vec<Row>,
    ) -> Result<Prepared<ApplyOutcome>> {
        self.ensure_can_prepare()?;
        let mut candidate = self.storage.clone();
        let result = candidate.replace_table_snapshot(schema, rows)?;
        let commit = self.stage_candidate(candidate, &result.tables)?;
        Ok(Prepared { result, commit })
    }

    pub fn prepare_apply_batch(&mut self, batch: &ChangeBatch) -> Result<Prepared<ApplyOutcome>> {
        self.ensure_can_prepare()?;
        let mut candidate = self.storage.clone();
        let result = candidate.apply_batch(batch)?;
        let commit = self.stage_candidate(candidate, &result.tables)?;
        Ok(Prepared { result, commit })
    }

    pub fn install_prepared_commit(&mut self, bytes: &[u8]) -> Result<ApplyOutcome> {
        let pending = self.prepared.as_ref().ok_or_else(|| {
            EngineError::new("NO_PREPARED_COMMIT", "No prepared commit is pending")
        })?;
        if pending.commit.bytes() != bytes {
            return Err(EngineError::new(
                "PREPARED_COMMIT_MISMATCH",
                "The installed bytes do not match the pending prepared commit",
            ));
        }
        if self.storage.revision() != pending.commit.revision_before() {
            return Err(EngineError::new(
                "PREPARED_COMMIT_REVISION_MISMATCH",
                "The database revision changed after the commit was prepared",
            ));
        }
        let pending = self
            .prepared
            .take()
            .expect("the pending commit was resolved above");
        let outcome = ApplyOutcome {
            revision: pending.commit.revision_after(),
            tables: pending.commit.tables().to_vec(),
        };
        self.storage = pending.storage;
        Ok(outcome)
    }

    pub fn abort_prepared_commit(&mut self) -> Result<()> {
        self.prepared = None;
        Ok(())
    }

    pub fn replay_commit(&mut self, bytes: &[u8]) -> Result<ApplyOutcome> {
        self.ensure_can_prepare()?;
        let (commit, payload) = PreparedCommit::decode(bytes)?;
        let candidate = crate::prepared::apply_payload(&self.storage, &payload)?;
        let canonical = PreparedCommit::from_states(&self.storage, &candidate, commit.tables())?
            .ok_or_else(|| EngineError::new("INVALID_PREPARED_COMMIT", "Commit has no effect"))?;
        if canonical.bytes() != bytes {
            return Err(EngineError::new(
                "INVALID_PREPARED_COMMIT",
                "Prepared commit operations are not canonical for this database state",
            ));
        }
        let outcome = ApplyOutcome {
            revision: commit.revision_after(),
            tables: commit.tables().to_vec(),
        };
        self.storage = candidate;
        Ok(outcome)
    }

    pub fn prepare_execute_sql(
        &mut self,
        sql: &str,
        params: &[Value],
    ) -> Result<Prepared<ExecuteResult>> {
        self.ensure_no_prepared()?;
        if self.in_transaction() {
            return Err(EngineError::transaction_active());
        }
        let mut candidate = Engine::new(self.storage.clone());
        let result = candidate.execute_sql(sql, params)?;
        let commit = self.stage_candidate(candidate.storage, &result.tables)?;
        Ok(Prepared { result, commit })
    }

    pub fn prepare_commit_transaction(&mut self) -> Result<Prepared<ApplyOutcome>> {
        self.ensure_no_prepared()?;
        let mut candidate = self
            .transaction
            .as_ref()
            .cloned()
            .ok_or_else(EngineError::no_active_transaction)?;
        if candidate.tables.is_empty() {
            self.transaction = None;
            return Ok(Prepared {
                result: ApplyOutcome {
                    revision: self.storage.revision(),
                    tables: vec![],
                },
                commit: None,
            });
        }
        let revision = candidate.storage.advance_revision()?;
        let tables = candidate.tables.into_iter().collect::<Vec<_>>();
        let commit = self.stage_candidate(candidate.storage, &tables)?;
        self.transaction = None;
        Ok(Prepared {
            result: ApplyOutcome { revision, tables },
            commit,
        })
    }

    fn stage_candidate(
        &mut self,
        candidate: InMemoryStorage,
        tables: &[String],
    ) -> Result<Option<PreparedCommit>> {
        let Some(commit) = PreparedCommit::from_states(&self.storage, &candidate, tables)? else {
            return Ok(None);
        };
        self.prepared = Some(PendingCommit {
            commit: commit.clone(),
            storage: candidate,
        });
        Ok(Some(commit))
    }
}

impl<S> Engine<S> {
    pub fn new(storage: S) -> Self {
        Self {
            storage,
            transaction: None,
            prepared: None,
        }
    }
}

impl<S: StorageDriver> Engine<S> {
    pub fn define_table(&mut self, schema: TableSchema) -> Result<()> {
        self.ensure_no_prepared()?;
        self.ensure_no_transaction()?;
        self.storage.define_table(schema)
    }

    pub fn replace_table_snapshot(
        &mut self,
        schema: TableSchema,
        rows: Vec<Row>,
    ) -> Result<ApplyOutcome> {
        self.ensure_no_prepared()?;
        self.ensure_no_transaction()?;
        self.storage.replace_table_snapshot(schema, rows)
    }

    pub fn replace_table(&mut self, table: &str, rows: Vec<Row>) -> Result<ApplyOutcome> {
        self.ensure_no_prepared()?;
        self.ensure_no_transaction()?;
        self.storage.replace_table(table, rows)
    }

    pub fn apply_batch(&mut self, batch: &ChangeBatch) -> Result<ApplyOutcome> {
        self.ensure_no_prepared()?;
        self.ensure_no_transaction()?;
        self.storage.apply_batch(batch)
    }
}

impl<S: StorageReader> Engine<S> {
    pub fn query(&self, plan: &QueryPlan) -> Result<QueryResult> {
        crate::query::execute(self.read_storage(), plan)
    }

    pub fn query_sql(&self, sql: &str, params: &[Value]) -> Result<QueryResult> {
        match crate::statement::parse(sql, params)? {
            crate::statement::Statement::Select(plan) => self.query(&plan),
            crate::statement::Statement::Aggregate(plan) => {
                crate::aggregate::execute(self.read_storage(), &plan)
            }
            crate::statement::Statement::Join(plan) => {
                crate::join::execute(self.read_storage(), &plan)
            }
            crate::statement::Statement::Write(_) => Err(EngineError::unsupported_sql(
                "query_sql accepts only SELECT statements",
            )),
        }
    }

    pub fn revision(&self) -> u64 {
        self.storage.revision()
    }
}

impl<S> Engine<S> {
    pub fn in_transaction(&self) -> bool {
        self.transaction.is_some()
    }

    pub fn rollback_transaction(&mut self) -> Result<()> {
        self.ensure_no_prepared()?;
        if self.transaction.take().is_none() {
            return Err(EngineError::no_active_transaction());
        }
        Ok(())
    }

    pub fn into_storage(self) -> S {
        self.storage
    }

    fn read_storage(&self) -> &S {
        self.transaction
            .as_ref()
            .map_or(&self.storage, |transaction| &transaction.storage)
    }

    fn ensure_no_transaction(&self) -> Result<()> {
        if self.in_transaction() {
            Err(EngineError::transaction_active())
        } else {
            Ok(())
        }
    }

    fn ensure_no_prepared(&self) -> Result<()> {
        if self.prepared.is_some() {
            Err(EngineError::new(
                "PREPARED_COMMIT_PENDING",
                "A prepared commit is already pending",
            ))
        } else {
            Ok(())
        }
    }

    fn ensure_can_prepare(&self) -> Result<()> {
        self.ensure_no_prepared()?;
        self.ensure_no_transaction()
    }
}

impl<S: StorageDriver + Clone> Engine<S> {
    pub fn begin_transaction(&mut self) -> Result<()> {
        self.ensure_no_prepared()?;
        if self.in_transaction() {
            return Err(EngineError::transaction_active());
        }
        self.transaction = Some(Transaction {
            storage: self.storage.clone(),
            tables: BTreeSet::new(),
        });
        Ok(())
    }

    pub fn commit_transaction(&mut self) -> Result<ApplyOutcome> {
        self.ensure_no_prepared()?;
        let transaction = self
            .transaction
            .as_mut()
            .ok_or_else(EngineError::no_active_transaction)?;
        if transaction.tables.is_empty() {
            self.transaction = None;
            return Ok(ApplyOutcome {
                revision: self.storage.revision(),
                tables: vec![],
            });
        }

        let revision = transaction.storage.advance_revision()?;
        let transaction = self
            .transaction
            .take()
            .expect("the active transaction was checked above");
        let tables = transaction.tables.into_iter().collect();
        self.storage = transaction.storage;
        Ok(ApplyOutcome { revision, tables })
    }

    pub fn execute_sql(&mut self, sql: &str, params: &[Value]) -> Result<ExecuteResult> {
        self.ensure_no_prepared()?;
        match crate::statement::parse(sql, params)? {
            crate::statement::Statement::Select(plan) => {
                let result = self.query(&plan)?;
                Ok(ExecuteResult {
                    command: "SELECT".to_owned(),
                    revision: result.revision,
                    row_count: result.rows.len(),
                    rows: result.rows,
                    tables: vec![],
                })
            }
            crate::statement::Statement::Aggregate(plan) => {
                let result = crate::aggregate::execute(self.read_storage(), &plan)?;
                Ok(ExecuteResult {
                    command: "SELECT".to_owned(),
                    revision: result.revision,
                    row_count: result.rows.len(),
                    rows: result.rows,
                    tables: vec![],
                })
            }
            crate::statement::Statement::Join(plan) => {
                let result = crate::join::execute(self.read_storage(), &plan)?;
                Ok(ExecuteResult {
                    command: "SELECT".to_owned(),
                    revision: result.revision,
                    row_count: result.rows.len(),
                    rows: result.rows,
                    tables: vec![],
                })
            }
            crate::statement::Statement::Write(statement) => {
                if let Some(transaction) = &mut self.transaction {
                    let mut candidate = transaction.storage.clone();
                    let outcome = crate::statement::execute(&mut candidate, &statement)?;
                    if outcome.mutated {
                        transaction.storage = candidate;
                        transaction.tables.extend(outcome.tables.iter().cloned());
                    }
                    Ok(ExecuteResult {
                        command: outcome.command.to_owned(),
                        revision: self.storage.revision(),
                        row_count: outcome.row_count,
                        rows: outcome.rows,
                        tables: outcome.tables,
                    })
                } else {
                    let mut candidate = self.storage.clone();
                    let outcome = crate::statement::execute(&mut candidate, &statement)?;
                    let revision = if outcome.mutated {
                        candidate.advance_revision()?
                    } else {
                        candidate.revision()
                    };
                    self.storage = candidate;
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
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::Change;

    fn row(value: Value) -> Row {
        value
            .as_object()
            .expect("test row must be an object")
            .clone()
    }

    fn create_posts(engine: &mut Engine) {
        let result = engine
            .execute_sql(
                "CREATE TABLE posts (\
                    id INTEGER PRIMARY KEY, \
                    title TEXT NOT NULL, \
                    published BOOLEAN NOT NULL DEFAULT false, \
                    rating DOUBLE PRECISION, \
                    metadata JSONB\
                )",
                &[],
            )
            .unwrap();
        assert_eq!(result.command, "CREATE TABLE");
        assert_eq!(result.revision, 1);
        assert_eq!(result.tables, vec!["posts"]);
    }

    #[test]
    fn creates_a_typed_table_and_inserts_defaults_and_json() {
        let mut engine = Engine::default();
        create_posts(&mut engine);

        let result = engine
            .execute_sql(
                "INSERT INTO posts (id, title, metadata) \
                 VALUES ($1, $2, $3), ($4, $5, DEFAULT) \
                 RETURNING id, title, published, rating, metadata",
                &[
                    json!(1),
                    json!("one"),
                    json!({"tags": ["small", "local"]}),
                    json!(2),
                    json!("two"),
                ],
            )
            .unwrap();

        assert_eq!(result.command, "INSERT");
        assert_eq!(result.revision, 2);
        assert_eq!(result.row_count, 2);
        assert_eq!(result.tables, vec!["posts"]);
        assert_eq!(
            result.rows,
            vec![
                row(json!({
                    "id": 1,
                    "title": "one",
                    "published": false,
                    "rating": null,
                    "metadata": {"tags": ["small", "local"]}
                })),
                row(json!({
                    "id": 2,
                    "title": "two",
                    "published": false,
                    "rating": null,
                    "metadata": null
                })),
            ]
        );
        assert_eq!(
            engine
                .query_sql("SELECT id, title FROM posts WHERE published = false", &[])
                .unwrap()
                .rows,
            vec![
                row(json!({"id": 1, "title": "one"})),
                row(json!({"id": 2, "title": "two"})),
            ]
        );
    }

    #[test]
    fn update_and_delete_support_returning_and_postgres_null_equality() {
        let mut engine = Engine::default();
        create_posts(&mut engine);
        engine
            .execute_sql(
                "INSERT INTO posts (id, title) VALUES (1, 'one'), (2, 'two')",
                &[],
            )
            .unwrap();

        let update = engine
            .execute_sql(
                "UPDATE posts SET published = true, rating = $1 \
                 WHERE id = $2 RETURNING *",
                &[json!(4.5), json!(1)],
            )
            .unwrap();
        assert_eq!(update.row_count, 1);
        assert_eq!(update.rows[0]["published"], json!(true));
        assert_eq!(update.rows[0]["rating"], json!(4.5));

        let null_comparison = engine
            .execute_sql("DELETE FROM posts WHERE rating = NULL RETURNING id", &[])
            .unwrap();
        assert_eq!(null_comparison.row_count, 0);
        assert!(null_comparison.rows.is_empty());
        assert!(null_comparison.tables.is_empty());
        assert_eq!(null_comparison.revision, 3);

        let delete = engine
            .execute_sql("DELETE FROM posts WHERE id = 2 RETURNING id, title", &[])
            .unwrap();
        assert_eq!(delete.row_count, 1);
        assert_eq!(delete.rows, vec![row(json!({"id": 2, "title": "two"}))]);
        assert_eq!(
            engine.query_sql("SELECT id FROM posts", &[]).unwrap().rows,
            vec![row(json!({"id": 1}))]
        );
    }

    #[test]
    fn typed_integers_stay_lossless_across_the_javascript_boundary() {
        let mut engine = Engine::default();
        create_posts(&mut engine);
        for value in [
            json!(9_007_199_254_740_992_u64),
            json!(-9_007_199_254_740_992_i64),
        ] {
            let error = engine
                .execute_sql(
                    "INSERT INTO posts (id, title) VALUES ($1, 'unsafe')",
                    &[value],
                )
                .unwrap_err();
            assert_eq!(error.code, "TYPE_MISMATCH");
        }
        assert_eq!(engine.revision(), 1);
    }

    #[test]
    fn every_failed_statement_is_atomic() {
        let mut engine = Engine::default();
        create_posts(&mut engine);
        engine
            .execute_sql("INSERT INTO posts (id, title) VALUES (1, 'kept')", &[])
            .unwrap();
        let revision = engine.revision();

        let duplicate = engine
            .execute_sql(
                "INSERT INTO posts (id, title) VALUES (2, 'new'), (1, 'duplicate')",
                &[],
            )
            .unwrap_err();
        assert_eq!(duplicate.code, "CONSTRAINT_VIOLATION");

        let wrong_type = engine
            .execute_sql("UPDATE posts SET published = 'yes'", &[])
            .unwrap_err();
        assert_eq!(wrong_type.code, "TYPE_MISMATCH");

        let missing_required = engine
            .execute_sql("INSERT INTO posts (id) VALUES (3)", &[])
            .unwrap_err();
        assert_eq!(missing_required.code, "CONSTRAINT_VIOLATION");

        assert_eq!(engine.revision(), revision);
        assert_eq!(
            engine
                .query_sql("SELECT id, title FROM posts", &[])
                .unwrap()
                .rows,
            vec![row(json!({"id": 1, "title": "kept"}))]
        );
    }

    #[test]
    fn primary_key_updates_detect_collisions_without_partial_changes() {
        let mut engine = Engine::default();
        create_posts(&mut engine);
        engine
            .execute_sql(
                "INSERT INTO posts (id, title) VALUES (1, 'one'), (2, 'two')",
                &[],
            )
            .unwrap();
        let revision = engine.revision();

        let error = engine
            .execute_sql("UPDATE posts SET id = 1 WHERE id = 2", &[])
            .unwrap_err();
        assert_eq!(error.code, "CONSTRAINT_VIOLATION");
        assert_eq!(engine.revision(), revision);
        assert_eq!(
            engine
                .query_sql("SELECT id, title FROM posts", &[])
                .unwrap()
                .rows,
            vec![
                row(json!({"id": 1, "title": "one"})),
                row(json!({"id": 2, "title": "two"})),
            ]
        );
    }

    #[test]
    fn writable_predicates_share_select_null_and_boolean_semantics() {
        let mut engine = Engine::default();
        create_posts(&mut engine);
        engine
            .execute_sql(
                "INSERT INTO posts (id, title, rating) VALUES \
                 (1, 'one', NULL), (2, 'two', 2), (3, 'three', 3)",
                &[],
            )
            .unwrap();

        let updated = engine
            .execute_sql(
                "UPDATE posts SET published = true \
                 WHERE id >= 2 AND (rating < 3 OR title = 'three') RETURNING id",
                &[],
            )
            .unwrap();
        assert_eq!(
            updated.rows,
            vec![row(json!({"id": 2})), row(json!({"id": 3}))]
        );

        let deleted = engine
            .execute_sql(
                "DELETE FROM posts WHERE rating IS NULL OR id IN (3) RETURNING id",
                &[],
            )
            .unwrap();
        assert_eq!(
            deleted.rows,
            vec![row(json!({"id": 1})), row(json!({"id": 3}))]
        );
        assert_eq!(
            engine.query_sql("SELECT id FROM posts", &[]).unwrap().rows,
            vec![row(json!({"id": 2}))]
        );
    }

    #[test]
    fn explicit_transactions_publish_once_and_snapshots_stay_committed() {
        let mut engine = Engine::default();
        create_posts(&mut engine);
        let committed_snapshot = engine.export_snapshot().unwrap();

        engine.begin_transaction().unwrap();
        let insert = engine
            .execute_sql("INSERT INTO posts (id, title) VALUES (1, 'staged')", &[])
            .unwrap();
        assert_eq!(insert.revision, 1);
        assert_eq!(engine.revision(), 1);
        assert_eq!(
            engine
                .query_sql("SELECT title FROM posts", &[])
                .unwrap()
                .rows,
            vec![row(json!({"title": "staged"}))]
        );

        // Persistence must not observe an uncommitted transaction.
        assert_eq!(engine.export_snapshot().unwrap(), committed_snapshot);
        let mut restored = Engine::default();
        restored
            .import_snapshot(&engine.export_snapshot().unwrap())
            .unwrap();
        assert!(
            restored
                .query_sql("SELECT * FROM posts", &[])
                .unwrap()
                .rows
                .is_empty()
        );

        engine
            .execute_sql(
                "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL)",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO notes (id, body) VALUES (1, 'also staged')",
                &[],
            )
            .unwrap();
        assert_eq!(
            engine.replace_table("posts", vec![]).unwrap_err().code,
            "TRANSACTION_ACTIVE"
        );

        let committed = engine.commit_transaction().unwrap();
        assert_eq!(committed.revision, 2);
        assert_eq!(committed.tables, vec!["notes", "posts"]);
        assert_eq!(engine.revision(), 2);
        assert_eq!(
            engine
                .query_sql("SELECT body FROM notes", &[])
                .unwrap()
                .rows,
            vec![row(json!({"body": "also staged"}))]
        );
    }

    #[test]
    fn rollback_discards_staged_work_and_empty_commit_does_not_advance() {
        let mut engine = Engine::default();
        create_posts(&mut engine);

        engine.begin_transaction().unwrap();
        engine
            .execute_sql("INSERT INTO posts (id, title) VALUES (1, 'discarded')", &[])
            .unwrap();
        engine.rollback_transaction().unwrap();
        assert!(
            engine
                .query_sql("SELECT * FROM posts", &[])
                .unwrap()
                .rows
                .is_empty()
        );
        assert_eq!(engine.revision(), 1);

        engine.begin_transaction().unwrap();
        let outcome = engine.commit_transaction().unwrap();
        assert_eq!(outcome.revision, 1);
        assert!(outcome.tables.is_empty());
        assert_eq!(engine.revision(), 1);
        assert_eq!(
            engine.rollback_transaction().unwrap_err().code,
            "NO_ACTIVE_TRANSACTION"
        );
    }

    #[test]
    fn a_failed_statement_preserves_prior_work_inside_a_transaction() {
        let mut engine = Engine::default();
        create_posts(&mut engine);
        engine.begin_transaction().unwrap();
        engine
            .execute_sql("INSERT INTO posts (id, title) VALUES (1, 'kept')", &[])
            .unwrap();
        assert_eq!(
            engine
                .execute_sql("INSERT INTO posts (id, title) VALUES (1, 'duplicate')", &[])
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        engine
            .execute_sql("UPDATE posts SET title = 'continued' WHERE id = 1", &[])
            .unwrap();
        engine.commit_transaction().unwrap();

        assert_eq!(
            engine.query_sql("SELECT * FROM posts", &[]).unwrap().rows[0]["title"],
            json!("continued")
        );
        assert_eq!(engine.revision(), 2);
    }

    #[test]
    fn typed_catalog_survives_snapshots_and_validates_empty_queries() {
        let mut engine = Engine::default();
        create_posts(&mut engine);
        let bytes = engine.export_snapshot().unwrap();
        let mut restored = Engine::default();
        restored.import_snapshot(&bytes).unwrap();

        assert_eq!(
            restored
                .query_sql("SELECT missing FROM posts", &[])
                .unwrap_err()
                .code,
            "COLUMN_NOT_FOUND"
        );
        assert_eq!(
            restored
                .execute_sql("INSERT INTO posts (id, title) VALUES (1, 42)", &[])
                .unwrap_err()
                .code,
            "TYPE_MISMATCH"
        );
    }

    #[test]
    fn unique_indexes_are_atomic_and_treat_nulls_as_distinct() {
        let mut engine = Engine::default();
        engine
            .execute_sql(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, tenant TEXT, email TEXT, active BOOLEAN)",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO users (id, tenant, email, active) VALUES \
                 (1, 'one', 'a@example.com', true), \
                 (2, NULL, 'a@example.com', false), \
                 (3, NULL, 'a@example.com', true), \
                 (4, NULL, NULL, true), (5, NULL, NULL, false)",
                &[],
            )
            .unwrap();
        let created = engine
            .execute_sql(
                "CREATE UNIQUE INDEX users_tenant_email ON users (tenant, email)",
                &[],
            )
            .unwrap();
        assert_eq!(created.command, "CREATE INDEX");
        assert_eq!(created.revision, 3);

        let revision = engine.revision();
        assert_eq!(
            engine
                .execute_sql(
                    "INSERT INTO users (id, tenant, email) VALUES (6, 'one', 'a@example.com')",
                    &[],
                )
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        assert_eq!(
            engine
                .execute_sql("UPDATE users SET tenant = 'one' WHERE id = 2", &[])
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        assert_eq!(engine.revision(), revision);

        engine
            .execute_sql("DELETE FROM users WHERE id = 1", &[])
            .unwrap();
        engine
            .execute_sql("UPDATE users SET tenant = 'one' WHERE id = 2", &[])
            .unwrap();
        assert_eq!(
            engine
                .query_sql(
                    "SELECT id FROM users WHERE tenant = 'one' AND email = 'a@example.com'",
                    &[],
                )
                .unwrap()
                .rows,
            vec![row(json!({"id": 2}))]
        );

        let before_failed_ddl = engine.revision();
        assert_eq!(
            engine
                .execute_sql("CREATE UNIQUE INDEX users_active ON users (active)", &[])
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        assert_eq!(engine.revision(), before_failed_ddl);
    }

    #[test]
    fn schema_migrations_backfill_rows_and_preserve_indexes() {
        let mut engine = Engine::default();
        create_posts(&mut engine);
        engine
            .execute_sql(
                "INSERT INTO posts (id, title) VALUES (1, 'one'), (2, 'two')",
                &[],
            )
            .unwrap();
        engine
            .execute_sql("CREATE UNIQUE INDEX posts_title ON posts (title)", &[])
            .unwrap();

        let altered = engine
            .execute_sql(
                "ALTER TABLE posts ADD COLUMN priority INTEGER NOT NULL DEFAULT 3",
                &[],
            )
            .unwrap();
        assert_eq!(altered.command, "ALTER TABLE");
        assert_eq!(altered.tables, vec!["posts"]);
        assert_eq!(
            engine
                .query_sql("SELECT id, priority FROM posts ORDER BY id", &[])
                .unwrap()
                .rows,
            vec![
                row(json!({"id": 1, "priority": 3})),
                row(json!({"id": 2, "priority": 3})),
            ]
        );
        engine
            .execute_sql("ALTER TABLE posts ADD COLUMN note TEXT", &[])
            .unwrap();
        assert_eq!(
            engine
                .query_sql("SELECT note FROM posts WHERE id = 1", &[])
                .unwrap()
                .rows,
            vec![row(json!({"note": null}))]
        );
        engine
            .execute_sql("INSERT INTO posts (id, title) VALUES (3, 'three')", &[])
            .unwrap();
        assert_eq!(
            engine
                .execute_sql("INSERT INTO posts (id, title) VALUES (4, 'one')", &[])
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );

        let bytes = engine.export_snapshot().unwrap();
        let mut restored = Engine::default();
        restored.import_snapshot(&bytes).unwrap();
        assert_eq!(
            restored
                .query_sql("SELECT priority FROM posts WHERE title = 'three'", &[])
                .unwrap()
                .rows,
            vec![row(json!({"priority": 3}))]
        );
    }

    #[test]
    fn failed_and_redundant_column_migrations_are_atomic() {
        let mut engine = Engine::default();
        create_posts(&mut engine);
        engine
            .execute_sql("INSERT INTO posts (id, title) VALUES (1, 'one')", &[])
            .unwrap();
        let revision = engine.revision();

        assert_eq!(
            engine
                .execute_sql("ALTER TABLE posts ADD COLUMN required TEXT NOT NULL", &[],)
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        assert_eq!(engine.revision(), revision);
        assert_eq!(
            engine
                .query_sql("SELECT required FROM posts", &[])
                .unwrap_err()
                .code,
            "COLUMN_NOT_FOUND"
        );

        let unchanged = engine
            .execute_sql(
                "ALTER TABLE posts ADD COLUMN IF NOT EXISTS title BOOLEAN",
                &[],
            )
            .unwrap();
        assert!(unchanged.tables.is_empty());
        assert_eq!(unchanged.revision, revision);
        assert_eq!(
            engine
                .execute_sql("ALTER TABLE posts ADD COLUMN title BOOLEAN", &[])
                .unwrap_err()
                .code,
            "COLUMN_ALREADY_EXISTS"
        );
        assert_eq!(
            engine
                .execute_sql(
                    "ALTER TABLE posts ADD COLUMN second_id INTEGER PRIMARY KEY",
                    &[],
                )
                .unwrap_err()
                .code,
            "UNSUPPORTED_SQL"
        );
        assert_eq!(engine.revision(), revision);

        let mut legacy = Engine::default();
        legacy
            .define_table(TableSchema {
                name: "legacy".to_owned(),
                primary_key: vec!["id".to_owned()],
                columns: vec![],
            })
            .unwrap();
        assert_eq!(
            legacy
                .execute_sql("ALTER TABLE legacy ADD COLUMN value TEXT", &[])
                .unwrap_err()
                .code,
            "UNSUPPORTED_SQL"
        );

        let mut empty = Engine::default();
        empty
            .execute_sql("CREATE TABLE empty_table (id INTEGER PRIMARY KEY)", &[])
            .unwrap();
        empty
            .execute_sql(
                "ALTER TABLE empty_table ADD COLUMN required TEXT NOT NULL",
                &[],
            )
            .unwrap();
        assert_eq!(
            empty
                .execute_sql("INSERT INTO empty_table (id) VALUES (1)", &[])
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
    }

    #[test]
    fn tables_and_indexes_drop_atomically_and_transactionally() {
        let mut engine = Engine::default();
        create_posts(&mut engine);
        engine
            .execute_sql("CREATE UNIQUE INDEX posts_title ON posts (title)", &[])
            .unwrap();
        engine
            .execute_sql("INSERT INTO posts (id, title) VALUES (1, 'one')", &[])
            .unwrap();

        engine.begin_transaction().unwrap();
        engine.execute_sql("DROP TABLE posts", &[]).unwrap();
        assert_eq!(
            engine
                .query_sql("SELECT * FROM posts", &[])
                .unwrap_err()
                .code,
            "TABLE_NOT_FOUND"
        );
        engine.rollback_transaction().unwrap();
        assert_eq!(
            engine
                .query_sql("SELECT * FROM posts", &[])
                .unwrap()
                .rows
                .len(),
            1
        );

        let dropped = engine.execute_sql("DROP INDEX posts_title", &[]).unwrap();
        assert_eq!(dropped.command, "DROP INDEX");
        engine
            .execute_sql("INSERT INTO posts (id, title) VALUES (2, 'one')", &[])
            .unwrap();
        assert!(
            engine
                .execute_sql("DROP INDEX IF EXISTS posts_title", &[])
                .unwrap()
                .tables
                .is_empty()
        );

        engine
            .execute_sql("CREATE INDEX posts_title ON posts (title)", &[])
            .unwrap();
        engine.execute_sql("DROP TABLE posts", &[]).unwrap();
        assert!(
            engine
                .execute_sql("DROP TABLE IF EXISTS posts", &[])
                .unwrap()
                .tables
                .is_empty()
        );
        engine
            .execute_sql(
                "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                &[],
            )
            .unwrap();
        engine
            .execute_sql("CREATE INDEX posts_title ON notes (title)", &[])
            .unwrap();
    }

    #[test]
    fn index_ddl_validates_catalog_and_if_not_exists_is_a_noop() {
        let mut engine = Engine::default();
        create_posts(&mut engine);
        for sql in [
            "CREATE INDEX bad_float ON posts (rating)",
            "CREATE INDEX bad_json ON posts (metadata)",
            "CREATE INDEX bad_missing ON posts (missing)",
            "CREATE INDEX bad_duplicate ON posts (title, title)",
        ] {
            assert!(
                matches!(
                    engine.execute_sql(sql, &[]).unwrap_err().code.as_str(),
                    "UNSUPPORTED_SQL" | "COLUMN_NOT_FOUND" | "INVALID_SCHEMA"
                ),
                "statement was `{sql}`"
            );
        }
        assert_eq!(engine.revision(), 1);
        engine
            .execute_sql("CREATE INDEX posts_title ON posts (title)", &[])
            .unwrap();
        let unchanged = engine
            .execute_sql(
                "CREATE INDEX IF NOT EXISTS posts_title ON posts (published)",
                &[],
            )
            .unwrap();
        assert_eq!(unchanged.revision, 2);
        assert!(unchanged.tables.is_empty());
        assert_eq!(
            engine
                .execute_sql("CREATE INDEX posts_title ON posts (published)", &[])
                .unwrap_err()
                .code,
            "INDEX_ALREADY_EXISTS"
        );
    }

    #[test]
    fn indexes_survive_snapshots_and_transaction_rollback() {
        let mut engine = Engine::default();
        create_posts(&mut engine);
        engine
            .execute_sql("CREATE UNIQUE INDEX posts_title ON posts (title)", &[])
            .unwrap();
        engine
            .execute_sql("INSERT INTO posts (id, title) VALUES (1, 'one')", &[])
            .unwrap();

        engine.begin_transaction().unwrap();
        engine
            .execute_sql("CREATE INDEX posts_published ON posts (published)", &[])
            .unwrap();
        engine.rollback_transaction().unwrap();
        engine
            .execute_sql("CREATE INDEX posts_published ON posts (published)", &[])
            .unwrap();

        let bytes = engine.export_snapshot().unwrap();
        let mut restored = Engine::default();
        restored.import_snapshot(&bytes).unwrap();
        assert_eq!(restored.export_snapshot().unwrap(), bytes);
        assert_eq!(
            restored
                .execute_sql("INSERT INTO posts (id, title) VALUES (2, 'one')", &[])
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
    }

    #[test]
    fn source_changes_rebuild_indexes_atomically() {
        let mut engine = Engine::default();
        create_posts(&mut engine);
        engine
            .execute_sql("CREATE UNIQUE INDEX posts_title ON posts (title)", &[])
            .unwrap();
        engine
            .apply_batch(&ChangeBatch {
                changes: vec![Change::Upsert {
                    table: "posts".to_owned(),
                    row: row(json!({
                        "id": 1, "title": "kept", "published": false,
                        "rating": null, "metadata": null
                    })),
                }],
                ..ChangeBatch::default()
            })
            .unwrap();
        let revision = engine.revision();
        let error = engine
            .apply_batch(&ChangeBatch {
                changes: vec![
                    Change::Upsert {
                        table: "posts".to_owned(),
                        row: row(json!({
                            "id": 2, "title": "other", "published": false,
                            "rating": null, "metadata": null
                        })),
                    },
                    Change::Upsert {
                        table: "posts".to_owned(),
                        row: row(json!({
                            "id": 3, "title": "kept", "published": false,
                            "rating": null, "metadata": null
                        })),
                    },
                ],
                ..ChangeBatch::default()
            })
            .unwrap_err();
        assert_eq!(error.code, "CONSTRAINT_VIOLATION");
        assert_eq!(engine.revision(), revision);
        assert_eq!(
            engine.query_sql("SELECT id FROM posts", &[]).unwrap().rows,
            vec![row(json!({"id": 1}))]
        );

        assert_eq!(
            engine
                .replace_table(
                    "posts",
                    vec![
                        row(json!({
                            "id": 2, "title": "same", "published": false,
                            "rating": null, "metadata": null
                        })),
                        row(json!({
                            "id": 3, "title": "same", "published": false,
                            "rating": null, "metadata": null
                        })),
                    ],
                )
                .unwrap_err()
                .code,
            "CONSTRAINT_VIOLATION"
        );
        assert_eq!(engine.revision(), revision);
        assert_eq!(
            engine.query_sql("SELECT id FROM posts", &[]).unwrap().rows,
            vec![row(json!({"id": 1}))]
        );
    }

    #[test]
    fn supports_composite_primary_keys_type_aliases_and_if_not_exists() {
        let mut engine = Engine::default();
        engine
            .execute_sql(
                "CREATE TABLE memberships (\
                    team_id INT8, user_id INT4, role VARCHAR NOT NULL DEFAULT 'member', \
                    PRIMARY KEY (team_id, user_id)\
                )",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "INSERT INTO memberships (team_id, user_id) VALUES (1, 2) RETURNING *",
                &[],
            )
            .unwrap();

        let unchanged = engine
            .execute_sql(
                "CREATE TABLE IF NOT EXISTS memberships (id INTEGER PRIMARY KEY)",
                &[],
            )
            .unwrap();
        assert_eq!(unchanged.revision, 2);
        assert!(unchanged.tables.is_empty());
        assert_eq!(
            engine
                .execute_sql("CREATE TABLE memberships (id INTEGER PRIMARY KEY)", &[])
                .unwrap_err()
                .code,
            "TABLE_ALREADY_EXISTS"
        );
        assert_eq!(
            engine
                .query_sql(
                    "SELECT role FROM memberships WHERE team_id = 1 AND user_id = 2",
                    &[],
                )
                .unwrap()
                .rows,
            vec![row(json!({"role": "member"}))]
        );
    }

    #[test]
    fn rejects_unbounded_database_features_and_malformed_catalogs() {
        let mut engine = Engine::default();
        for sql in [
            "CREATE TABLE no_key (value TEXT)",
            "CREATE TABLE duplicate (id INTEGER, id TEXT, PRIMARY KEY (id))",
            "CREATE TABLE missing (id INTEGER, PRIMARY KEY (other))",
            "CREATE TABLE nullable_default (id INTEGER PRIMARY KEY DEFAULT NULL)",
        ] {
            assert_eq!(
                engine.execute_sql(sql, &[]).unwrap_err().code,
                "INVALID_SCHEMA",
                "statement was `{sql}`"
            );
        }
        for sql in [
            "CREATE TABLE generated (id SERIAL PRIMARY KEY)",
            "CREATE TABLE sized (id INTEGER PRIMARY KEY, name VARCHAR(100))",
            "INSERT INTO missing SELECT * FROM elsewhere",
            "UPDATE missing SET value = value + 1",
        ] {
            assert_eq!(
                engine.execute_sql(sql, &[]).unwrap_err().code,
                "UNSUPPORTED_SQL",
                "statement was `{sql}`"
            );
        }
        assert_eq!(engine.revision(), 0);
    }

    #[test]
    fn explicit_transactions_reject_invalid_lifecycle_transitions() {
        let mut engine = Engine::default();
        assert_eq!(
            engine.commit_transaction().unwrap_err().code,
            "NO_ACTIVE_TRANSACTION"
        );
        engine.begin_transaction().unwrap();
        assert_eq!(
            engine.begin_transaction().unwrap_err().code,
            "TRANSACTION_ACTIVE"
        );
        engine.rollback_transaction().unwrap();
    }

    #[test]
    fn prepared_sql_is_invisible_until_exact_install_and_abort_preserves_prior_state() {
        let mut engine = Engine::default();
        let prepared = engine
            .prepare_execute_sql(
                "CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT)",
                &[],
            )
            .unwrap();
        let create_bytes = prepared.commit.unwrap().into_bytes();
        assert_eq!(engine.revision(), 0);
        assert_eq!(
            engine
                .query_sql("SELECT id FROM posts", &[])
                .unwrap_err()
                .code,
            "TABLE_NOT_FOUND"
        );
        assert_eq!(
            engine
                .prepare_execute_sql("CREATE TABLE other (id INTEGER PRIMARY KEY)", &[])
                .unwrap_err()
                .code,
            "PREPARED_COMMIT_PENDING"
        );

        let mut wrong = create_bytes.clone();
        *wrong.last_mut().unwrap() ^= 1;
        assert_eq!(
            engine.install_prepared_commit(&wrong).unwrap_err().code,
            "PREPARED_COMMIT_MISMATCH"
        );
        assert_eq!(engine.revision(), 0);
        engine.abort_prepared_commit().unwrap();
        assert_eq!(
            engine
                .query_sql("SELECT id FROM posts", &[])
                .unwrap_err()
                .code,
            "TABLE_NOT_FOUND"
        );

        let prepared = engine
            .prepare_execute_sql(
                "CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT)",
                &[],
            )
            .unwrap();
        let bytes = prepared.commit.unwrap().into_bytes();
        let outcome = engine.install_prepared_commit(&bytes).unwrap();
        assert_eq!(outcome.revision, 1);
        assert_eq!(engine.revision(), 1);
    }

    #[test]
    fn prepared_bytes_are_deterministic_small_row_deltas_and_replayable() {
        let mut first = Engine::default();
        first
            .execute_sql(
                "CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT NOT NULL)",
                &[],
            )
            .unwrap();
        first
            .execute_sql(
                "INSERT INTO posts (id, title) VALUES (1, 'one'), (2, 'two')",
                &[],
            )
            .unwrap();
        let base = first.export_snapshot().unwrap();
        let mut second = Engine::default();
        second.import_snapshot(&base).unwrap();

        let left = first
            .prepare_execute_sql("UPDATE posts SET title = 'changed' WHERE id = 2", &[])
            .unwrap()
            .commit
            .unwrap()
            .into_bytes();
        let right = second
            .prepare_execute_sql("UPDATE posts SET title = 'changed' WHERE id = 2", &[])
            .unwrap()
            .commit
            .unwrap()
            .into_bytes();
        assert_eq!(left, right);
        assert!(left.len() < 512, "one-row delta was {} bytes", left.len());

        first.abort_prepared_commit().unwrap();
        let outcome = first.replay_commit(&left).unwrap();
        assert_eq!(outcome.revision, 3);
        assert_eq!(
            first
                .query_sql("SELECT title FROM posts WHERE id = 2", &[])
                .unwrap()
                .rows,
            vec![row(json!({"title": "changed"}))]
        );
        assert_eq!(
            first.replay_commit(&left).unwrap_err().code,
            "PREPARED_COMMIT_REVISION_MISMATCH"
        );
    }

    #[test]
    fn prepared_transactions_coalesce_all_mutations_and_are_consumed_on_prepare() {
        let mut engine = Engine::default();
        create_posts(&mut engine);
        engine.begin_transaction().unwrap();
        engine
            .execute_sql("INSERT INTO posts (id, title) VALUES (1, 'one')", &[])
            .unwrap();
        engine
            .execute_sql("UPDATE posts SET published = true WHERE id = 1", &[])
            .unwrap();

        let prepared = engine.prepare_commit_transaction().unwrap();
        assert!(!engine.in_transaction());
        assert_eq!(engine.revision(), 1);
        assert!(
            engine
                .query_sql("SELECT id FROM posts", &[])
                .unwrap()
                .rows
                .is_empty()
        );
        let bytes = prepared.commit.unwrap().into_bytes();
        assert!(!String::from_utf8_lossy(&bytes).contains("INSERT INTO"));
        engine.install_prepared_commit(&bytes).unwrap();
        assert_eq!(engine.revision(), 2);
        assert_eq!(
            engine
                .query_sql("SELECT published FROM posts WHERE id = 1", &[])
                .unwrap()
                .rows,
            vec![row(json!({"published": true}))]
        );

        engine.begin_transaction().unwrap();
        let empty = engine.prepare_commit_transaction().unwrap();
        assert!(empty.commit.is_none());
        assert!(!engine.in_transaction());
    }

    #[test]
    fn prepared_catalog_and_source_mutations_replay_with_exact_revision_semantics() {
        let schema = TableSchema {
            name: "posts".to_owned(),
            primary_key: vec!["id".to_owned()],
            columns: vec![],
        };
        let mut source = Engine::default();
        let defined = source.prepare_define_tables(vec![schema.clone()]).unwrap();
        assert_eq!(defined.commit.as_ref().unwrap().revision_before(), 0);
        assert_eq!(defined.commit.as_ref().unwrap().revision_after(), 0);
        let define_bytes = defined.commit.unwrap().into_bytes();
        source.install_prepared_commit(&define_bytes).unwrap();
        assert_eq!(source.revision(), 0);

        let batch = ChangeBatch {
            changes: vec![Change::Upsert {
                table: "posts".to_owned(),
                row: row(json!({"id": 1, "title": "one"})),
            }],
            ..ChangeBatch::default()
        };
        let applied = source.prepare_apply_batch(&batch).unwrap();
        let batch_bytes = applied.commit.unwrap().into_bytes();
        source.install_prepared_commit(&batch_bytes).unwrap();
        assert_eq!(source.revision(), 1);

        let mut restored = Engine::default();
        restored.replay_commit(&define_bytes).unwrap();
        restored.replay_commit(&batch_bytes).unwrap();
        assert_eq!(restored.revision(), 1);
        assert_eq!(
            restored
                .query(&QueryPlan {
                    table: "posts".to_owned(),
                    columns: None,
                    filters: vec![],
                    predicate: None,
                    order_by: vec![],
                    limit: None,
                    offset: 0,
                })
                .unwrap()
                .rows,
            vec![row(json!({"id": 1, "title": "one"}))]
        );
    }

    #[test]
    fn prepared_schema_migration_replay_preserves_existing_indexes() {
        let mut engine = Engine::default();
        engine
            .execute_sql(
                "CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT NOT NULL)",
                &[],
            )
            .unwrap();
        engine
            .execute_sql("CREATE INDEX posts_title ON posts (title)", &[])
            .unwrap();
        engine
            .execute_sql("INSERT INTO posts (id, title) VALUES (1, 'one')", &[])
            .unwrap();
        let base = engine.export_snapshot().unwrap();
        let bytes = engine
            .prepare_execute_sql("ALTER TABLE posts ADD COLUMN note TEXT", &[])
            .unwrap()
            .commit
            .unwrap()
            .into_bytes();
        engine.abort_prepared_commit().unwrap();

        let mut restored = Engine::default();
        restored.import_snapshot(&base).unwrap();
        restored.replay_commit(&bytes).unwrap();
        assert_eq!(
            restored
                .into_storage()
                .index_definition("posts_title")
                .unwrap()
                .columns,
            vec!["title"]
        );
    }

    #[test]
    fn failed_prepare_and_corrupt_replay_preserve_prior_state() {
        let mut engine = Engine::default();
        create_posts(&mut engine);
        let before = engine.export_snapshot().unwrap();
        assert_eq!(
            engine
                .prepare_execute_sql("INSERT INTO posts (id, title) VALUES (1, 42)", &[])
                .unwrap_err()
                .code,
            "TYPE_MISMATCH"
        );
        assert_eq!(engine.export_snapshot().unwrap(), before);

        let prepared = engine
            .prepare_execute_sql("INSERT INTO posts (id, title) VALUES (1, 'one')", &[])
            .unwrap();
        let mut corrupt = prepared.commit.unwrap().into_bytes();
        engine.abort_prepared_commit().unwrap();
        *corrupt.last_mut().unwrap() ^= 1;
        assert_eq!(
            engine.replay_commit(&corrupt).unwrap_err().code,
            "INVALID_PREPARED_COMMIT"
        );
        assert_eq!(engine.export_snapshot().unwrap(), before);
    }

    #[test]
    fn every_sql_mutation_prepares_installs_and_replays_the_same_state() {
        fn apply(source: &mut Engine, replica: &mut Engine, sql: &str) -> Vec<u8> {
            let prepared = source.prepare_execute_sql(sql, &[]).unwrap();
            let bytes = prepared.commit.unwrap().into_bytes();
            source.install_prepared_commit(&bytes).unwrap();
            replica.replay_commit(&bytes).unwrap();
            assert_eq!(
                source.export_snapshot().unwrap(),
                replica.export_snapshot().unwrap()
            );
            bytes
        }

        let mut source = Engine::default();
        let mut replica = Engine::default();
        apply(
            &mut source,
            &mut replica,
            "CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT NOT NULL)",
        );
        apply(
            &mut source,
            &mut replica,
            "INSERT INTO posts (id, title) VALUES (1, 'one'), (2, 'two')",
        );
        apply(
            &mut source,
            &mut replica,
            "CREATE INDEX posts_title ON posts (title)",
        );
        apply(
            &mut source,
            &mut replica,
            "UPDATE posts SET title = 'changed' WHERE id = 2",
        );

        // A matched write that produces the same row still preserves the
        // existing revision/invalidation contract without inventing SQL redo.
        let revision_only = apply(
            &mut source,
            &mut replica,
            "UPDATE posts SET title = 'changed' WHERE id = 2",
        );
        assert!(String::from_utf8_lossy(&revision_only).contains("\"operations\":[]"));

        apply(&mut source, &mut replica, "DELETE FROM posts WHERE id = 1");
        apply(
            &mut source,
            &mut replica,
            "ALTER TABLE posts ADD COLUMN note TEXT",
        );
        apply(&mut source, &mut replica, "DROP INDEX posts_title");
        apply(&mut source, &mut replica, "DROP TABLE posts");
    }

    #[test]
    fn replacement_and_batch_deltas_install_and_replay_atomically() {
        let schema = TableSchema {
            name: "posts".to_owned(),
            primary_key: vec!["id".to_owned()],
            columns: vec![],
        };
        let mut source = Engine::default();
        let mut replica = Engine::default();

        for rows in [
            vec![
                row(json!({"id": 1, "title": "one"})),
                row(json!({"id": 2, "title": "two"})),
            ],
            vec![
                row(json!({"id": 2, "title": "changed"})),
                row(json!({"id": 3, "title": "three"})),
            ],
        ] {
            let prepared = source
                .prepare_replace_table_snapshot(schema.clone(), rows)
                .unwrap();
            let bytes = prepared.commit.unwrap().into_bytes();
            source.install_prepared_commit(&bytes).unwrap();
            replica.replay_commit(&bytes).unwrap();
            assert_eq!(
                source.export_snapshot().unwrap(),
                replica.export_snapshot().unwrap()
            );
        }

        let batch = ChangeBatch {
            changes: vec![
                Change::Delete {
                    table: "posts".to_owned(),
                    key: row(json!({"id": 2})),
                },
                Change::Upsert {
                    table: "posts".to_owned(),
                    row: row(json!({"id": 4, "title": "four"})),
                },
            ],
            ..ChangeBatch::default()
        };
        let prepared = source.prepare_apply_batch(&batch).unwrap();
        let bytes = prepared.commit.unwrap().into_bytes();
        source.install_prepared_commit(&bytes).unwrap();
        replica.replay_commit(&bytes).unwrap();
        assert_eq!(
            source.export_snapshot().unwrap(),
            replica.export_snapshot().unwrap()
        );
    }

    #[test]
    fn define_tables_is_atomic_and_abort_is_idempotent() {
        let mut engine = Engine::default();
        let error = engine
            .prepare_define_tables(vec![
                TableSchema {
                    name: "would_have_existed".to_owned(),
                    primary_key: vec!["id".to_owned()],
                    columns: vec![],
                },
                TableSchema {
                    name: "invalid".to_owned(),
                    primary_key: vec![],
                    columns: vec![],
                },
            ])
            .unwrap_err();
        assert_eq!(error.code, "INVALID_SCHEMA");
        assert_eq!(
            engine
                .query_sql("SELECT id FROM would_have_existed", &[])
                .unwrap_err()
                .code,
            "TABLE_NOT_FOUND"
        );
        engine.abort_prepared_commit().unwrap();
        engine.abort_prepared_commit().unwrap();
    }

    #[test]
    fn prepared_table_definitions_ignore_idempotent_existing_schemas() {
        let existing = TableSchema {
            name: "existing".to_owned(),
            primary_key: vec!["id".to_owned()],
            columns: vec![],
        };
        let added = TableSchema {
            name: "added".to_owned(),
            primary_key: vec!["id".to_owned()],
            columns: vec![],
        };
        let mut source = Engine::default();
        source.define_table(existing.clone()).unwrap();
        let mut replica = source.clone();

        let prepared = source.prepare_define_tables(vec![existing, added]).unwrap();
        let bytes = prepared.commit.unwrap().into_bytes();
        let installed = source.install_prepared_commit(&bytes).unwrap();
        assert_eq!(installed.tables, vec!["added"]);
        let replayed = replica.replay_commit(&bytes).unwrap();
        assert_eq!(replayed.tables, vec!["added"]);
        assert_eq!(
            source.export_snapshot().unwrap(),
            replica.export_snapshot().unwrap()
        );
    }

    #[test]
    fn failed_transaction_prepare_retains_staged_work_for_explicit_rollback() {
        let mut engine = Engine::default();
        engine
            .execute_sql(
                "CREATE TABLE payloads (id INTEGER PRIMARY KEY, body TEXT NOT NULL)",
                &[],
            )
            .unwrap();
        engine.begin_transaction().unwrap();
        let body = "x".repeat(1_000_000);
        let rows = (0..18)
            .map(|id| row(json!({"id": id, "body": body})))
            .collect::<Vec<_>>();
        let transaction = engine.transaction.as_mut().unwrap();
        transaction
            .storage
            .replace_table_unrevisioned("payloads", rows)
            .unwrap();
        transaction.tables.insert("payloads".to_owned());

        assert_eq!(
            engine.prepare_commit_transaction().unwrap_err().code,
            "PREPARED_COMMIT_TOO_LARGE"
        );
        assert!(engine.in_transaction());
        assert_eq!(
            engine
                .query_sql("SELECT id FROM payloads", &[])
                .unwrap()
                .rows
                .len(),
            18
        );
        engine.rollback_transaction().unwrap();
        assert!(
            engine
                .query_sql("SELECT id FROM payloads", &[])
                .unwrap()
                .rows
                .is_empty()
        );
    }
}
