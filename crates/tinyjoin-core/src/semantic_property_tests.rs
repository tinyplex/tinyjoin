//! Bounded, reproducible checks against the public SQL semantics, across executor paths.
//! These tests deliberately use small row models, not the engine's predicate evaluator.
//! Run with `npm run cargo -- test -p tinyjoin-core semantic_property_tests`.
//! Failures identify their matrix case or fixed seed and mutated SQL for regression fixtures.

use serde_json::{Value, json};

use crate::{ExecuteResult, MemoryPageDevice, PagedEngine, Result, Row};

type Database = PagedEngine<MemoryPageDevice>;

fn row(value: Value) -> Row {
    value.as_object().unwrap().clone()
}

fn rows(engine: &mut Database) -> Vec<Row> {
    engine
        .query_sql("SELECT * FROM items ORDER BY id", &[])
        .unwrap()
        .rows
}

fn ids(rows: &[Row]) -> Vec<i64> {
    let mut ids = ordered_ids(rows);
    ids.sort_unstable();
    ids
}

fn ordered_ids(rows: &[Row]) -> Vec<i64> {
    rows.iter().map(|row| row["id"].as_i64().unwrap()).collect()
}

fn execute(
    engine: &mut Database,
    prepared: bool,
    sql: &str,
    params: &[Value],
) -> Result<ExecuteResult> {
    if prepared {
        let id = engine.prepare_sql(sql)?;
        let result = engine.execute_prepared(id, params);
        engine.close_prepared(id).unwrap();
        result
    } else {
        engine.execute_sql(sql, params)
    }
}

#[derive(Clone, Copy, Debug)]
enum Predicate {
    Equal,
    Unequal,
    InWithNull,
    NotInWithNull,
    EqualOrNull,
    UnmatchedAndEqual,
}

impl Predicate {
    fn sql(self, column: &str, id: &str) -> String {
        match self {
            Self::Equal => format!("{column} = $1"),
            Self::Unequal => format!("NOT ({column} = $1)"),
            Self::InWithNull => format!("{column} IN ($1, NULL)"),
            Self::NotInWithNull => format!("{column} NOT IN ($1, NULL)"),
            Self::EqualOrNull => format!("{column} = $1 OR {column} IS NULL"),
            Self::UnmatchedAndEqual => format!("{id} = -1 AND {column} = $1"),
        }
    }

    fn matches(self, value: &Value, parameter: &Value) -> bool {
        let comparable = !value.is_null() && !parameter.is_null();
        // Numeric parameters compare across INTEGER/FLOAT; nested JSON uses structural equality.
        let equal = if value.is_number() && parameter.is_number() {
            value.as_f64() == parameter.as_f64()
        } else {
            value == parameter
        };
        match self {
            Self::Equal | Self::InWithNull => comparable && equal,
            Self::Unequal => comparable && !equal,
            Self::EqualOrNull => value.is_null() || (comparable && equal),
            Self::NotInWithNull | Self::UnmatchedAndEqual => false,
        }
    }
}

#[test]
fn predicate_matrix_agrees_across_reads_writes_indexes_preparation_and_overlays() {
    let types = [
        ("BOOLEAN", vec![json!(false), json!(true)]),
        ("INTEGER", vec![json!(-1), json!(0), json!(1)]),
        ("FLOAT", vec![json!(-1.5), json!(0.0), json!(1.0)]),
        ("TEXT", vec![json!(""), json!("x"), json!("é🦀")]),
        (
            "JSON",
            vec![
                json!(false),
                json!(1),
                json!("x"),
                json!([]),
                json!({"a": [1, null]}),
            ],
        ),
    ];
    let parameters = [
        Value::Null,
        json!(false),
        json!(true),
        json!(0),
        json!(1.0),
        json!(0.5),
        json!("x"),
        json!("é🦀"),
        json!([]),
        json!({"a": [1, null]}),
    ];
    for (kind, values) in types {
        // FLOAT and JSON secondary indexes are outside the supported SQL contract.
        for indexed in [false, true] {
            if indexed && matches!(kind, "FLOAT" | "JSON") {
                continue;
            }
            let mut schema = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
            schema.execute_sql(
                &format!("CREATE TABLE items (id INTEGER PRIMARY KEY, v {kind}, marked BOOLEAN NOT NULL DEFAULT false)"), &[],
            ).unwrap();
            if indexed {
                schema
                    .execute_sql("CREATE INDEX items_v ON items (v)", &[])
                    .unwrap();
            }
            let empty = schema.into_device();
            let mut populated = PagedEngine::open(empty.clone()).unwrap();
            let mut all_values = vec![Value::Null];
            all_values.extend(values.iter().cloned());
            // Duplicate non-null values also exercise multi-row secondary-index postings.
            all_values.push(all_values[1].clone());
            for (id, value) in all_values.iter().enumerate() {
                populated
                    .execute_sql(
                        "INSERT INTO items (id, v) VALUES ($1, $2)",
                        &[json!(id), value.clone()],
                    )
                    .unwrap();
            }
            let populated = populated.into_device();
            for state in ["empty", "empty_transaction", "committed", "overlay"] {
                for parameter in &parameters {
                    let valid = parameter.is_null()
                        || match kind {
                            "BOOLEAN" => parameter.is_boolean(),
                            "INTEGER" | "FLOAT" => parameter.is_number(),
                            "TEXT" => parameter.is_string(),
                            "JSON" => true,
                            _ => unreachable!(),
                        };
                    for predicate in [
                        Predicate::Equal,
                        Predicate::Unequal,
                        Predicate::InWithNull,
                        Predicate::NotInWithNull,
                        Predicate::EqualOrNull,
                        Predicate::UnmatchedAndEqual,
                    ] {
                        for prepared in [false, true] {
                            let context = format!(
                                "{kind}, indexed={indexed}, {state}, {predicate:?}, parameter={parameter}, prepared={prepared}"
                            );
                            let mut engine = PagedEngine::open(if state.starts_with("empty") {
                                empty.clone()
                            } else {
                                populated.clone()
                            })
                            .unwrap();
                            let mut model = if state.starts_with("empty") {
                                Vec::new()
                            } else {
                                all_values
                                    .iter()
                                    .enumerate()
                                    .map(|(id, value)| {
                                        row(json!({"id": id, "v": value, "marked": false}))
                                    })
                                    .collect::<Vec<_>>()
                            };
                            let committed = model.clone();
                            if matches!(state, "overlay" | "empty_transaction") {
                                engine.begin_transaction().unwrap();
                            }
                            if state == "overlay" {
                                engine
                                    .execute_sql("DELETE FROM items WHERE id = 1", &[])
                                    .unwrap();
                                model.retain(|row| row["id"] != json!(1));
                                engine
                                    .execute_sql(
                                        "UPDATE items SET v = $1 WHERE id = 2",
                                        &[Value::Null],
                                    )
                                    .unwrap();
                                model
                                    .iter_mut()
                                    .find(|row| row["id"] == json!(2))
                                    .unwrap()
                                    .insert("v".to_owned(), Value::Null);
                                let value = &all_values[1];
                                engine
                                    .execute_sql(
                                        "INSERT INTO items (id, v) VALUES (99, $1)",
                                        std::slice::from_ref(value),
                                    )
                                    .unwrap();
                                model.push(row(json!({"id": 99, "v": value, "marked": false})));
                            }
                            let expected_ids = ids(&model
                                .iter()
                                .filter(|row| predicate.matches(&row["v"], parameter))
                                .cloned()
                                .collect::<Vec<_>>());
                            let filter = predicate.sql("v", "id");
                            let join_filter = predicate.sql("a.v", "a.id");
                            let statements = [
                                format!("SELECT id FROM items WHERE {filter} ORDER BY id"),
                                format!("SELECT COUNT(*) AS n FROM items WHERE {filter}"),
                                format!(
                                    "SELECT a.id AS id FROM items a JOIN items b ON a.id = b.id WHERE {join_filter} ORDER BY id"
                                ),
                                format!(
                                    "UPDATE items SET marked = true WHERE {filter} RETURNING id"
                                ),
                                format!("DELETE FROM items WHERE {filter} RETURNING id"),
                            ];
                            for (family, sql) in statements.iter().enumerate() {
                                let before = model.clone();
                                let revision = engine.revision();
                                let result = execute(
                                    &mut engine,
                                    prepared,
                                    sql,
                                    std::slice::from_ref(parameter),
                                );
                                if !valid {
                                    assert_eq!(
                                        result.unwrap_err().code,
                                        "TYPE_MISMATCH",
                                        "{context}: {sql}"
                                    );
                                    assert_eq!(
                                        rows(&mut engine),
                                        before,
                                        "failed statement changed rows: {context}: {sql}"
                                    );
                                    assert_eq!(engine.revision(), revision, "{context}: {sql}");
                                    continue;
                                }
                                let result = result
                                    .unwrap_or_else(|error| panic!("{context}: {sql}: {error}"));
                                if family == 1 {
                                    assert_eq!(
                                        result.rows,
                                        vec![row(json!({"n": expected_ids.len()}))],
                                        "{context}: {sql}"
                                    );
                                } else {
                                    let actual_ids = if family < 3 {
                                        ordered_ids(&result.rows)
                                    } else {
                                        ids(&result.rows)
                                    };
                                    assert_eq!(actual_ids, expected_ids, "{context}: {sql}");
                                    assert_eq!(
                                        result.row_count,
                                        expected_ids.len(),
                                        "{context}: {sql}"
                                    );
                                }
                                if family < 3 {
                                    assert_eq!(engine.revision(), revision, "{context}");
                                } else if family == 3 {
                                    for row in &mut model {
                                        if expected_ids.contains(&row["id"].as_i64().unwrap()) {
                                            row.insert("marked".to_owned(), json!(true));
                                        }
                                    }
                                } else {
                                    model.retain(|row| {
                                        !expected_ids.contains(&row["id"].as_i64().unwrap())
                                    });
                                }
                                assert_eq!(rows(&mut engine), model, "{context}: {sql}");
                            }
                            if engine.in_transaction() {
                                engine.rollback_transaction().unwrap();
                                model = committed;
                            }
                            let mut reopened = PagedEngine::open(engine.into_device()).unwrap();
                            assert_eq!(rows(&mut reopened), model, "reopen: {context}");
                        }
                    }
                }
            }
        }
    }
}

/// A filter's model result: `None` is SQL unknown, which a `WHERE` clause rejects like false.
type Truth = Option<bool>;

fn and(left: Truth, right: Truth) -> Truth {
    match (left, right) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

fn or(left: Truth, right: Truth) -> Truth {
    match (left, right) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

fn compare(value: &Value, bound: &Value) -> Option<std::cmp::Ordering> {
    match (value, bound) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Number(value), Value::Number(bound)) => value.as_f64().partial_cmp(&bound.as_f64()),
        // Rust string ordering is UTF-8 byte order, which is Unicode code-point order.
        (Value::String(value), Value::String(bound)) => Some(value.cmp(bound)),
        (Value::Bool(value), Value::Bool(bound)) => Some(value.cmp(bound)),
        _ => unreachable!("the matrix only binds parameters of the column's own type"),
    }
}

/// One `WHERE` template over column `v` and parameters `$1`, `$2`, ..., with its model.
struct Filter {
    sql: &'static str,
    matches: fn(&Value, &[Value]) -> Truth,
}

/// Checks each filter against a row model in every statement family, directly and prepared, with
/// and without a secondary index on the filtered column, inside a transaction that also has staged
/// changes. Every case rolls back, so each one starts from the same committed rows.
fn assert_filter_matrix(
    kind: &str,
    values: &[Value],
    filters: &[Filter],
    parameter_sets: &[Vec<Value>],
) {
    for indexed in [false, true] {
        if indexed && kind == "FLOAT" {
            continue;
        }
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .execute_sql(
                &format!("CREATE TABLE items (id INTEGER PRIMARY KEY, v {kind}, marked BOOLEAN NOT NULL DEFAULT false)"),
                &[],
            )
            .unwrap();
        if indexed {
            engine
                .execute_sql("CREATE INDEX items_v ON items (v)", &[])
                .unwrap();
        }
        let mut committed = Vec::new();
        for (id, value) in std::iter::once(&Value::Null)
            .chain(values)
            .chain(values.first())
            .enumerate()
        {
            engine
                .execute_sql(
                    "INSERT INTO items (id, v) VALUES ($1, $2)",
                    &[json!(id), value.clone()],
                )
                .unwrap();
            committed.push(row(json!({"id": id, "v": value, "marked": false})));
        }
        for filter in filters {
            let condition = filter.sql.replace("{v}", "v");
            let join_condition = filter.sql.replace("{v}", "a.v");
            let statements = [
                format!("SELECT id FROM items WHERE {condition} ORDER BY id"),
                format!("SELECT COUNT(*) AS n FROM items WHERE {condition}"),
                format!(
                    "SELECT a.id AS id FROM items a JOIN items b ON a.id = b.id WHERE {join_condition} ORDER BY id"
                ),
                format!("UPDATE items SET marked = true WHERE {condition} RETURNING id"),
                format!("DELETE FROM items WHERE {condition} RETURNING id"),
            ];
            for params in parameter_sets {
                for prepared in [false, true] {
                    let context = format!(
                        "{kind}, indexed={indexed}, {}, params={params:?}, prepared={prepared}",
                        filter.sql
                    );
                    engine.begin_transaction().unwrap();
                    // A staged row makes every family read through the transaction overlay.
                    let staged = values.last().unwrap();
                    engine
                        .execute_sql(
                            "INSERT INTO items (id, v) VALUES (99, $1)",
                            std::slice::from_ref(staged),
                        )
                        .unwrap();
                    let mut model = committed.clone();
                    model.push(row(json!({"id": 99, "v": staged, "marked": false})));
                    for (family, sql) in statements.iter().enumerate() {
                        let expected = ids(&model
                            .iter()
                            .filter(|row| (filter.matches)(&row["v"], params) == Some(true))
                            .cloned()
                            .collect::<Vec<_>>());
                        let result = execute(&mut engine, prepared, sql, params)
                            .unwrap_or_else(|error| panic!("{context}: {sql}: {error}"));
                        match family {
                            1 => assert_eq!(
                                result.rows,
                                vec![row(json!({"n": expected.len()}))],
                                "{context}: {sql}"
                            ),
                            0 | 2 => {
                                assert_eq!(ordered_ids(&result.rows), expected, "{context}: {sql}")
                            }
                            _ => assert_eq!(ids(&result.rows), expected, "{context}: {sql}"),
                        }
                        let matched = |row: &Row| expected.contains(&row["id"].as_i64().unwrap());
                        if family == 3 {
                            for row in model.iter_mut().filter(|row| matched(row)) {
                                row.insert("marked".to_owned(), json!(true));
                            }
                        } else if family == 4 {
                            model.retain(|row| !matched(row));
                        }
                        assert_eq!(rows(&mut engine), model, "{context}: {sql}");
                    }
                    engine.rollback_transaction().unwrap();
                    assert_eq!(rows(&mut engine), committed, "{context}");
                }
            }
        }
    }
}

#[test]
fn between_agrees_with_its_model_across_families_indexes_and_preparation() {
    fn between(value: &Value, params: &[Value]) -> Truth {
        and(
            compare(value, &params[0]).map(|ordering| ordering.is_ge()),
            compare(value, &params[1]).map(|ordering| ordering.is_le()),
        )
    }
    let filters = [
        Filter {
            sql: "{v} BETWEEN $1 AND $2",
            matches: between,
        },
        Filter {
            sql: "{v} NOT BETWEEN $1 AND $2",
            matches: |value, params| between(value, params).map(|truth| !truth),
        },
        Filter {
            sql: "NOT ({v} BETWEEN $1 AND $2) OR {v} IS NULL",
            matches: |value, params| {
                or(
                    between(value, params).map(|truth| !truth),
                    Some(value.is_null()),
                )
            },
        },
    ];
    for (kind, values) in [
        ("BOOLEAN", vec![json!(false), json!(true)]),
        ("INTEGER", vec![json!(-1), json!(0), json!(1)]),
        ("FLOAT", vec![json!(-1.5), json!(0.0), json!(1.0)]),
        ("TEXT", vec![json!(""), json!("x"), json!("é🦀")]),
    ] {
        let bounds = std::iter::once(Value::Null)
            .chain(values.iter().cloned())
            .collect::<Vec<_>>();
        let parameter_sets = bounds
            .iter()
            .flat_map(|low| bounds.iter().map(|high| vec![low.clone(), high.clone()]))
            .collect::<Vec<_>>();
        assert_filter_matrix(kind, &values, &filters, &parameter_sets);
    }
}

#[test]
fn distinct_agrees_with_sql_equality_across_executors_preparation_and_overlays() {
    // Each case holds values SQL considers equal to one another, including a float spelled as an
    // integer and both signs of zero, then values that are distinct from each other.
    for (kind, equal, others) in [
        (
            "BOOLEAN",
            vec![json!(true), json!(true)],
            vec![json!(false)],
        ),
        (
            "INTEGER",
            vec![json!(1), json!(1)],
            vec![json!(-1), json!(0)],
        ),
        (
            "FLOAT",
            vec![json!(0.0), json!(-0.0), json!(0)],
            vec![json!(1.5), json!(-1.5)],
        ),
        (
            "TEXT",
            vec![json!("é🦀"), json!("é🦀")],
            vec![json!(""), json!("E")],
        ),
    ] {
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .execute_sql(
                &format!("CREATE TABLE items (id INTEGER PRIMARY KEY, v {kind})"),
                &[],
            )
            .unwrap();
        let committed = [Value::Null, Value::Null]
            .into_iter()
            .chain(equal.iter().cloned())
            .chain(others.iter().cloned())
            .collect::<Vec<_>>();
        for (id, value) in committed.iter().enumerate() {
            engine
                .execute_sql(
                    "INSERT INTO items VALUES ($1, $2)",
                    &[json!(id), value.clone()],
                )
                .unwrap();
        }
        // NULL, the equal group, and each other value.
        let expected = 2 + others.len();
        for transaction in [false, true] {
            if transaction {
                engine.begin_transaction().unwrap();
                for (offset, value) in equal.iter().chain(&others).enumerate() {
                    engine
                        .execute_sql(
                            "INSERT INTO items VALUES ($1, $2)",
                            &[json!(100 + offset), value.clone()],
                        )
                        .unwrap();
                }
            }
            for sql in [
                "SELECT DISTINCT v FROM items ORDER BY v",
                "SELECT DISTINCT v AS value FROM items WHERE id >= $1",
                "SELECT DISTINCT a.v AS v FROM items a JOIN items b ON a.id = b.id WHERE a.id >= $1 ORDER BY v",
                "SELECT DISTINCT a.v AS v FROM items a LEFT JOIN items b ON a.id = b.id WHERE a.id >= $1",
            ] {
                let params = if sql.contains("$1") {
                    vec![json!(0)]
                } else {
                    vec![]
                };
                for prepared in [false, true] {
                    let context =
                        format!("{kind}, transaction={transaction}, prepared={prepared}: {sql}");
                    let result = execute(&mut engine, prepared, sql, &params)
                        .unwrap_or_else(|error| panic!("{context}: {error}"));
                    assert_eq!(result.rows.len(), expected, "{context}");
                    let values = result
                        .rows
                        .iter()
                        .map(|row| row.values().next().unwrap())
                        .collect::<Vec<_>>();
                    assert_eq!(
                        values.iter().filter(|value| value.is_null()).count(),
                        1,
                        "{context}"
                    );
                    for other in &others {
                        assert_eq!(
                            values
                                .iter()
                                .filter(|value| compare(value, other)
                                    .is_some_and(|ordering| ordering.is_eq()))
                                .count(),
                            1,
                            "{context}"
                        );
                    }
                }
            }
            if transaction {
                engine.rollback_transaction().unwrap();
            }
        }
    }
}

// Fixed integer arithmetic makes the generated corpus stable across platforms/toolchains.
struct Generator(u64);

impl Generator {
    fn pick(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 32) as usize) % bound
    }

    fn value(&mut self, depth: usize) -> Value {
        match self.pick(if depth == 0 { 5 } else { 7 }) {
            0 => Value::Null,
            1 => json!(self.pick(2) == 1),
            2 => json!(self.pick(17) as i64 - 8),
            3 => json!((self.pick(17) as f64 - 8.0) / 2.0),
            4 => json!(["", "'\";--$1", "é🦀", "\0\n\\"][self.pick(4)]),
            5 => Value::Array((0..self.pick(4)).map(|_| self.value(depth - 1)).collect()),
            _ => json!({"nested": self.value(depth - 1), "\u{0}tinyjoin:parameter": self.pick(5)}),
        }
    }
}

#[test]
fn generated_structured_parameters_remain_data_across_prepared_reuse_and_reopen() {
    // Actual singleton marker lookalikes are caller data, including when nested.
    let corpus = [
        json!({"\u{0}tinyjoin:parameter": 0}),
        json!({"\u{0}tinyjoin:parameter": 1}),
        json!({"\u{0}tinyjoin:parameter": 1025}),
        json!({"\u{0}tinyjoin:parameter": "1"}),
        json!({"nested": [{"\u{0}tinyjoin:parameter": 1}]}),
    ];
    for seed in [1, 0x5eed, 0xdead_beef] {
        let mut generator = Generator(seed);
        let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
        engine
            .execute_sql("CREATE TABLE items (id INTEGER PRIMARY KEY, v JSON)", &[])
            .unwrap();
        let insert = engine
            .prepare_sql("INSERT INTO items VALUES ($1, $2)")
            .unwrap();
        let select = engine
            .prepare_sql("SELECT v FROM items WHERE id = $1")
            .unwrap();
        let mut model = Vec::new();
        for id in 0..64 {
            let value = corpus
                .get(id as usize)
                .cloned()
                .unwrap_or_else(|| generator.value(4));
            let context = format!("seed={seed:#x}, case={id}, value={value}");
            let params = [json!(id), value.clone()];
            engine.begin_transaction().unwrap();
            if id % 2 == 0 {
                engine.execute_prepared(insert, &params).unwrap();
            } else {
                engine
                    .execute_sql("INSERT INTO items VALUES ($1, $2)", &params)
                    .unwrap();
            }
            assert_eq!(
                engine.execute_prepared(select, &[json!(id)]).unwrap().rows,
                vec![row(json!({"v": value}))],
                "{context}"
            );
            if id % 3 == 0 {
                engine.rollback_transaction().unwrap();
            } else {
                engine.commit_transaction().unwrap();
                model.push(row(json!({"id": id, "v": value})));
            }
            assert_eq!(rows(&mut engine), model, "{context}");
        }
        let mut reopened = PagedEngine::open(engine.into_device()).unwrap();
        assert_eq!(rows(&mut reopened), model, "seed={seed:#x}");
    }
}

#[test]
fn reused_predicate_plans_do_not_retain_previous_or_failed_bindings() {
    let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
    engine.exec_sql("CREATE TABLE items (id INTEGER PRIMARY KEY, v INTEGER, marked BOOLEAN DEFAULT false); CREATE INDEX items_v ON items (v); INSERT INTO items (id, v) VALUES (0, NULL), (1, 0), (2, 1)").unwrap();
    let original = rows(&mut engine);
    let sql = [
        "SELECT id FROM items WHERE v = $1 ORDER BY id",
        "SELECT COUNT(*) AS n FROM items WHERE v = $1",
        "SELECT a.id AS id FROM items a JOIN items b ON a.id = b.id WHERE a.v = $1 ORDER BY id",
        "UPDATE items SET marked = true WHERE v = $1 RETURNING id",
        "DELETE FROM items WHERE v = $1 RETURNING id",
    ];
    let plans = sql.map(|sql| engine.prepare_sql(sql).unwrap());
    for (parameter, expected) in [
        (json!(0), Some(vec![1])),
        (json!("wrong"), None),
        (Value::Null, Some(vec![])),
        (json!(1.0), Some(vec![2, 99])),
        (json!({"\u{0}tinyjoin:parameter": 1}), None),
        (json!(0.5), Some(vec![])),
        (json!(0), Some(vec![1])),
    ] {
        engine.begin_transaction().unwrap();
        engine
            .execute_sql("INSERT INTO items (id, v) VALUES (99, 1)", &[])
            .unwrap();
        for (family, plan) in plans.iter().enumerate() {
            let before = rows(&mut engine);
            let revision = engine.revision();
            let result = engine.execute_prepared(*plan, std::slice::from_ref(&parameter));
            let context = format!("parameter={parameter}, sql={}", sql[family]);
            if let Some(expected) = &expected {
                let result = result.unwrap_or_else(|error| panic!("{context}: {error}"));
                if family == 1 {
                    assert_eq!(
                        result.rows,
                        vec![row(json!({"n": expected.len()}))],
                        "{context}"
                    );
                } else {
                    assert_eq!(ids(&result.rows), *expected, "{context}");
                }
            } else {
                assert_eq!(result.unwrap_err().code, "TYPE_MISMATCH", "{context}");
                assert_eq!(rows(&mut engine), before, "{context}");
            }
            assert_eq!(engine.revision(), revision, "{context}");
            assert!(engine.in_transaction(), "{context}");
        }
        engine.rollback_transaction().unwrap();
        assert_eq!(rows(&mut engine), original);
    }
    for plan in plans {
        engine.close_prepared(plan).unwrap();
    }
}

#[test]
fn deterministic_sql_mutations_never_panic_or_partially_apply_failed_scripts() {
    let fragments = [
        "'", "\"", "$$", "$tag$", "$0", "$1", "$1025", ";", "--\n", "/*", "*/", "(", ")", ",",
        "é🦀", "\0", "1e999", "NULL", " AND ", " OR ",
    ];
    let corpus = [
        "SELECT id FROM items WHERE id = 1",
        "SELECT COUNT(*) AS n FROM items WHERE v = 'base'",
        "SELECT a.id FROM items a JOIN items b ON a.id = b.id",
        "INSERT INTO items VALUES (2, 'new')",
        "UPDATE items SET v = 'changed' WHERE id = 1",
        "DELETE FROM items WHERE id = 1",
        "CREATE TABLE other (id INTEGER PRIMARY KEY)",
        "INSERT INTO items VALUES (1, 'duplicate')",
        "SELECT id FROM items WHERE v = 1",
        "SELECT missing FROM items",
    ];
    for seed in [7, 0x5eed, 0xcafe_babe] {
        let mut generator = Generator(seed);
        let mut successes = 0;
        let mut syntax_errors = 0;
        let mut semantic_errors = 0;
        for case in 0..128 {
            let mut sql = corpus[if case < corpus.len() {
                case
            } else {
                generator.pick(corpus.len())
            }]
            .to_owned();
            let mutations = if case < corpus.len() {
                0
            } else {
                1 + generator.pick(4)
            };
            for _ in 0..mutations {
                let boundaries = sql
                    .char_indices()
                    .map(|(index, _)| index)
                    .chain(std::iter::once(sql.len()))
                    .collect::<Vec<_>>();
                let index = generator.pick(boundaries.len());
                let start = boundaries[index];
                if generator.pick(3) == 0 && index + 1 < boundaries.len() {
                    sql.replace_range(start..boundaries[index + 1], "");
                } else {
                    sql.insert_str(start, fragments[generator.pick(fragments.len())]);
                }
            }
            // Exercise the single-statement and prepared-layout parsers too: scripts
            // can reject a suffix in their splitter before either of these sees it.
            crate::corpus_support::assert_case(&crate::corpus_support::Case {
                name: format!("parser seed={seed:#x}, case={case}"),
                transaction: false,
                operations: [vec![], vec![json!({"nested": [true, null, "' ; $1"]})]]
                    .into_iter()
                    .map(|params| crate::corpus_support::Operation {
                        mode: "parsers".to_owned(),
                        sql: sql.clone(),
                        params,
                    })
                    .collect(),
            });
            for transaction in [false, true] {
                let catalog_prefix = if transaction {
                    ""
                } else {
                    "CREATE TABLE audit_marker (id INTEGER PRIMARY KEY); "
                };
                let case = crate::corpus_support::Case {
                    name: format!("seed={seed:#x}, case={case}"),
                    transaction,
                    operations: vec![crate::corpus_support::Operation {
                        mode: "script".to_owned(),
                        sql: format!("{catalog_prefix}UPDATE items SET v = 'prefix'; {sql}"),
                        params: vec![],
                    }],
                };
                for outcome in crate::corpus_support::assert_case(&case) {
                    match outcome.as_deref() {
                        None => successes += 1,
                        Some("SQL_PARSE_ERROR") => syntax_errors += 1,
                        Some(
                            "TYPE_MISMATCH"
                            | "CONSTRAINT_VIOLATION"
                            | "COLUMN_NOT_FOUND"
                            | "TABLE_NOT_FOUND",
                        ) => semantic_errors += 1,
                        _ => {}
                    }
                }
            }
        }
        assert!(
            successes > 0 && syntax_errors > 0 && semantic_errors > 0,
            "seed={seed:#x}: success={successes}, syntax={syntax_errors}, semantic={semantic_errors}"
        );
    }
}
