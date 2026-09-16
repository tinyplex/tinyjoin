use serde_json::{Value, json};

use crate::{ExecuteResult, MemoryPageDevice, PagedEngine, Result, Row};

type Database = PagedEngine<MemoryPageDevice>;

fn row(value: Value) -> Row {
    value.as_object().unwrap().clone()
}

fn rows(engine: &Database, table: &str) -> Vec<Row> {
    engine
        .query_sql(&format!("SELECT * FROM {table} ORDER BY id"), &[])
        .unwrap()
        .rows
}

/// Every statement runs directly and through a prepared handle, committed and staged, so the
/// overlay path that cannot visit secondary indexes is exercised as well as the committed one.
#[derive(Clone, Copy, Debug)]
struct Mode {
    prepared: bool,
    transaction: bool,
}

const MODES: [Mode; 4] = [
    Mode {
        prepared: false,
        transaction: false,
    },
    Mode {
        prepared: true,
        transaction: false,
    },
    Mode {
        prepared: false,
        transaction: true,
    },
    Mode {
        prepared: true,
        transaction: true,
    },
];

fn open(mode: Mode, script: &str) -> Database {
    let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
    engine.exec_sql(script).unwrap();
    if mode.transaction {
        engine.begin_transaction().unwrap();
    }
    engine
}

fn run(engine: &mut Database, mode: Mode, sql: &str, params: &[Value]) -> Result<ExecuteResult> {
    if mode.prepared {
        let id = engine.prepare_sql(sql)?;
        let result = engine.execute_prepared(id, params);
        engine.close_prepared(id).unwrap();
        result
    } else {
        engine.execute_sql(sql, params)
    }
}

const KV: &str = "CREATE TABLE kv (id INTEGER PRIMARY KEY, v TEXT NOT NULL, n INTEGER DEFAULT 0);\
                  INSERT INTO kv (id, v) VALUES (1, 'a');";

#[test]
fn do_nothing_skips_rows_conflicting_with_stored_or_earlier_rows() {
    for mode in MODES {
        let mut engine = open(mode, KV);
        let result = run(
            &mut engine,
            mode,
            "INSERT INTO kv (id, v) VALUES (1, 'x'), (2, 'b'), ($1, 'c') \
             ON CONFLICT (id) DO NOTHING RETURNING id, v",
            &[json!(2)],
        )
        .unwrap();
        assert_eq!(result.command, "INSERT", "{mode:?}");
        assert_eq!(result.row_count, 1, "{mode:?}");
        assert_eq!(
            result.rows,
            vec![row(json!({"id": 2, "v": "b"}))],
            "{mode:?}"
        );
        assert_eq!(result.tables, ["kv"], "{mode:?}");
        assert_eq!(result.keys["kv"], vec![row(json!({"id": 2}))], "{mode:?}");
        assert_eq!(
            rows(&engine, "kv"),
            vec![
                row(json!({"id": 1, "v": "a", "n": 0})),
                row(json!({"id": 2, "v": "b", "n": 0})),
            ],
            "{mode:?}"
        );

        // A statement that skips every row changes nothing and reports no table.
        let revision = engine.revision();
        let result = run(
            &mut engine,
            mode,
            "INSERT INTO kv VALUES (1, 'z', 9) ON CONFLICT DO NOTHING RETURNING *",
            &[],
        )
        .unwrap();
        assert_eq!(result.row_count, 0, "{mode:?}");
        assert!(result.rows.is_empty(), "{mode:?}");
        assert!(result.tables.is_empty(), "{mode:?}");
        assert!(result.keys.is_empty(), "{mode:?}");
        assert_eq!(engine.revision(), revision, "{mode:?}");
    }
}

#[test]
fn do_update_rewrites_the_conflicting_row_from_excluded_values() {
    for mode in MODES {
        let mut engine = open(mode, KV);
        let result = run(
            &mut engine,
            mode,
            "INSERT INTO kv (id, v) VALUES (1, 'x'), (3, 'c') \
             ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v, n = $1 RETURNING *",
            &[json!(5)],
        )
        .unwrap();
        assert_eq!(result.row_count, 2, "{mode:?}");
        assert_eq!(
            result.rows,
            vec![
                row(json!({"id": 1, "v": "x", "n": 5})),
                row(json!({"id": 3, "v": "c", "n": 0})),
            ],
            "{mode:?}"
        );
        assert_eq!(
            result.keys["kv"],
            vec![row(json!({"id": 1})), row(json!({"id": 3}))],
            "{mode:?}"
        );

        // EXCLUDED holds the proposed row after defaults, and an unassigned column is kept.
        // Assigning a primary-key column its own value is allowed, as generated upserts often do.
        run(
            &mut engine,
            mode,
            "INSERT INTO kv (id, v) VALUES (1, 'ignored') \
             ON CONFLICT (id) DO UPDATE SET n = excluded.n, id = \"excluded\".id",
            &[],
        )
        .unwrap();
        assert_eq!(
            rows(&engine, "kv")[0],
            row(json!({"id": 1, "v": "x", "n": 0})),
            "{mode:?}"
        );

        let before = rows(&engine, "kv");
        for (sql, code) in [
            // Two proposed rows cannot both update, or insert and then update, the same row.
            (
                "INSERT INTO kv (id, v) VALUES (4, 'a'), (4, 'b') ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v",
                "CONSTRAINT_VIOLATION",
            ),
            (
                "INSERT INTO kv (id, v) VALUES (1, 'a'), (1, 'b') ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v",
                "CONSTRAINT_VIOLATION",
            ),
            (
                "INSERT INTO kv (id, v) VALUES (1, 'a') ON CONFLICT (id) DO UPDATE SET id = 99",
                "UNSUPPORTED_SQL",
            ),
            (
                "INSERT INTO kv (id, v) VALUES (1, 'a') ON CONFLICT (id) DO UPDATE SET v = DEFAULT",
                "CONSTRAINT_VIOLATION",
            ),
            (
                "INSERT INTO kv (id, v) VALUES (1, 'a') ON CONFLICT (id) DO UPDATE SET n = 'bad'",
                "TYPE_MISMATCH",
            ),
            (
                "INSERT INTO kv (id, v) VALUES (1, 'a') ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.v",
                "TYPE_MISMATCH",
            ),
            (
                "INSERT INTO kv (id, v) VALUES (1, 'a') ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.missing",
                "COLUMN_NOT_FOUND",
            ),
            (
                "INSERT INTO kv (id, v) VALUES (1, 'a') ON CONFLICT (id) DO UPDATE SET v = 'b', v = 'c'",
                "INVALID_QUERY",
            ),
            // A type error in SET is found even when no row conflicts.
            (
                "INSERT INTO kv (id, v) VALUES (50, 'a') ON CONFLICT (id) DO UPDATE SET n = 'bad'",
                "TYPE_MISMATCH",
            ),
        ] {
            let revision = engine.revision();
            assert_eq!(
                run(&mut engine, mode, sql, &[]).unwrap_err().code,
                code,
                "{mode:?}: {sql}"
            );
            assert_eq!(rows(&engine, "kv"), before, "{mode:?}: {sql}");
            assert_eq!(engine.revision(), revision, "{mode:?}: {sql}");
        }
    }
}

const USERS: &str = "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, name TEXT NOT NULL);\
                     CREATE UNIQUE INDEX users_email ON users (email);\
                     INSERT INTO users VALUES (1, 'a@x', 'A'), (2, NULL, 'N');";

#[test]
fn unique_index_targets_and_untargeted_do_nothing_use_every_arbiter() {
    for mode in MODES {
        let mut engine = open(mode, USERS);
        let result = run(
            &mut engine,
            mode,
            "INSERT INTO users VALUES (5, $1, 'A2') \
             ON CONFLICT (email) DO UPDATE SET name = EXCLUDED.name RETURNING id, name",
            &[json!("a@x")],
        )
        .unwrap();
        assert_eq!(
            result.rows,
            vec![row(json!({"id": 1, "name": "A2"}))],
            "{mode:?}"
        );

        // NULL never conflicts, so a NULL email is inserted even when another row has one.
        let result = run(
            &mut engine,
            mode,
            "INSERT INTO users VALUES (6, NULL, 'N2'), (7, 'b@x', 'B'), (8, 'b@x', 'B2') \
             ON CONFLICT (email) DO NOTHING RETURNING id",
            &[],
        )
        .unwrap();
        assert_eq!(
            result.rows,
            vec![row(json!({"id": 6})), row(json!({"id": 7}))],
            "{mode:?}"
        );

        // Without a target, the primary key and every unique index are arbiters.
        let result = run(
            &mut engine,
            mode,
            "INSERT INTO users VALUES (9, 'a@x', 'email'), (1, 'new@x', 'key'), (10, 'c@x', 'C') \
             ON CONFLICT DO NOTHING RETURNING id",
            &[],
        )
        .unwrap();
        assert_eq!(result.rows, vec![row(json!({"id": 10}))], "{mode:?}");

        let before = rows(&engine, "users");
        for (sql, code) in [
            // A conflict on a constraint that is not the arbiter still fails the statement.
            (
                "INSERT INTO users VALUES (11, 'a@x', 'x') ON CONFLICT (id) DO NOTHING",
                "CONSTRAINT_VIOLATION",
            ),
            (
                "INSERT INTO users VALUES (1, 'z@x', 'x') ON CONFLICT (email) DO NOTHING",
                "CONSTRAINT_VIOLATION",
            ),
            (
                "INSERT INTO users VALUES (1, 'z@x', 'x') ON CONFLICT (name) DO NOTHING",
                "INVALID_QUERY",
            ),
            // The first row moves `a@x` to `moved@x`, so the second conflicts with a row this
            // statement already updated.
            (
                "INSERT INTO users VALUES (12, 'a@x', 'x'), (13, 'moved@x', 'y') \
                 ON CONFLICT (email) DO UPDATE SET email = 'moved@x'",
                "CONSTRAINT_VIOLATION",
            ),
            // An update that collides with another row's unique value is not an arbiter conflict.
            (
                "INSERT INTO users VALUES (12, 'a@x', 'x') ON CONFLICT (email) DO UPDATE SET email = 'b@x'",
                "CONSTRAINT_VIOLATION",
            ),
        ] {
            assert_eq!(
                run(&mut engine, mode, sql, &[]).unwrap_err().code,
                code,
                "{mode:?}: {sql}"
            );
            assert_eq!(rows(&engine, "users"), before, "{mode:?}: {sql}");
        }

        // A row updated earlier in the statement no longer holds its old unique value, so a later
        // row proposing that value inserts rather than conflicting with the stale version.
        let result = run(
            &mut engine,
            mode,
            "INSERT INTO users VALUES (14, 'a@x', 'x'), (15, 'a@x', 'reused') \
             ON CONFLICT (email) DO UPDATE SET email = 'renamed@x' RETURNING id, email",
            &[],
        )
        .unwrap();
        assert_eq!(
            result.rows,
            vec![
                row(json!({"id": 1, "email": "renamed@x"})),
                row(json!({"id": 15, "email": "a@x"})),
            ],
            "{mode:?}"
        );

        if mode.transaction {
            engine.commit_transaction().unwrap();
        }
        let emails = engine
            .query_sql("SELECT id, email FROM users ORDER BY id", &[])
            .unwrap()
            .rows;
        assert_eq!(
            emails,
            [
                json!({"id": 1, "email": "renamed@x"}),
                json!({"id": 2, "email": null}),
                json!({"id": 6, "email": null}),
                json!({"id": 7, "email": "b@x"}),
                json!({"id": 10, "email": "c@x"}),
                json!({"id": 15, "email": "a@x"}),
            ]
            .map(row),
            "{mode:?}"
        );
        assert_eq!(before.len() + 1, emails.len(), "{mode:?}");
    }
}

#[test]
fn composite_targets_match_in_any_order_and_scripts_report_changed_keys() {
    let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
    let results = engine
        .exec_sql(
            "CREATE TABLE tags (post_id INTEGER, tag TEXT, weight INTEGER, PRIMARY KEY (post_id, tag));\
             INSERT INTO tags VALUES (1, 'x', 1);\
             INSERT INTO tags VALUES (1, 'x', 2), (1, 'y', 3) \
               ON CONFLICT (tag, post_id) DO UPDATE SET weight = EXCLUDED.weight;\
             INSERT INTO tags VALUES (1, 'y', 4) ON CONFLICT (post_id, tag) DO NOTHING;\
             SELECT tag, weight FROM tags ORDER BY tag",
        )
        .unwrap();
    assert_eq!(results[2].row_count, 2);
    assert_eq!(
        results[2].keys["tags"],
        vec![
            row(json!({"post_id": 1, "tag": "x"})),
            row(json!({"post_id": 1, "tag": "y"}))
        ]
    );
    assert_eq!(results[3].row_count, 0);
    assert_eq!(
        results[4].rows,
        vec![
            row(json!({"tag": "x", "weight": 2})),
            row(json!({"tag": "y", "weight": 3}))
        ]
    );
    let reopened = PagedEngine::open(engine.into_device()).unwrap();
    assert_eq!(
        reopened
            .query_sql("SELECT weight FROM tags ORDER BY tag", &[])
            .unwrap()
            .rows,
        vec![row(json!({"weight": 2})), row(json!({"weight": 3}))]
    );
}

#[test]
fn unsupported_on_conflict_forms_fail_explicitly() {
    let mut engine = open(MODES[0], KV);
    for (sql, code) in [
        (
            "INSERT INTO kv VALUES (1, 'a', 0) ON CONFLICT ON CONSTRAINT kv_primary DO NOTHING",
            "UNSUPPORTED_SQL",
        ),
        (
            "INSERT INTO kv VALUES (1, 'a', 0) ON CONFLICT DO UPDATE SET v = 'b'",
            "INVALID_QUERY",
        ),
        (
            "INSERT INTO kv VALUES (1, 'a', 0) ON CONFLICT (id) WHERE n > 0 DO NOTHING",
            "UNSUPPORTED_SQL",
        ),
        (
            "INSERT INTO kv VALUES (1, 'a', 0) ON CONFLICT (id) DO UPDATE SET v = 'b' WHERE n > 0",
            "UNSUPPORTED_SQL",
        ),
        (
            "INSERT INTO kv VALUES (1, 'a', 0) ON CONFLICT (id) DO UPDATE SET n = n",
            "UNSUPPORTED_SQL",
        ),
        (
            "INSERT INTO kv VALUES (1, 'a', 0) ON CONFLICT (id) DO",
            "SQL_PARSE_ERROR",
        ),
        (
            "INSERT INTO kv VALUES (1, 'a', 0) ON CONFLICT (id, id) DO NOTHING",
            "INVALID_QUERY",
        ),
        (
            "INSERT INTO kv VALUES (1, 'a', 0) ON CONFLICT (missing) DO NOTHING",
            "COLUMN_NOT_FOUND",
        ),
        (
            "INSERT INTO kv VALUES (1, 'a', 0) ON CONFLICT (id) DO NOTHING RETURNING id, id",
            "INVALID_QUERY",
        ),
    ] {
        assert_eq!(
            engine.execute_sql(sql, &[]).unwrap_err().code,
            code,
            "{sql}"
        );
    }
    // Preparation rejects the same forms the parser does, before any execution.
    assert_eq!(
        engine
            .prepare_sql("INSERT INTO kv VALUES ($1, 'a', 0) ON CONFLICT DO UPDATE SET v = $2")
            .unwrap_err()
            .code,
        "INVALID_QUERY"
    );
}
