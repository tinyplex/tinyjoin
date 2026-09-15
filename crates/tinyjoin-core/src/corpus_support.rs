//! Test-only deterministic reduction and replay for SQL atomicity failures.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{ExecuteResult, MemoryPageDevice, PagedEngine, Result, Row};

type Database = PagedEngine<MemoryPageDevice>;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Case {
    pub name: String,
    pub transaction: bool,
    pub operations: Vec<Operation>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Operation {
    pub mode: String,
    pub sql: String,
    #[serde(default)]
    pub params: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Failure {
    invariant: &'static str,
    error_code: Option<String>,
}

fn rows(engine: &mut Database) -> Vec<Row> {
    engine
        .query_sql("SELECT * FROM items ORDER BY id", &[])
        .unwrap()
        .rows
}

fn catalog_exists(engine: &mut Database) -> bool {
    match engine.query_sql("SELECT id FROM audit_marker", &[]) {
        Ok(_) => true,
        Err(error) if error.code == "TABLE_NOT_FOUND" => false,
        Err(error) => panic!("unexpected catalog probe: {error}"),
    }
}

fn execute(engine: &mut Database, operation: &Operation) -> Result<()> {
    match operation.mode.as_str() {
        "parsers" => {
            let _ = crate::statement::parse(&operation.sql, &operation.params);
            let mut registry = crate::prepared_statement::PreparedStatementRegistry::default();
            if let Ok(id) = registry.prepare(&operation.sql) {
                let _ = registry.bind(id, &operation.params);
                registry.close(id).unwrap();
            }
            Ok(())
        }
        "script" => engine.exec_sql(&operation.sql).map(|_| ()),
        "prepared" => {
            let id = engine.prepare_sql(&operation.sql)?;
            let result: Result<ExecuteResult> = engine.execute_prepared(id, &operation.params);
            engine.close_prepared(id).unwrap();
            result.map(|_| ())
        }
        "direct" => engine
            .execute_sql(&operation.sql, &operation.params)
            .map(|_| ()),
        mode => panic!("unknown corpus operation mode {mode}"),
    }
}

pub(super) fn replay(case: &Case) -> std::result::Result<Vec<Option<String>>, Failure> {
    replay_with(case, execute)
}

fn replay_with(
    case: &Case,
    mut run: impl FnMut(&mut Database, &Operation) -> Result<()>,
) -> std::result::Result<Vec<Option<String>>, Failure> {
    let mut engine = PagedEngine::open(MemoryPageDevice::new(0).unwrap()).unwrap();
    engine.exec_sql("CREATE TABLE items (id INTEGER PRIMARY KEY, v TEXT); INSERT INTO items VALUES (1, 'base')").unwrap();
    if case.transaction {
        engine.begin_transaction().unwrap();
        engine
            .execute_sql("INSERT INTO items VALUES (3, 'staged')", &[])
            .unwrap();
    }
    let mut outcomes = Vec::new();
    for operation in &case.operations {
        let before = rows(&mut engine);
        let revision = engine.revision();
        let catalog = catalog_exists(&mut engine);
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut engine, operation)))
                .map_err(|_| Failure {
                    invariant: "execution_panic",
                    error_code: None,
                })?;
        let error_code = result.err().map(|error| error.code);
        if error_code.is_some() {
            let invariant = if rows(&mut engine) != before {
                Some("rows_after_failure")
            } else if engine.revision() != revision {
                Some("revision_after_failure")
            } else if engine.in_transaction() != case.transaction {
                Some("transaction_after_failure")
            } else if catalog_exists(&mut engine) != catalog {
                Some("catalog_after_failure")
            } else {
                None
            };
            if let Some(invariant) = invariant {
                return Err(Failure {
                    invariant,
                    error_code,
                });
            }
        }
        outcomes.push(error_code);
    }
    if case.transaction {
        engine.rollback_transaction().unwrap();
    }
    let before = rows(&mut engine);
    let catalog = catalog_exists(&mut engine);
    let mut reopened = PagedEngine::open(engine.into_device()).unwrap();
    if rows(&mut reopened) != before || catalog_exists(&mut reopened) != catalog {
        return Err(Failure {
            invariant: "state_after_reopen",
            error_code: None,
        });
    }
    Ok(outcomes)
}

/// Greedy deletion at successively smaller granularity, with an explicit oracle-call budget.
/// The failure signature includes the invariant and error code, so a syntax error cannot replace
/// the semantic failure being investigated. UTF-8, JSON, and operation ordering remain valid.
pub(super) fn minimize(
    original: &Case,
    budget: usize,
    mut oracle: impl FnMut(&Case) -> Option<Failure>,
) -> (Case, usize) {
    let target = oracle(original).expect("only failing cases can be minimized");
    let mut calls = 1;
    let mut current = original.clone();
    let mut accept = |candidate: &Case| {
        if calls >= budget.max(1) {
            return false;
        }
        calls += 1;
        oracle(candidate).as_ref() == Some(&target)
    };
    // Restart each successful reduction to remove dependencies exposed by earlier deletions.
    loop {
        let before = serde_json::to_string(&current).unwrap();
        let mut width = current.operations.len();
        while width > 0 {
            let mut index = 0;
            while index + width <= current.operations.len() {
                let mut candidate = current.clone();
                candidate.operations.drain(index..index + width);
                if accept(&candidate) {
                    current = candidate;
                } else {
                    index += 1;
                }
            }
            width /= 2;
        }
        for index in 0..current.operations.len() {
            let mut width = current.operations[index].sql.chars().count();
            while width > 0 {
                let mut start = 0;
                loop {
                    let boundaries = current.operations[index]
                        .sql
                        .char_indices()
                        .map(|(at, _)| at)
                        .chain(std::iter::once(current.operations[index].sql.len()))
                        .collect::<Vec<_>>();
                    if start + width >= boundaries.len() {
                        break;
                    }
                    let mut candidate = current.clone();
                    candidate.operations[index]
                        .sql
                        .replace_range(boundaries[start]..boundaries[start + width], "");
                    if accept(&candidate) {
                        current = candidate;
                    } else {
                        start += 1;
                    }
                }
                width /= 2;
            }
            // Remove trailing unused bind slots, then simplify each JSON parameter recursively.
            while !current.operations[index].params.is_empty() {
                let mut candidate = current.clone();
                candidate.operations[index].params.pop();
                if accept(&candidate) {
                    current = candidate;
                } else {
                    break;
                }
            }
            for parameter in 0..current.operations[index].params.len() {
                loop {
                    let mut changed = false;
                    for value in simpler_values(&current.operations[index].params[parameter]) {
                        let mut candidate = current.clone();
                        candidate.operations[index].params[parameter] = value;
                        if accept(&candidate) {
                            current = candidate;
                            changed = true;
                            break;
                        }
                    }
                    if !changed {
                        break;
                    }
                }
            }
        }
        if before == serde_json::to_string(&current).unwrap() {
            break;
        }
    }
    (current, calls)
}

fn simpler_values(value: &Value) -> Vec<Value> {
    let mut candidates = vec![Value::Null, json!(false), json!(0), json!("")];
    match value {
        Value::String(text) => {
            let chars = text.chars().collect::<Vec<_>>();
            for index in 0..chars.len() {
                candidates.push(Value::String(
                    chars
                        .iter()
                        .enumerate()
                        .filter_map(|(at, ch)| (at != index).then_some(ch))
                        .collect(),
                ));
            }
        }
        Value::Array(values) => {
            for index in 0..values.len() {
                let mut less = values.clone();
                less.remove(index);
                candidates.push(Value::Array(less));
                for value in simpler_values(&values[index]) {
                    let mut less = values.clone();
                    less[index] = value;
                    candidates.push(Value::Array(less));
                }
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                let mut less = values.clone();
                less.remove(key);
                candidates.push(Value::Object(less));
                for value in simpler_values(value) {
                    let mut less = values.clone();
                    less.insert(key.clone(), value);
                    candidates.push(Value::Object(less));
                }
            }
        }
        _ => {}
    }
    let original = serde_json::to_string(value).unwrap();
    candidates.retain(|value| {
        let encoded = serde_json::to_string(value).unwrap();
        (encoded.len(), &encoded) < (original.len(), &original)
    });
    candidates
}

pub(super) fn assert_case(case: &Case) -> Vec<Option<String>> {
    replay(case).unwrap_or_else(|failure| {
        let (minimal, calls) = minimize(case, 2048, |case| replay(case).err());
        panic!(
            "{failure:?}; reducer calls={calls}; replay fixture:\n{}",
            serde_json::to_string_pretty(&minimal).unwrap()
        );
    })
}

#[test]
fn retained_corpus_replays_without_partial_rows_catalog_or_commits() {
    #[derive(Deserialize)]
    struct Fixture {
        case: Case,
        errors: Vec<Option<String>>,
    }
    let fixtures: Vec<Fixture> =
        serde_json::from_str(include_str!("../corpus/atomicity.json")).unwrap();
    for fixture in fixtures {
        assert_eq!(
            assert_case(&fixture.case),
            fixture.errors,
            "{}",
            fixture.case.name
        );
    }
}

#[test]
fn reducer_preserves_injected_partial_commit_and_replays_deterministically() {
    // Deliberately broken test executor publishes a write after a failed statement. Production
    // code is never patched, and the minimized case is also replayed against the real engine.
    let broken = |case: &Case| {
        replay_with(case, |engine, operation| {
            let result = execute(engine, operation);
            if result
                .as_ref()
                .is_err_and(|error| error.code == "COLUMN_NOT_FOUND")
            {
                engine
                    .execute_sql("UPDATE items SET v = 'leaked' WHERE id = 1", &[])
                    .unwrap();
            }
            result
        })
        .err()
    };
    let case = Case {
        name: "reducer-oracle-self-test".to_owned(),
        transaction: false,
        operations: vec![
            Operation {
                mode: "direct".to_owned(),
                sql: "SELECT id FROM items".to_owned(),
                params: vec![],
            },
            Operation {
                mode: "direct".to_owned(),
                sql: "SELECT missing FROM items WHERE id = $1 /* é🦀 */".to_owned(),
                params: vec![json!(1), json!({"nested": [true, null, "noise"]})],
            },
        ],
    };
    let (minimal, calls) = minimize(&case, 2048, broken);
    let (again, same_calls) = minimize(&case, 2048, broken);
    assert!(calls <= 2048);
    assert_eq!(calls, same_calls);
    assert_eq!(
        serde_json::to_string(&minimal).unwrap(),
        serde_json::to_string(&again).unwrap()
    );
    assert_eq!(broken(&minimal), broken(&case));
    assert_eq!(minimal.operations.len(), 1);
    assert!(minimal.operations[0].sql.len() < case.operations[1].sql.len());
    assert!(minimal.operations[0].params.is_empty());
    assert_eq!(
        replay(&minimal).unwrap(),
        vec![Some("COLUMN_NOT_FOUND".to_owned())]
    );
    eprintln!(
        "reducer self-test calls={calls}: {}",
        serde_json::to_string(&minimal).unwrap()
    );
}

#[test]
fn reducer_simplifies_nested_parameters_without_changing_the_failure() {
    let case = Case {
        name: "parameter-reducer-self-test".to_owned(),
        transaction: true,
        operations: vec![Operation {
            mode: "direct".to_owned(),
            sql: "SELECT id FROM items".to_owned(),
            params: vec![
                json!({"unused": true, "nested": ["padding", {"keep": 42, "discard": [1, 2]}]}),
            ],
        }],
    };
    let oracle = |case: &Case| {
        case.operations
            .first()
            .filter(|operation| {
                operation
                    .params
                    .first()
                    .is_some_and(|value| value.to_string().contains("\"keep\""))
            })
            .map(|_| Failure {
                invariant: "injected_parameter_mismatch",
                error_code: None,
            })
    };
    let (minimal, calls) = minimize(&case, 512, oracle);
    assert!(calls <= 512);
    assert_eq!(oracle(&minimal), oracle(&case));
    assert_eq!(
        minimal.operations[0].params,
        vec![json!({"nested": [{"keep": 0}]})]
    );
}
