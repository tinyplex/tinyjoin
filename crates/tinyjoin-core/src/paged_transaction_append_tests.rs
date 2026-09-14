use serde_json::{Value, json};

use super::*;
use crate::{MemoryPageDevice, PagedEngine};

fn row(value: Value) -> Row {
    value.as_object().unwrap().clone()
}

fn storage() -> PagedStorage<MemoryPageDevice> {
    let mut storage = PagedStorage::open(MemoryPageDevice::new(0).unwrap()).unwrap();
    let script = "CREATE TABLE items (id FLOAT PRIMARY KEY, email TEXT, group_id INTEGER, payload TEXT);\
                  CREATE UNIQUE INDEX items_email ON items (email);\
                  CREATE UNIQUE INDEX items_group_email ON items (group_id, email);\
                  CREATE TABLE other (id FLOAT PRIMARY KEY, email TEXT, group_id INTEGER, payload TEXT);\
                  CREATE UNIQUE INDEX other_email ON other (email);\
                  INSERT INTO items VALUES (0, 'base', 0, 'committed');";
    let statements = crate::sql_script::split(script)
        .unwrap()
        .into_iter()
        .map(|sql| crate::statement::parse(sql, &[]).unwrap())
        .collect();
    storage.execute_script(statements).unwrap();
    storage
}

fn upsert(table: &str, id: Value, email: Value, payload: &str) -> RowChange {
    RowChange::Upsert {
        table: table.to_owned(),
        row: row(json!({"id": id, "email": email, "group_id": 1, "payload": payload})),
    }
}

fn delete(id: i64) -> RowChange {
    RowChange::Delete {
        table: "items".to_owned(),
        key: row(json!({"id": id})),
    }
}

fn compare_stage(
    storage: &PagedStorage<MemoryPageDevice>,
    fast: &mut PagedTransaction,
    fallback: &mut PagedTransaction,
    changes: Vec<RowChange>,
) -> Option<String> {
    let before = fast.changes();
    let touched = fast.touched_tables();
    let eligible = fast.append_validation.is_some();
    let actual = fast.stage(storage, changes.clone());
    let reference = fallback.stage(storage, changes);
    assert_eq!(
        actual.as_ref().err().map(|error| &error.code),
        reference.as_ref().err().map(|error| &error.code)
    );
    assert_eq!(fast.changes(), fallback.changes());
    assert_eq!(fast.touched_tables(), fallback.touched_tables());
    for table in ["items", "other"] {
        assert_eq!(
            PagedReadView::new(storage, Some(fast))
                .scan_table(table)
                .unwrap(),
            PagedReadView::new(storage, Some(fallback))
                .scan_table(table)
                .unwrap()
        );
    }
    if actual.is_err() {
        assert_eq!(fast.changes(), before);
        assert_eq!(fast.touched_tables(), touched);
        assert_eq!(fast.append_validation.is_some(), eligible);
    } else if let Some(cache) = &fast.append_validation {
        // The uncached validator is an independent oracle for all three retention estimates
        // and row/index/catalog operations, including catalog costs charged once per table.
        let full = storage
            .validate_row_write_set(&fast.changes(), None)
            .unwrap();
        assert_eq!(cache.usage, full.usage);
        assert_eq!(cache.unique_prefixes, full.unique_prefixes);
        let mut keys = 0;
        let mut bytes = 0;
        for (table, entries) in &fast.entries {
            for (key, entry) in entries {
                retain_entry(table, key, entry, &mut keys, &mut bytes).unwrap();
            }
        }
        assert_eq!(cache.overlay_keys, keys);
        assert_eq!(cache.overlay_bytes, bytes);
    }
    actual.err().map(|error| error.code)
}

#[test]
fn append_and_forced_fallback_have_identical_unique_and_mixed_statement_semantics() {
    let mut storage = storage();
    let mut fast = PagedTransaction::new(storage.revision());
    let mut fallback = PagedTransaction::new(storage.revision());
    fallback.append_validation = None;
    let appends = [
        vec![upsert("items", json!(1), json!("one"), "")],
        vec![
            upsert("items", json!(2), Value::Null, ""),
            upsert("items", json!(3), Value::Null, ""),
        ],
        // The same encoded prefix in a different unique index is independent.
        vec![upsert("other", json!(1), json!("one"), "")],
    ];
    for patch in appends {
        assert_eq!(
            compare_stage(&storage, &mut fast, &mut fallback, patch),
            None
        );
        assert!(fast.append_validation.is_some());
    }
    for patch in [
        vec![
            upsert("items", json!(4), json!("four"), ""),
            upsert("items", json!(5), json!("four"), ""),
        ],
        vec![upsert("items", json!(4), json!("one"), "")],
        vec![upsert("items", json!(4), json!("base"), "")],
        vec![
            upsert("items", json!(4), json!("four"), ""),
            upsert("items", json!(4.0), json!("different"), ""),
        ],
        // A failed update of an already staged key must not disable append eligibility.
        vec![upsert("items", json!(1), json!("base"), "")],
    ] {
        assert_eq!(
            compare_stage(&storage, &mut fast, &mut fallback, patch).as_deref(),
            Some("CONSTRAINT_VIOLATION")
        );
        assert!(fast.append_validation.is_some());
    }
    assert_eq!(
        compare_stage(
            &storage,
            &mut fast,
            &mut fallback,
            vec![upsert("items", json!(4), json!("four"), "")]
        ),
        None
    );
    assert!(fast.append_validation.is_some());

    // The first successful mixed patch drops the cache only after full validation succeeds.
    assert_eq!(
        compare_stage(
            &storage,
            &mut fast,
            &mut fallback,
            vec![delete(0), upsert("items", json!(5), json!("base"), "")]
        ),
        None
    );
    assert!(fast.append_validation.is_none());
    for patch in [
        vec![upsert("items", json!(1), json!("updated"), "")],
        vec![upsert("items", json!(6), json!("one"), "")],
        vec![delete(2)],
        vec![upsert("items", json!(2), json!("two"), "")],
        vec![upsert("items", json!(9), json!("temporary"), "")],
        vec![delete(9)],
    ] {
        assert_eq!(
            compare_stage(&storage, &mut fast, &mut fallback, patch),
            None
        );
        assert!(fast.append_validation.is_none());
    }
    let expected = PagedReadView::new(&storage, Some(&fast))
        .scan_table("items")
        .unwrap();
    storage
        .commit_transaction_changes(&fast.changes(), fast.touched_tables())
        .unwrap();
    let reopened = PagedStorage::open(storage.into_device()).unwrap();
    assert_eq!(reopened.scan_table("items").unwrap(), expected);
}

#[test]
fn append_budget_failures_match_full_validation_and_do_not_consume_capacity() {
    let storage = storage();
    let mut fast = PagedTransaction::new(storage.revision());
    let mut fallback = PagedTransaction::new(storage.revision());
    fallback.append_validation = None;
    let large = "x".repeat(500_000);
    let mut failed_id = None;
    for id in 1..20 {
        let failure = compare_stage(
            &storage,
            &mut fast,
            &mut fallback,
            vec![upsert(
                "items",
                json!(id),
                json!(format!("email-{id}")),
                &large,
            )],
        );
        if let Some(code) = failure {
            assert_eq!(code, "TRANSACTION_TOO_LARGE");
            failed_id = Some(id);
            break;
        }
        assert!(fast.append_validation.is_some());
    }
    let id = failed_id.expect("individually valid rows must reach a cumulative byte limit");
    assert!(id > 2);
    assert!(fast.append_validation.is_some());
    // Retry the rejected row key and unique prefix with a small value: neither was reserved.
    assert_eq!(
        compare_stage(
            &storage,
            &mut fast,
            &mut fallback,
            vec![upsert(
                "items",
                json!(id),
                json!(format!("email-{id}")),
                "small"
            )]
        ),
        None
    );
    assert!(fast.append_validation.is_some());
    assert_eq!(
        compare_stage(
            &storage,
            &mut fast,
            &mut fallback,
            vec![upsert(
                "items",
                json!(id + 1),
                json!(format!("email-{}", id + 1)),
                &large
            )]
        )
        .as_deref(),
        Some("TRANSACTION_TOO_LARGE")
    );
}

#[test]
fn transaction_script_savepoints_restore_append_claims_and_mixed_fallback_state() {
    let mut engine = PagedEngine::open(storage().into_device()).unwrap();
    engine.begin_transaction().unwrap();
    engine
        .execute_sql("INSERT INTO items VALUES (1, 'one', 1, 'prior')", &[])
        .unwrap();
    assert_eq!(engine.exec_sql("INSERT INTO items VALUES (2, 'two', 1, 'temporary'); INSERT INTO items VALUES (3, 'one', 1, 'conflict');").unwrap_err().code, "CONSTRAINT_VIOLATION");
    // The rolled-back script's prefix must be available again.
    engine
        .execute_sql("INSERT INTO items VALUES (2, 'two', 1, 'retained')", &[])
        .unwrap();
    assert_eq!(engine.exec_sql("UPDATE items SET email = 'changed' WHERE id = 1; INSERT INTO items VALUES (3, 'two', 1, 'conflict');").unwrap_err().code, "CONSTRAINT_VIOLATION");
    // A failed script that entered mixed-DML fallback must restore the original rows and claims.
    engine
        .execute_sql(
            "INSERT INTO items VALUES (3, 'changed', 1, 'after rollback')",
            &[],
        )
        .unwrap();
    engine.exec_sql("UPDATE items SET email = 'updated' WHERE id = 1; INSERT INTO items VALUES (4, 'one', 1, 'reused');").unwrap();
    engine
        .execute_sql(
            "INSERT INTO items VALUES (5, 'five', 1, 'after mixed script')",
            &[],
        )
        .unwrap();
    engine.commit_transaction().unwrap();
    let reopened = PagedEngine::open(engine.into_device()).unwrap();
    assert_eq!(
        reopened
            .query_sql("SELECT id, email FROM items ORDER BY id", &[])
            .unwrap()
            .rows,
        vec![
            row(json!({"id": 0, "email": "base"})),
            row(json!({"id": 1, "email": "updated"})),
            row(json!({"id": 2, "email": "two"})),
            row(json!({"id": 3, "email": "changed"})),
            row(json!({"id": 4, "email": "one"})),
            row(json!({"id": 5, "email": "five"})),
        ]
    );
}
