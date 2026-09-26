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
            // A prepared statement takes exactly as many parameters as the filter references.
            let arity = (1..=9)
                .filter(|index| filter.sql.contains(&format!("${index}")))
                .max()
                .unwrap_or(0);
            for model_params in parameter_sets {
                let params = &model_params[..arity];
                for prepared in [false, true] {
                    let context = format!(
                        "{kind}, indexed={indexed}, {}, params={model_params:?}, prepared={prepared}",
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
                            .filter(|row| (filter.matches)(&row["v"], model_params) == Some(true))
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

/// An exhaustive recursive `LIKE` model, deliberately unlike the engine's segment matcher.
fn like_model(text: &[char], pattern: &[char], escape: Option<char>, fold: bool) -> bool {
    let same = |left: char, right: char| {
        left == right || (fold && left.is_ascii() && left.eq_ignore_ascii_case(&right))
    };
    match pattern {
        [] => text.is_empty(),
        [first, literal, rest @ ..] if Some(*first) == escape => {
            matches!(text, [head, tail @ ..] if same(*head, *literal) && like_model(tail, rest, escape, fold))
        }
        ['%', rest @ ..] => {
            (0..=text.len()).any(|skip| like_model(&text[skip..], rest, escape, fold))
        }
        ['_', rest @ ..] => matches!(text, [_, tail @ ..] if like_model(tail, rest, escape, fold)),
        [literal, rest @ ..] => {
            matches!(text, [head, tail @ ..] if same(*head, *literal) && like_model(tail, rest, escape, fold))
        }
    }
}

/// The engine's `LIKE` matcher, including its literal-segment searches, against the model on
/// generated texts and patterns over characters that exercise wildcards, escapes, ASCII folding,
/// and multi-byte UTF-8.
#[test]
fn generated_like_patterns_agree_with_the_model() {
    let alphabet = ['a', 'b', 'A', 'B', 'é', 'É', '🦀', '%', '_', '\\', '#'];
    let mut generator = Generator(0x11ce);
    let string = |generator: &mut Generator, length: usize| {
        (0..generator.pick(length))
            .map(|_| alphabet[generator.pick(alphabet.len())])
            .collect::<String>()
    };
    for case in 0..20_000 {
        let text = string(&mut generator, 9);
        let mut pattern = string(&mut generator, 7);
        let escape = [None, Some('\\'), Some('#')][generator.pick(3)];
        // A pattern cannot end in its escape character.
        while escape.is_some() && pattern.ends_with(escape.unwrap()) {
            pattern.pop();
        }
        let fold = generator.pick(2) == 1;
        let chars = |value: &str| value.chars().collect::<Vec<_>>();
        assert_eq!(
            crate::query::like_matches(&text, &pattern, escape, fold),
            like_model(&chars(&text), &chars(&pattern), escape, fold),
            "case {case}: {text:?} LIKE {pattern:?} ESCAPE {escape:?}, fold {fold}"
        );
    }
}

fn like_truth(value: &Value, pattern: &Value, escape: Option<&Value>, fold: bool) -> Truth {
    let escape = match escape {
        None => Some('\\'),
        Some(Value::String(escape)) => escape.chars().next(),
        Some(_) => return None,
    };
    match (value, pattern) {
        (Value::String(text), Value::String(pattern)) => Some(like_model(
            &text.chars().collect::<Vec<_>>(),
            &pattern.chars().collect::<Vec<_>>(),
            escape,
            fold,
        )),
        _ => None,
    }
}

#[test]
fn like_agrees_with_its_model_across_families_indexes_and_preparation() {
    let filters = [
        Filter {
            sql: "{v} LIKE $1",
            matches: |value, params| like_truth(value, &params[0], None, false),
        },
        Filter {
            sql: "{v} NOT LIKE $1",
            matches: |value, params| like_truth(value, &params[0], None, false).map(|truth| !truth),
        },
        Filter {
            sql: "{v} ILIKE $1",
            matches: |value, params| like_truth(value, &params[0], None, true),
        },
        Filter {
            sql: "NOT ({v} ILIKE $1 ESCAPE $2) OR {v} IS NULL",
            matches: |value, params| {
                or(
                    like_truth(value, &params[0], Some(&params[1]), true).map(|truth| !truth),
                    Some(value.is_null()),
                )
            },
        },
    ];
    let values = ["", "x", "X_%", "é🦀", "Éx\\", "a!b%"].map(|value| json!(value));
    let patterns = [
        Value::Null,
        json!(""),
        json!("%"),
        json!("_"),
        json!("x"),
        json!("x%"),
        json!("%🦀"),
        json!("_É%"),
        json!("X\\_\\%"),
        json!("%!%%"),
        json!("%\\\\"),
        // A prefix and suffix that overlap in a short value, and a non-ASCII case difference.
        json!("x%x"),
        json!("é%"),
    ];
    let parameter_sets = patterns
        .iter()
        .flat_map(|pattern| {
            [Value::Null, json!(""), json!("!")]
                .into_iter()
                .map(move |escape| vec![pattern.clone(), escape])
        })
        .collect::<Vec<_>>();
    assert_filter_matrix("TEXT", &values, &filters, &parameter_sets);
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

/// The `ON CONFLICT` clauses the generated upsert test draws from, over `items (id, email, v)`
/// with a unique index on `email`.
#[derive(Clone, Copy, Debug)]
enum Upsert {
    NothingOnId,
    NothingOnEmail,
    NothingOnAny,
    UpdateValueOnId,
    UpdateValueOnEmail,
    UpdateEmailOnId,
    MoveEmailOnEmail,
}

/// The row-at-a-time behaviors a generated run must reach before its agreement means anything.
#[derive(Debug, Default)]
struct UpsertCoverage {
    inserted: usize,
    updated: usize,
    skipped_stored: usize,
    skipped_written: usize,
    updated_twice: usize,
    duplicate_key: usize,
    duplicate_email: usize,
    reused_moved_email: usize,
}

type ModelRow = (i64, Option<i64>, i64);

impl Upsert {
    const ALL: [Self; 7] = [
        Self::NothingOnId,
        Self::NothingOnEmail,
        Self::NothingOnAny,
        Self::UpdateValueOnId,
        Self::UpdateValueOnEmail,
        Self::UpdateEmailOnId,
        Self::MoveEmailOnEmail,
    ];

    fn sql(self) -> &'static str {
        match self {
            Self::NothingOnId => "ON CONFLICT (id) DO NOTHING",
            Self::NothingOnEmail => "ON CONFLICT (email) DO NOTHING",
            Self::NothingOnAny => "ON CONFLICT DO NOTHING",
            Self::UpdateValueOnId => "ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v",
            Self::UpdateValueOnEmail => "ON CONFLICT (email) DO UPDATE SET v = EXCLUDED.v",
            Self::UpdateEmailOnId => {
                "ON CONFLICT (id) DO UPDATE SET email = EXCLUDED.email, v = EXCLUDED.v"
            }
            // Moves the conflicting row to a different unique value, leaving its old one free.
            Self::MoveEmailOnEmail => "ON CONFLICT (email) DO UPDATE SET email = EXCLUDED.v",
        }
    }

    /// Applies one statement to a sorted model of `(id, email, v)` rows, inserting proposed rows
    /// one at a time as PostgreSQL does. Returns how many rows the statement reports, or `None`
    /// when it fails, in which case the model is unchanged.
    fn apply(
        self,
        model: &mut Vec<ModelRow>,
        proposed: &[ModelRow],
        coverage: &mut UpsertCoverage,
    ) -> Option<usize> {
        let (on_id, on_email, update) = match self {
            Self::NothingOnId => (true, false, false),
            Self::NothingOnEmail => (false, true, false),
            Self::NothingOnAny => (true, true, false),
            Self::UpdateValueOnId | Self::UpdateEmailOnId => (true, false, true),
            Self::UpdateValueOnEmail | Self::MoveEmailOnEmail => (false, true, true),
        };
        let mut next = model.clone();
        let mut written = Vec::new();
        let mut moved = Vec::new();
        let mut reported = 0;
        for &(id, email, v) in proposed {
            let id_conflict = on_id
                .then(|| next.iter().position(|row| row.0 == id))
                .flatten();
            let email_conflict = email
                .filter(|_| on_email)
                .and_then(|email| next.iter().position(|row| row.1 == Some(email)));
            match id_conflict.or(email_conflict) {
                Some(index) if !update => {
                    if written.contains(&next[index].0) {
                        coverage.skipped_written += 1;
                    } else {
                        coverage.skipped_stored += 1;
                    }
                    continue;
                }
                Some(index) => {
                    if written.contains(&next[index].0) {
                        coverage.updated_twice += 1;
                        return None;
                    }
                    match self {
                        Self::UpdateEmailOnId => next[index].1 = email,
                        Self::MoveEmailOnEmail => {
                            moved.extend(next[index].1);
                            next[index].1 = Some(v);
                        }
                        _ => {}
                    }
                    if !matches!(self, Self::MoveEmailOnEmail) {
                        next[index].2 = v;
                    }
                    written.push(next[index].0);
                    coverage.updated += 1;
                }
                None => {
                    if next.iter().any(|row| row.0 == id) {
                        coverage.duplicate_key += 1;
                        return None;
                    }
                    if email.is_some_and(|email| moved.contains(&email)) {
                        coverage.reused_moved_email += 1;
                    }
                    next.push((id, email, v));
                    written.push(id);
                    coverage.inserted += 1;
                }
            }
            reported += 1;
        }
        let emails = next.iter().filter_map(|row| row.1).collect::<Vec<_>>();
        if (1..emails.len()).any(|index| emails[..index].contains(&emails[index])) {
            coverage.duplicate_email += 1;
            return None;
        }
        next.sort_unstable();
        *model = next;
        Some(reported)
    }
}

#[test]
fn generated_upserts_agree_with_a_row_at_a_time_model() {
    let mut coverage = UpsertCoverage::default();
    for seed in [3, 0x5eed, 0xface_feed] {
        for transaction in [false, true] {
            let mut generator = Generator(seed);
            let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
            engine
                .exec_sql(
                    "CREATE TABLE items (id INTEGER PRIMARY KEY, email INTEGER, v INTEGER NOT NULL);\
                     CREATE UNIQUE INDEX items_email ON items (email)",
                )
                .unwrap();
            if transaction {
                engine.begin_transaction().unwrap();
            }
            let mut model: Vec<ModelRow> = Vec::new();
            for case in 0..400 {
                let context = format!("seed={seed:#x}, transaction={transaction}, case={case}");
                // Periodically free keys so that inserts and in-statement collisions stay common.
                if case % 8 == 7 {
                    let id = generator.pick(8) as i64;
                    engine
                        .execute_sql("DELETE FROM items WHERE id >= $1", &[json!(id)])
                        .unwrap();
                    model.retain(|row| row.0 < id);
                }
                let upsert = Upsert::ALL[generator.pick(Upsert::ALL.len())];
                let mut proposed: Vec<ModelRow> = Vec::new();
                for _ in 0..1 + generator.pick(3) {
                    let id = match proposed.last() {
                        Some(previous) if generator.pick(3) == 0 => previous.0,
                        _ => generator.pick(8) as i64,
                    };
                    let email = generator.pick(8);
                    proposed.push((
                        id,
                        (email < 5).then_some(email as i64),
                        generator.pick(6) as i64,
                    ));
                }
                let placeholders = (0..proposed.len())
                    .map(|row| format!("(${}, ${}, ${})", row * 3 + 1, row * 3 + 2, row * 3 + 3))
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    "INSERT INTO items VALUES {placeholders} {} RETURNING id",
                    upsert.sql()
                );
                let params = proposed
                    .iter()
                    .flat_map(|(id, email, v)| [json!(id), json!(email), json!(v)])
                    .collect::<Vec<_>>();
                let context = format!("{context}, {upsert:?}, rows={proposed:?}, model={model:?}");
                let expected = upsert.apply(&mut model, &proposed, &mut coverage);
                match (expected, execute(&mut engine, case % 2 == 1, &sql, &params)) {
                    (Some(reported), Ok(result)) => {
                        assert_eq!(result.row_count, reported, "{context}");
                        assert_eq!(result.rows.len(), reported, "{context}");
                    }
                    (None, Err(error)) => {
                        assert_eq!(error.code, "CONSTRAINT_VIOLATION", "{context}");
                    }
                    (expected, result) => {
                        panic!("{context}: expected {expected:?}, engine returned {result:?}")
                    }
                }
                let expected_rows = model
                    .iter()
                    .map(|(id, email, v)| row(json!({"id": id, "email": email, "v": v})))
                    .collect::<Vec<_>>();
                assert_eq!(rows(&mut engine), expected_rows, "{context}");
            }
            if transaction {
                engine.commit_transaction().unwrap();
            }
            let expected_rows = model
                .iter()
                .map(|(id, email, v)| row(json!({"id": id, "email": email, "v": v})))
                .collect::<Vec<_>>();
            let mut reopened = PagedEngine::open(engine.into_device()).unwrap();
            assert_eq!(rows(&mut reopened), expected_rows, "seed={seed:#x}");
        }
    }
    let UpsertCoverage {
        inserted,
        updated,
        skipped_stored,
        skipped_written,
        updated_twice,
        duplicate_key,
        duplicate_email,
        reused_moved_email,
    } = coverage;
    assert!(
        [
            inserted,
            updated,
            skipped_stored,
            skipped_written,
            updated_twice,
            duplicate_key,
            duplicate_email,
            reused_moved_email,
        ]
        .iter()
        .all(|count| *count > 0),
        "{coverage:?}"
    );
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

/// Range reads through secondary indexes and primary keys must return what a full scan returns,
/// in the same order, with aggregates bit for bit. `indexed` holds the same rows as `plain`, with
/// an index on every indexable column the predicates bound, so each generated predicate is answered
/// once through a range and once by scanning. The tables are large enough that a range covering a
/// small part of either is read through its index.
#[test]
fn generated_ranges_agree_with_full_scans() {
    const COLUMNS: &str = "id INTEGER PRIMARY KEY, i INTEGER, f FLOAT, t TEXT, b BOOLEAN, \
        n INTEGER NOT NULL";
    let texts = [
        "", "a", "ab", "abc", "abd", "ab%c", "b", "ba", "é", "🦀", "a\u{0}b", "\u{0}", "zz",
    ];
    let floats = [-1.5, -0.0, 0.0, 0.5, 2.0, 1e10];
    let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
    engine
        .exec_sql(&format!(
            "CREATE TABLE plain ({COLUMNS}); CREATE TABLE indexed ({COLUMNS}); \
             CREATE INDEX indexed_i ON indexed (i); CREATE INDEX indexed_t ON indexed (t); \
             CREATE INDEX indexed_b ON indexed (b); \
             CREATE INDEX indexed_n_i ON indexed (n, i); CREATE INDEX indexed_i_n ON indexed (i, n)"
        ))
        .unwrap();
    let mut generator = Generator(0x5eed);
    engine.begin_transaction().unwrap();
    for id in 1..=300 {
        let mut maybe = |value: Value| {
            if generator.pick(8) == 0 {
                Value::Null
            } else {
                value
            }
        };
        let row = [
            json!(id),
            maybe(json!(generator_value(id, 41) - 20)),
            maybe(json!(floats[id as usize % floats.len()])),
            maybe(json!(texts[(id as usize * 7) % texts.len()])),
            maybe(json!(id % 3 == 0)),
            json!(id % 10),
        ];
        for table in ["plain", "indexed"] {
            engine
                .execute_sql(
                    &format!("INSERT INTO {table} VALUES ($1, $2, $3, $4, $5, $6)"),
                    &row,
                )
                .unwrap();
        }
    }
    engine.commit_transaction().unwrap();

    let parameters = [
        json!(-3),
        json!(0),
        json!(2.5),
        json!(7.0),
        json!(-0.0),
        json!(1e20),
        json!(-1e20),
        json!(0.5),
        json!("ab"),
        json!("abd"),
        json!("é"),
        json!(true),
        Value::Null,
    ];
    let terms = [
        "i > $1",
        "i >= $1",
        "i < $1",
        "i <= $1",
        "i = $1",
        "i BETWEEN $1 AND $2",
        "f > $1",
        "f >= $1",
        "f < $1",
        "f <= $1",
        "f = $1",
        "f BETWEEN $1 AND $2",
        "t > $1",
        "t >= $1",
        "t < $1",
        "t <= $1",
        "t = $1",
        "t BETWEEN $1 AND $2",
        "t LIKE 'ab%'",
        "t LIKE 'a\\_b%'",
        "t LIKE 'é%'",
        "t LIKE '%b'",
        "t ILIKE 'AB%'",
        "b = $1",
        "n >= 7",
        "n = 3",
        "id > 290",
        "id <= 12",
        "id BETWEEN 100 AND 110",
        "i <> 3",
        "t IS NULL",
        "(i < 0 OR i > 10)",
        "i BETWEEN -3 AND -1",
        "i > 17",
        "i < -18",
        "i >= 2.5 AND i <= 4",
        "t > 'é'",
        "t < 'a'",
        "t LIKE 'abd%'",
        "t BETWEEN 'ab' AND 'abc'",
        "b = false AND i > 15",
    ];
    let generate = |generator: &mut Generator| {
        let condition = (0..1 + generator.pick(3))
            .map(|_| terms[generator.pick(terms.len())])
            .collect::<Vec<_>>()
            .join(" AND ");
        let arity = (1..=2)
            .filter(|index| condition.contains(&format!("${index}")))
            .max()
            .unwrap_or(0);
        let params = (0..arity)
            .map(|_| parameters[generator.pick(parameters.len())].clone())
            .collect::<Vec<_>>();
        (condition, params)
    };
    let compare_reads = |engine: &Database, case: usize, condition: &str, params: &[Value]| {
        let mut succeeded = 0;
        for statement in [
            "SELECT id FROM {table} WHERE {condition}",
            "SELECT count(*) AS c, sum(f) AS s, min(t) AS m FROM {table} WHERE {condition}",
            "SELECT id, t FROM {table} WHERE {condition} ORDER BY id LIMIT 5",
            "SELECT n, count(*) AS c FROM {table} WHERE {condition} GROUP BY n",
        ] {
            let sql = |table: &str| {
                statement
                    .replace("{table}", table)
                    .replace("{condition}", condition)
            };
            match (
                engine.query_sql(&sql("plain"), params),
                engine.query_sql(&sql("indexed"), params),
            ) {
                (Ok(plain), Ok(indexed)) => {
                    assert_eq!(
                        plain.rows,
                        indexed.rows,
                        "case {case}: {} {params:?}",
                        sql("?")
                    );
                    succeeded += 1;
                }
                (Err(plain), Err(indexed)) => {
                    assert_eq!(plain.code, indexed.code, "case {case}: {}", sql("?"))
                }
                (plain, indexed) => panic!(
                    "case {case}: {} {params:?}: {plain:?} but indexed {indexed:?}",
                    sql("?")
                ),
            }
        }
        succeeded
    };
    let mut checked = 0;
    for case in 0..250 {
        let (condition, params) = generate(&mut generator);
        checked += compare_reads(&engine, case, &condition, &params);
        // Writes narrow their rows the same way. Each is undone afterwards, row by row.
        let mut deleted = Vec::new();
        let mut updated = Vec::new();
        for table in ["plain", "indexed"] {
            let result = engine.execute_sql(
                &format!("DELETE FROM {table} WHERE {condition} RETURNING *"),
                &params,
            );
            let rows = result
                .as_ref()
                .map_or_else(|_| Vec::new(), |result| result.rows.clone());
            for row in &rows {
                let values = ["id", "i", "f", "t", "b", "n"].map(|column| row[column].clone());
                engine
                    .execute_sql(
                        &format!("INSERT INTO {table} VALUES ($1, $2, $3, $4, $5, $6)"),
                        &values,
                    )
                    .unwrap();
            }
            deleted.push(result.map(|_| ids(&rows)).map_err(|error| error.code));

            let result = engine.execute_sql(
                &format!("UPDATE {table} SET n = n + 1 WHERE {condition} RETURNING id"),
                &params,
            );
            let rows = result
                .as_ref()
                .map_or_else(|_| Vec::new(), |result| result.rows.clone());
            for row in &rows {
                engine
                    .execute_sql(
                        &format!("UPDATE {table} SET n = n - 1 WHERE id = $1"),
                        &[row["id"].clone()],
                    )
                    .unwrap();
            }
            updated.push(result.map(|_| ids(&rows)).map_err(|error| error.code));
        }
        assert_eq!(
            deleted[0], deleted[1],
            "case {case}: DELETE WHERE {condition} {params:?}"
        );
        assert_eq!(
            updated[0], updated[1],
            "case {case}: UPDATE WHERE {condition} {params:?}"
        );
    }
    assert!(checked > 500, "only {checked} statements succeeded");

    // Inside a transaction, a table it has not changed is still read through its ranges and
    // indexes, while one it has changed is scanned along with its staged rows.
    for staged in [false, true] {
        engine.begin_transaction().unwrap();
        if staged {
            for table in ["plain", "indexed"] {
                engine
                    .execute_sql(
                        &format!("INSERT INTO {table} VALUES (1000, 3, 0.5, 'abc', true, 3)"),
                        &[],
                    )
                    .unwrap();
            }
        }
        for case in 0..60 {
            let (condition, params) = generate(&mut generator);
            compare_reads(&engine, case, &condition, &params);
        }
        engine.rollback_transaction().unwrap();
    }
}

/// A fixed pseudo-random value in `0..bound` for row `id`.
fn generator_value(id: i64, bound: i64) -> i64 {
    (id * 7919 + id * id * 31) % bound
}

/// Queries ordered by a prefix of the primary key stream rows in key order, forward or backward,
/// and stop at their limit, while any other order is sorted. Both must return exactly the rows a
/// model sorts, with every combination of range, index and filter, inside and outside a
/// transaction.
#[test]
fn generated_key_orders_agree_with_sorting() {
    type Pair = (i64, String, Option<i64>);
    let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
    engine
        .exec_sql(
            "CREATE TABLE pairs (a INTEGER NOT NULL, b TEXT NOT NULL, v INTEGER, \
             PRIMARY KEY (a, b)); CREATE INDEX pairs_v ON pairs (v)",
        )
        .unwrap();
    let texts = ["", "a", "ab", "b", "é", "🦀", "a\u{0}"];
    let mut model = Vec::<Pair>::new();
    let mut generator = Generator(0x0bde);
    engine.begin_transaction().unwrap();
    for a in -6..30 {
        for b in texts {
            if generator.pick(3) == 0 {
                continue;
            }
            let v = (generator.pick(5) != 0).then(|| generator.pick(40) as i64);
            engine
                .execute_sql(
                    "INSERT INTO pairs VALUES ($1, $2, $3)",
                    &[json!(a), json!(b), json!(v)],
                )
                .unwrap();
            model.push((a, b.to_owned(), v));
        }
    }
    engine.commit_transaction().unwrap();

    type Term = (&'static str, fn(&Pair) -> bool);
    let terms: [Term; 12] = [
        ("a > 20", |row| row.0 > 20),
        ("a <= -2", |row| row.0 <= -2),
        ("a BETWEEN 3 AND 5", |row| (3..=5).contains(&row.0)),
        ("a = 7", |row| row.0 == 7),
        ("a >= 2.5", |row| row.0 >= 3),
        ("b >= 'b'", |row| row.1.as_str() >= "b"),
        ("b < 'ab'", |row| row.1.as_str() < "ab"),
        ("b LIKE 'a%'", |row| row.1.starts_with('a')),
        ("v = 3", |row| row.2 == Some(3)),
        ("v > 30", |row| row.2.is_some_and(|v| v > 30)),
        ("v IS NULL", |row| row.2.is_none()),
        ("a <> 4", |row| row.0 != 4),
    ];
    type Order = (&'static str, fn(&Pair, &Pair) -> std::cmp::Ordering);
    let orders: [Order; 7] = [
        ("a", |x, y| x.0.cmp(&y.0).then(x.1.cmp(&y.1))),
        ("a DESC", |x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1))),
        ("a, b", |x, y| (x.0, &x.1).cmp(&(y.0, &y.1))),
        ("a DESC, b DESC", |x, y| (y.0, &y.1).cmp(&(x.0, &x.1))),
        ("a ASC, b DESC", |x, y| x.0.cmp(&y.0).then(y.1.cmp(&x.1))),
        ("a DESC, b", |x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1))),
        ("b DESC, a", |x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0))),
    ];
    for staged in [false, true, false] {
        let in_transaction = staged || engine.in_transaction();
        if staged {
            engine.begin_transaction().unwrap();
            engine
                .execute_sql("INSERT INTO pairs VALUES (100, 'z', 1)", &[])
                .unwrap();
            model.push((100, "z".to_owned(), Some(1)));
        }
        for case in 0..300 {
            let chosen = (0..generator.pick(3))
                .map(|_| &terms[generator.pick(terms.len())])
                .collect::<Vec<_>>();
            let (order_sql, compare) = orders[generator.pick(orders.len())];
            let limit = [None, Some(0), Some(1), Some(5), Some(40)][generator.pick(5)];
            let offset = [0, 0, 1, 7][generator.pick(4)];
            let mut sql = String::from("SELECT a, b FROM pairs");
            if !chosen.is_empty() {
                let condition = chosen.iter().map(|(sql, _)| *sql).collect::<Vec<_>>();
                sql.push_str(&format!(" WHERE {}", condition.join(" AND ")));
            }
            sql.push_str(&format!(" ORDER BY {order_sql}"));
            if let Some(limit) = limit {
                sql.push_str(&format!(" LIMIT {limit}"));
            }
            if offset > 0 {
                sql.push_str(&format!(" OFFSET {offset}"));
            }
            let mut expected = model
                .iter()
                .filter(|row| chosen.iter().all(|(_, matches)| matches(row)))
                .collect::<Vec<_>>();
            // Rows equal on every ordering column keep ascending key order, as a stable sort of
            // rows read in key order leaves them.
            expected.sort_by(|x, y| compare(x, y));
            let expected = expected
                .into_iter()
                .skip(offset)
                .take(limit.unwrap_or(usize::MAX))
                .map(|row| self::row(json!({"a": row.0, "b": row.1})))
                .collect::<Vec<_>>();
            let actual = engine.query_sql(&sql, &[]).unwrap().rows;
            assert_eq!(
                actual, expected,
                "case {case}, transaction {in_transaction}: {sql}"
            );
        }
        if staged {
            engine.rollback_transaction().unwrap();
            model.pop();
            engine.begin_transaction().unwrap();
        }
    }
    engine.rollback_transaction().unwrap();
}
