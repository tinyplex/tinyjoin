use std::collections::BTreeMap;
use std::mem::size_of;

use serde_json::Value;

use crate::query::{
    MAX_SQL_PARAMETERS, Token, bind_predicate_parameters, bind_prepared_value, parameter_index,
    prepared_parameter_marker, tokenize, validate_bound_parameter_bytes, validate_sql_input,
    validate_sql_parameters,
};
use crate::statement::{SqlValue, Statement, WriteStatement};
use crate::{EngineError, Result};

pub type PreparedStatementId = u32;

const MAX_PREPARED_STATEMENTS: usize = 128;
const MAX_PREPARED_RETAINED_BYTES: usize = 8 * 1024 * 1024;
const TOKEN_AST_UPPER_BOUND: usize = 512;
const PREPARED_ENTRY_OVERHEAD: usize = 256;

#[derive(Clone, Debug)]
pub(crate) struct PreparedStatement {
    source: Box<str>,
    statement: Statement,
    parameter_count: usize,
    parameter_occurrences: Box<[usize]>,
    limit_parameter: Option<usize>,
    offset_parameter: Option<usize>,
    retained_bytes: usize,
}

impl PreparedStatement {
    fn parse(sql: &str) -> Result<Self> {
        // Enforce the public SQL text bound before the layout lexer is allowed to allocate.
        validate_sql_input(sql, &[])?;
        let tokens = tokenize(sql)?;
        let layout = ParameterLayout::from_tokens(&tokens)?;
        let params = (1..=layout.parameter_count)
            .map(prepared_parameter_marker)
            .collect::<Vec<_>>();
        let statement = crate::statement::parse(sql, &params)?;
        if matches!(
            statement,
            Statement::Write(
                WriteStatement::CreateTable { .. }
                    | WriteStatement::CreateIndex { .. }
                    | WriteStatement::DropTable { .. }
                    | WriteStatement::DropIndex { .. }
                    | WriteStatement::AddColumn { .. }
            )
        ) {
            return Err(EngineError::unsupported_sql(
                "Prepared statements support SELECT, INSERT, UPDATE, and DELETE, but not DDL",
            ));
        }
        let retained_bytes = retained_bytes(sql.len(), tokens.len(), layout.parameter_count)?;
        Ok(Self {
            source: sql.into(),
            statement,
            parameter_count: layout.parameter_count,
            parameter_occurrences: layout.parameter_occurrences.into_boxed_slice(),
            limit_parameter: layout.limit_parameter,
            offset_parameter: layout.offset_parameter,
            retained_bytes,
        })
    }

    pub(crate) fn bind(&self, params: &[Value]) -> Result<Statement> {
        if params.len() != self.parameter_count {
            return Err(EngineError::bind_error(format!(
                "Prepared statement expects {} parameters, but received {}",
                self.parameter_count,
                params.len()
            )));
        }
        // Validate every slot, including gaps in PostgreSQL-style numbering, before cloning the
        // parsed template or opening any mutation candidate.
        validate_sql_parameters(params)?;
        validate_bound_parameter_bytes(&self.parameter_occurrences, params)?;
        match &self.statement {
            Statement::Select(plan) => Ok(Statement::Select(
                crate::query::bind_select_plan_parameters(
                    plan,
                    params,
                    self.limit_parameter,
                    self.offset_parameter,
                )?,
            )),
            Statement::Aggregate(plan) => Ok(Statement::Aggregate(
                crate::aggregate::bind_plan_parameters(
                    plan,
                    params,
                    self.limit_parameter,
                    self.offset_parameter,
                )?,
            )),
            Statement::Join(plan) => Ok(Statement::Join(crate::join::bind_plan_parameters(
                plan,
                params,
                self.limit_parameter,
                self.offset_parameter,
            )?)),
            Statement::Write(statement) => {
                bind_write_statement(statement, params).map(Statement::Write)
            }
        }
    }

    fn retained_bytes(&self) -> usize {
        debug_assert!(self.source.len() <= self.retained_bytes);
        self.retained_bytes
    }
}

#[derive(Debug)]
pub(crate) struct PreparedStatementRegistry {
    statements: BTreeMap<PreparedStatementId, PreparedStatement>,
    next_id: PreparedStatementId,
    retained_bytes: usize,
}

impl Default for PreparedStatementRegistry {
    fn default() -> Self {
        Self {
            statements: BTreeMap::new(),
            next_id: 1,
            retained_bytes: 0,
        }
    }
}

impl PreparedStatementRegistry {
    pub(crate) fn prepare(&mut self, sql: &str) -> Result<PreparedStatementId> {
        if self.statements.len() == MAX_PREPARED_STATEMENTS {
            return Err(prepared_limit("The prepared statement registry is full"));
        }
        if self.next_id == PreparedStatementId::MAX {
            return Err(prepared_limit(
                "The prepared statement identifier space is exhausted",
            ));
        }
        let statement = PreparedStatement::parse(sql)?;
        let retained_bytes = self
            .retained_bytes
            .checked_add(statement.retained_bytes())
            .ok_or_else(|| prepared_limit("Prepared statement memory accounting overflowed"))?;
        if retained_bytes > MAX_PREPARED_RETAINED_BYTES {
            return Err(prepared_limit(
                "Prepared statements cannot retain more than 8388608 bytes",
            ));
        }
        let id = self.next_id;
        self.next_id += 1;
        self.retained_bytes = retained_bytes;
        self.statements.insert(id, statement);
        Ok(id)
    }

    pub(crate) fn bind(&self, id: PreparedStatementId, params: &[Value]) -> Result<Statement> {
        self.statements
            .get(&id)
            .ok_or_else(|| prepared_not_found(id))?
            .bind(params)
    }

    pub(crate) fn close(&mut self, id: PreparedStatementId) -> Result<()> {
        if id == 0 || id >= self.next_id {
            return Err(prepared_not_found(id));
        }
        if let Some(statement) = self.statements.remove(&id) {
            self.retained_bytes = self
                .retained_bytes
                .checked_sub(statement.retained_bytes())
                .expect("registered prepared statement bytes were charged");
        }
        Ok(())
    }
}

struct ParameterLayout {
    parameter_count: usize,
    parameter_occurrences: Vec<usize>,
    limit_parameter: Option<usize>,
    offset_parameter: Option<usize>,
}

impl ParameterLayout {
    fn from_tokens(tokens: &[Token]) -> Result<Self> {
        let mut parameter_count = 0;
        let mut parameter_occurrences = Vec::new();
        let mut limit_parameter = None;
        let mut offset_parameter = None;
        for (position, token) in tokens.iter().enumerate() {
            let Token::Placeholder(raw_index) = token else {
                continue;
            };
            let index = parameter_index(raw_index)?;
            if index > MAX_SQL_PARAMETERS {
                return Err(EngineError::bind_error(format!(
                    "Parameter placeholder `${raw_index}` exceeds the {MAX_SQL_PARAMETERS}-parameter limit"
                )));
            }
            parameter_count = parameter_count.max(index);
            parameter_occurrences.resize(parameter_count, 0);
            parameter_occurrences[index - 1] += 1;
            match position
                .checked_sub(1)
                .and_then(|position| tokens.get(position))
            {
                Some(Token::Identifier {
                    value,
                    quoted: false,
                }) if value.eq_ignore_ascii_case("limit") => limit_parameter = Some(index),
                Some(Token::Identifier {
                    value,
                    quoted: false,
                }) if value.eq_ignore_ascii_case("offset") => offset_parameter = Some(index),
                _ => {}
            }
        }
        Ok(Self {
            parameter_count,
            parameter_occurrences,
            limit_parameter,
            offset_parameter,
        })
    }
}

fn bind_write_statement(statement: &WriteStatement, params: &[Value]) -> Result<WriteStatement> {
    let mut statement = statement.clone();
    match &mut statement {
        WriteStatement::Insert { values, .. } => {
            for row in values {
                for value in row {
                    bind_sql_value(value, params)?;
                }
            }
        }
        WriteStatement::Update {
            assignments,
            predicate,
            ..
        } => {
            for (_, value) in assignments {
                bind_sql_value(value, params)?;
            }
            bind_predicate_parameters(predicate.as_mut(), params)?;
        }
        WriteStatement::Delete { predicate, .. } => {
            bind_predicate_parameters(predicate.as_mut(), params)?;
        }
        WriteStatement::CreateTable { .. }
        | WriteStatement::CreateIndex { .. }
        | WriteStatement::DropTable { .. }
        | WriteStatement::DropIndex { .. }
        | WriteStatement::AddColumn { .. } => {
            return Err(EngineError::new(
                "INTERNAL_ERROR",
                "A prepared statement registry retained unsupported DDL",
            ));
        }
    }
    Ok(statement)
}

fn bind_sql_value(value: &mut SqlValue, params: &[Value]) -> Result<()> {
    match value {
        SqlValue::Value(value) => bind_prepared_value(value, params),
        SqlValue::Default => Ok(()),
    }
}

fn retained_bytes(sql_bytes: usize, tokens: usize, parameter_count: usize) -> Result<usize> {
    // The source is retained for statement identity and diagnostics. Token storage is discarded,
    // but every retained AST node originates in a bounded token. Charging 512 bytes per token is
    // deliberately conservative enough to cover enum/vector capacity and the allocated JSON map
    // used by a parameter marker, in addition to separately charged owned source bytes and entry
    // overhead. Token-dense statements can therefore hit the retained-memory bound early.
    size_of::<PreparedStatement>()
        .checked_add(PREPARED_ENTRY_OVERHEAD)
        .and_then(|bytes| bytes.checked_add(sql_bytes))
        .and_then(|bytes| bytes.checked_add(parameter_count.checked_mul(size_of::<usize>())?))
        .and_then(|bytes| bytes.checked_add(tokens.checked_mul(TOKEN_AST_UPPER_BOUND)?))
        .ok_or_else(|| prepared_limit("Prepared statement memory accounting overflowed"))
}

fn prepared_not_found(id: PreparedStatementId) -> EngineError {
    EngineError::new(
        "PREPARED_STATEMENT_NOT_FOUND",
        format!("Prepared statement `{id}` is not open"),
    )
}

fn prepared_limit(message: impl Into<String>) -> EngineError {
    EngineError::new("PREPARED_STATEMENT_LIMIT", message)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::{MemoryPageDevice, PagedEngine, Row};

    fn row(value: Value) -> Row {
        value.as_object().unwrap().clone()
    }

    fn engine() -> PagedEngine<MemoryPageDevice> {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .execute_sql(
                "CREATE TABLE tasks (\
                    id INTEGER PRIMARY KEY, \
                    title TEXT NOT NULL, \
                    done BOOLEAN NOT NULL DEFAULT false\
                )",
                &[],
            )
            .unwrap();
        engine
            .execute_sql(
                "CREATE TABLE owners (\
                    id INTEGER PRIMARY KEY, \
                    task_id INTEGER NOT NULL, \
                    name TEXT NOT NULL\
                )",
                &[],
            )
            .unwrap();
        engine
    }

    #[test]
    fn numbering_uses_the_highest_slot_and_allows_gaps() {
        let statement =
            PreparedStatement::parse("SELECT id FROM tasks WHERE id = $2 OR id = $2 LIMIT $3")
                .unwrap();
        assert_eq!(statement.parameter_count, 3);
        assert_eq!(&*statement.parameter_occurrences, &[0, 2, 1]);
        assert_eq!(statement.limit_parameter, Some(3));
        statement
            .bind(&[json!({"unused": true}), json!(7), json!(1)])
            .unwrap();
        assert_eq!(
            statement.bind(&[json!(7), json!(1)]).unwrap_err().code,
            "BIND_ERROR"
        );
    }

    #[test]
    fn invalid_and_excessive_parameter_indexes_fail_at_prepare_time() {
        for sql in [
            "SELECT id FROM tasks WHERE id = $0",
            "SELECT id FROM tasks WHERE id = $1025",
            "SELECT id FROM tasks WHERE id = $999999999999999999999999999999",
        ] {
            assert_eq!(
                PreparedStatement::parse(sql).unwrap_err().code,
                "BIND_ERROR"
            );
        }
    }

    #[test]
    fn expanded_parameter_limits_cover_every_parser_and_prepared_binding() {
        let placeholders = vec!["$1"; 384].join(", ");
        let insert_rows = (0..384)
            .map(|id| format!("({id}, $1)"))
            .collect::<Vec<_>>()
            .join(", ");
        let statements = [
            format!("SELECT id FROM tasks WHERE title IN ({placeholders}) LIMIT 0"),
            format!("SELECT COUNT(*) AS count FROM tasks WHERE title IN ({placeholders}) LIMIT 0"),
            format!(
                "SELECT t.id FROM tasks AS t JOIN owners AS o ON t.id = o.task_id WHERE t.title IN ({placeholders}) LIMIT 0"
            ),
            format!("UPDATE tasks SET done = true WHERE title IN ({placeholders})"),
            format!("DELETE FROM tasks WHERE title IN ({placeholders})"),
            format!("INSERT INTO tasks (id, title) VALUES {insert_rows}"),
        ];
        let oversized = [json!("x".repeat(64 * 1024))];
        let smaller = [json!("x".repeat(1024))];
        for sql in statements {
            let statement = PreparedStatement::parse(&sql).unwrap();
            assert_eq!(statement.parameter_occurrences[0], 384);
            assert_eq!(
                crate::statement::parse(&sql, &oversized).unwrap_err().code,
                "RESOURCE_LIMIT",
                "direct: {sql}"
            );
            assert_eq!(
                statement.bind(&oversized).unwrap_err().code,
                "RESOURCE_LIMIT",
                "prepared: {sql}"
            );
            // This cumulative amount would exceed the budget if accounting leaked between calls.
            for _ in 0..48 {
                statement.bind(&smaller).unwrap();
            }
            crate::statement::parse(&sql, &smaller).unwrap();
        }
    }

    #[test]
    fn parameter_occurrences_ignore_quoted_text_comments_and_unused_slots() {
        let quoted = "$1 ".repeat(384);
        let sql = format!(
            "SELECT \"$9999\" FROM tasks WHERE title = $2 AND title <> '{quoted}' \
             /* $0 /* $9999 */ $2 */ -- $9999"
        );
        let statement = PreparedStatement::parse(&sql).unwrap();
        assert_eq!(&*statement.parameter_occurrences, &[0, 1]);
        let params = [json!("x".repeat(64 * 1024)), json!("y".repeat(64 * 1024))];
        crate::statement::parse(&sql, &params).unwrap();
        statement.bind(&params).unwrap();

        let sparse = PreparedStatement::parse("SELECT id FROM tasks WHERE id = $1024").unwrap();
        assert_eq!(sparse.parameter_occurrences.len(), 1024);
        assert_eq!(sparse.parameter_occurrences.iter().sum::<usize>(), 1);
        assert!(sparse.retained_bytes() >= 1024 * size_of::<usize>());
        let mut params = vec![Value::Null; 1024];
        params[1023] = json!(1);
        sparse.bind(&params).unwrap();
        crate::statement::parse("SELECT id FROM tasks WHERE id = $1024", &params).unwrap();

        for sql in [
            "SELECT id FROM tasks WHERE id = $0",
            "SELECT id FROM tasks WHERE id = $1025",
        ] {
            assert_eq!(
                crate::statement::parse(sql, &params).unwrap_err().code,
                "BIND_ERROR"
            );
        }
        assert_eq!(
            crate::statement::parse("SELECT id FROM tasks WHERE id = $2", &[json!(1)])
                .unwrap_err()
                .message,
            "No value was provided for `$2`"
        );
    }

    #[test]
    fn expanded_binding_failures_preserve_committed_and_staged_rows() {
        let mut engine = engine();
        engine
            .execute_sql("INSERT INTO tasks (id, title) VALUES (1, 'base')", &[])
            .unwrap();
        let placeholders = vec!["$1"; 384].join(", ");
        let insert_rows = (10..394)
            .map(|id| format!("({id}, $1)"))
            .collect::<Vec<_>>()
            .join(", ");
        let statements = [
            format!("UPDATE tasks SET done = true WHERE title IN ({placeholders})"),
            format!("DELETE FROM tasks WHERE title IN ({placeholders})"),
            format!("INSERT INTO tasks (id, title) VALUES {insert_rows}"),
        ]
        .into_iter()
        .map(|sql| {
            let id = engine.prepare_sql(&sql).unwrap();
            (sql, id)
        })
        .collect::<Vec<_>>();
        let oversized = [json!("x".repeat(64 * 1024))];
        for transaction in [false, true] {
            if transaction {
                engine.begin_transaction().unwrap();
                engine
                    .execute_sql("INSERT INTO tasks (id, title) VALUES (2, 'staged')", &[])
                    .unwrap();
            }
            let before = engine
                .query_sql("SELECT * FROM tasks ORDER BY id", &[])
                .unwrap();
            for (sql, id) in &statements {
                assert_eq!(
                    engine.execute_sql(sql, &oversized).unwrap_err().code,
                    "RESOURCE_LIMIT"
                );
                assert_eq!(
                    engine.execute_prepared(*id, &oversized).unwrap_err().code,
                    "RESOURCE_LIMIT"
                );
                assert_eq!(
                    engine
                        .query_sql("SELECT * FROM tasks ORDER BY id", &[])
                        .unwrap(),
                    before
                );
                assert_eq!(engine.in_transaction(), transaction);
            }
            // Reusing an over-budget prepared handle with small bindings remains valid.
            assert_eq!(
                engine
                    .execute_prepared(statements[0].1, &[json!("not present")])
                    .unwrap()
                    .row_count,
                0
            );
            if transaction {
                engine.commit_transaction().unwrap();
            }
        }
        let reopened = PagedEngine::open(engine.into_device()).unwrap();
        assert_eq!(
            reopened
                .query_sql("SELECT id, title FROM tasks ORDER BY id", &[])
                .unwrap()
                .rows,
            vec![
                row(json!({"id": 1, "title": "base"})),
                row(json!({"id": 2, "title": "staged"}))
            ]
        );
    }

    #[test]
    fn prepared_execution_reuses_select_aggregate_join_and_dml_templates() {
        let mut engine = engine();
        let insert = engine
            .prepare_sql(
                "INSERT INTO tasks (id, title, done) VALUES ($1, $2, $3) \
                 RETURNING id, title, done",
            )
            .unwrap();
        for params in [
            vec![json!(1), json!("one"), json!(false)],
            vec![json!(2), json!("two"), json!(true)],
        ] {
            assert_eq!(
                engine.execute_prepared(insert, &params).unwrap().row_count,
                1
            );
        }

        let update = engine
            .prepare_sql("UPDATE tasks SET title = $1 WHERE id = $2 RETURNING id, title")
            .unwrap();
        assert_eq!(
            engine
                .execute_prepared(update, &[json!("updated"), json!(2)])
                .unwrap()
                .rows,
            vec![row(json!({"id": 2, "title": "updated"}))]
        );

        let select = engine
            .prepare_sql(
                "SELECT id, title FROM tasks WHERE id >= $2 ORDER BY id LIMIT $3 OFFSET $4",
            )
            .unwrap();
        assert_eq!(
            engine
                .execute_prepared(
                    select,
                    &[json!({"unused": true}), json!(1), json!(1), json!(1)],
                )
                .unwrap()
                .rows,
            vec![row(json!({"id": 2, "title": "updated"}))]
        );

        let aggregate = engine
            .prepare_sql(
                "SELECT done, COUNT(*) AS count FROM tasks WHERE id >= $1 \
                 GROUP BY done ORDER BY done LIMIT $2",
            )
            .unwrap();
        assert_eq!(
            engine
                .execute_prepared(aggregate, &[json!(1), json!(1)])
                .unwrap()
                .rows,
            vec![row(json!({"done": false, "count": 1}))]
        );

        let insert_owner = engine
            .prepare_sql("INSERT INTO owners (id, task_id, name) VALUES ($1, $2, $3)")
            .unwrap();
        engine
            .execute_prepared(insert_owner, &[json!(1), json!(2), json!("Ada")])
            .unwrap();
        let join = engine
            .prepare_sql(
                "SELECT t.id AS id, o.name AS owner FROM tasks t \
                 JOIN owners o ON t.id = o.task_id WHERE t.id = $1 LIMIT $2",
            )
            .unwrap();
        assert_eq!(
            engine
                .execute_prepared(join, &[json!(2), json!(1)])
                .unwrap()
                .rows,
            vec![row(json!({"id": 2, "owner": "Ada"}))]
        );

        let delete = engine
            .prepare_sql("DELETE FROM tasks WHERE id = $1 RETURNING id")
            .unwrap();
        assert_eq!(
            engine.execute_prepared(delete, &[json!(1)]).unwrap().rows,
            vec![row(json!({"id": 1}))]
        );
    }

    #[test]
    fn exact_arity_and_all_parameter_values_are_validated_before_mutation() {
        let mut engine = engine();
        engine
            .execute_sql("INSERT INTO tasks (id, title) VALUES (1, 'original')", &[])
            .unwrap();
        let update = engine
            .prepare_sql("UPDATE tasks SET title = $2 WHERE id = 1")
            .unwrap();
        let revision = engine.revision();

        for params in [
            vec![json!("only one")],
            vec![json!(null), json!("two"), json!(3)],
        ] {
            assert_eq!(
                engine.execute_prepared(update, &params).unwrap_err().code,
                "BIND_ERROR"
            );
            assert_eq!(engine.revision(), revision);
        }

        let mut too_deep = json!(null);
        for _ in 0..=crate::storage::MAX_JSON_DEPTH {
            too_deep = Value::Array(vec![too_deep]);
        }
        assert_eq!(
            engine
                .execute_prepared(update, &[too_deep, json!("mutated")])
                .unwrap_err()
                .code,
            "BIND_ERROR"
        );
        assert_eq!(engine.revision(), revision);
        assert_eq!(
            engine
                .query_sql("SELECT title FROM tasks WHERE id = 1", &[])
                .unwrap()
                .rows,
            vec![row(json!({"title": "original"}))]
        );

        assert_eq!(
            engine
                .execute_prepared(update, &[json!(null), json!(42)])
                .unwrap_err()
                .code,
            "TYPE_MISMATCH"
        );
        assert_eq!(engine.revision(), revision);
        engine
            .execute_prepared(update, &[json!({"unused": true}), json!("mutated")])
            .unwrap();
        assert_eq!(
            engine
                .query_sql("SELECT title FROM tasks WHERE id = 1", &[])
                .unwrap()
                .rows,
            vec![row(json!({"title": "mutated"}))]
        );
    }

    #[test]
    fn ddl_is_rejected_and_schema_changes_are_revalidated_on_execute() {
        let mut engine = engine();
        for sql in [
            "CREATE TABLE rejected (id INTEGER PRIMARY KEY)",
            "CREATE INDEX rejected ON tasks (title)",
            "DROP TABLE tasks",
            "DROP INDEX rejected",
            "ALTER TABLE tasks ADD COLUMN note TEXT",
        ] {
            assert_eq!(
                engine.prepare_sql(sql).unwrap_err().code,
                "UNSUPPORTED_SQL",
                "{sql}"
            );
        }
        assert_eq!(
            engine
                .prepare_sql("SELECT id FROM tasks; SELECT id FROM tasks")
                .unwrap_err()
                .code,
            "UNSUPPORTED_SQL"
        );

        engine
            .execute_sql("INSERT INTO tasks (id, title) VALUES (1, 'before')", &[])
            .unwrap();
        let select = engine
            .prepare_sql("SELECT title FROM tasks WHERE id = $1")
            .unwrap();
        assert_eq!(
            engine.execute_prepared(select, &[json!(1)]).unwrap().rows,
            vec![row(json!({"title": "before"}))]
        );

        engine
            .execute_sql("ALTER TABLE tasks ADD COLUMN note TEXT", &[])
            .unwrap();
        engine
            .execute_sql("CREATE INDEX tasks_title ON tasks (title)", &[])
            .unwrap();
        assert_eq!(
            engine.execute_prepared(select, &[json!(1)]).unwrap().rows,
            vec![row(json!({"title": "before"}))]
        );

        engine.execute_sql("DROP TABLE tasks", &[]).unwrap();
        engine
            .execute_sql(
                "CREATE TABLE tasks (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
                &[],
            )
            .unwrap();
        assert_eq!(
            engine
                .execute_prepared(select, &[json!(1)])
                .unwrap_err()
                .code,
            "COLUMN_NOT_FOUND"
        );

        engine.execute_sql("DROP TABLE tasks", &[]).unwrap();
        engine
            .execute_sql(
                "CREATE TABLE tasks (id INTEGER PRIMARY KEY, title TEXT NOT NULL)",
                &[],
            )
            .unwrap();
        engine
            .execute_sql("INSERT INTO tasks (id, title) VALUES (1, 'after')", &[])
            .unwrap();
        assert_eq!(
            engine.execute_prepared(select, &[json!(1)]).unwrap().rows,
            vec![row(json!({"title": "after"}))]
        );
    }

    #[test]
    fn prepared_statements_follow_explicit_transaction_visibility() {
        let mut engine = engine();
        let insert = engine
            .prepare_sql("INSERT INTO tasks (id, title) VALUES ($1, $2)")
            .unwrap();
        let select = engine
            .prepare_sql("SELECT title FROM tasks WHERE id = $1")
            .unwrap();

        engine.begin_transaction().unwrap();
        engine
            .execute_prepared(insert, &[json!(1), json!("staged")])
            .unwrap();
        assert_eq!(
            engine.execute_prepared(select, &[json!(1)]).unwrap().rows,
            vec![row(json!({"title": "staged"}))]
        );
        engine.rollback_transaction().unwrap();
        assert!(
            engine
                .execute_prepared(select, &[json!(1)])
                .unwrap()
                .rows
                .is_empty()
        );
    }

    #[test]
    fn handles_are_monotonic_close_is_idempotent_and_registry_is_bounded() {
        let mut engine = engine();
        let first = engine.prepare_sql("SELECT id FROM tasks").unwrap();
        engine.close_prepared(first).unwrap();
        engine.close_prepared(first).unwrap();
        assert_eq!(
            engine.execute_prepared(first, &[]).unwrap_err().code,
            "PREPARED_STATEMENT_NOT_FOUND"
        );
        for invalid in [0, first + 2] {
            assert_eq!(
                engine.close_prepared(invalid).unwrap_err().code,
                "PREPARED_STATEMENT_NOT_FOUND"
            );
        }
        let second = engine.prepare_sql("SELECT id FROM tasks").unwrap();
        assert!(second > first);

        let mut ids = vec![second];
        while ids.len() < MAX_PREPARED_STATEMENTS {
            ids.push(engine.prepare_sql("SELECT id FROM tasks").unwrap());
        }
        assert_eq!(
            engine.prepare_sql("SELECT id FROM tasks").unwrap_err().code,
            "PREPARED_STATEMENT_LIMIT"
        );
        engine.close_prepared(ids[0]).unwrap();
        assert!(engine.prepare_sql("SELECT id FROM tasks").is_ok());
    }

    #[test]
    fn retained_source_and_ast_charge_is_bounded_and_released_on_close() {
        let mut engine = engine();
        let prefix = "SELECT id FROM tasks";
        let sql = format!("{prefix}{}", " ".repeat(64 * 1024 - prefix.len()));
        let mut ids = Vec::new();
        let error = loop {
            match engine.prepare_sql(&sql) {
                Ok(id) => ids.push(id),
                Err(error) => break error,
            }
        };
        assert_eq!(error.code, "PREPARED_STATEMENT_LIMIT");
        assert!(!ids.is_empty());
        assert!(ids.len() < MAX_PREPARED_STATEMENTS);

        engine.close_prepared(ids[0]).unwrap();
        assert!(engine.prepare_sql(&sql).is_ok());
        assert_eq!(
            engine.prepare_sql(&format!("{sql} ")).unwrap_err().code,
            "INVALID_QUERY"
        );
    }
}
