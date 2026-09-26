use serde_json::{Value, json};

use super::*;
use crate::paged_storage::UniquePrefixes;
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
    // SQL planning normalizes a FLOAT key to binary64 before staging it.
    let id = crate::storage::float_value(&id);
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

/// Stages a statement by validating the whole write set it leaves, as the reference for the
/// totals [`PagedTransaction::stage`] keeps.
fn stage_with_full_validation(
    storage: &PagedStorage<MemoryPageDevice>,
    transaction: &mut PagedTransaction,
    changes: Vec<RowChange>,
) -> Result<()> {
    transaction.ensure_base_revision(storage)?;
    if changes.is_empty() {
        return Ok(());
    }
    let patch = transaction.patch(storage, changes, Vec::new())?;
    let mut entries = transaction.entries.clone();
    for (table, patched) in &patch.entries {
        entries.entry(table.clone()).or_default().extend(
            patched
                .iter()
                .map(|(key, entry)| (key.clone(), entry.clone())),
        );
    }
    let (mut keys, mut bytes) = (0, 0);
    for (table, entries) in &entries {
        for (key, entry) in entries {
            retain_entry(&mut keys, &mut bytes, retained_bytes(table, key, entry)?)?;
        }
    }
    storage.validate_row_write_set(&changes_from_entries(entries.iter().flat_map(
        |(table, entries)| entries.values().map(move |entry| (table.as_str(), entry)),
    )))?;
    for (table, patched) in patch.entries {
        transaction.touched_tables.insert(table.clone());
        transaction
            .entries
            .entry(table)
            .or_default()
            .extend(patched);
    }
    Ok(())
}

fn compare_stage(
    storage: &PagedStorage<MemoryPageDevice>,
    staged: &mut PagedTransaction,
    reference: &mut PagedTransaction,
    changes: Vec<RowChange>,
) -> Option<String> {
    let before = staged.changes();
    let touched = staged.touched_tables();
    let actual = staged.stage(storage, changes.clone(), Vec::new());
    let expected = stage_with_full_validation(storage, reference, changes);
    assert_eq!(
        actual.as_ref().err().map(|error| &error.code),
        expected.as_ref().err().map(|error| &error.code)
    );
    assert_eq!(staged.changes(), reference.changes());
    assert_eq!(staged.touched_tables(), reference.touched_tables());
    for table in ["items", "other"] {
        assert_eq!(
            PagedReadView::new(storage, Some(staged))
                .scan_table(table)
                .unwrap(),
            PagedReadView::new(storage, Some(reference))
                .scan_table(table)
                .unwrap()
        );
    }
    if actual.is_err() {
        assert_eq!(staged.changes(), before);
        assert_eq!(staged.touched_tables(), touched);
    }
    // The whole-write-set validator is an independent oracle for every total the transaction keeps
    // statement by statement: the three retention estimates, row, index and catalog operations,
    // unique claims, and the overlay's own retention.
    let totals = &staged.totals;
    let full = storage.validate_row_write_set(&staged.changes()).unwrap();
    let usage = storage
        .write_set_usage(
            totals.usage,
            totals
                .changed_tables
                .iter()
                .filter(|(_, count)| **count > 0)
                .map(|(table, _)| table.as_str()),
        )
        .unwrap();
    assert_eq!(usage, full.usage);
    assert_eq!(
        totals.claims.keys().cloned().collect::<UniquePrefixes>(),
        full.unique_prefixes
    );
    let (mut keys, mut bytes) = (0, 0);
    for (table, entries) in &staged.entries {
        for (key, entry) in entries {
            keys += 1;
            bytes += retained_bytes(table, key, entry).unwrap();
        }
    }
    assert_eq!(totals.overlay_keys, keys);
    assert_eq!(totals.overlay_bytes, bytes);
    actual.err().map(|error| error.code)
}

#[test]
fn staged_totals_match_full_validation_for_unique_and_mixed_statements() {
    let mut storage = storage();
    let mut fast = PagedTransaction::new(storage.revision());
    let mut fallback = PagedTransaction::new(storage.revision());
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
        // A failed update of an already staged key must leave its claim in place.
        vec![upsert("items", json!(1), json!("base"), "")],
    ] {
        assert_eq!(
            compare_stage(&storage, &mut fast, &mut fallback, patch).as_deref(),
            Some("CONSTRAINT_VIOLATION")
        );
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

    // A delete releases the committed row's unique value for another row in the same statement.
    assert_eq!(
        compare_stage(
            &storage,
            &mut fast,
            &mut fallback,
            vec![delete(0), upsert("items", json!(5), json!("base"), "")]
        ),
        None
    );
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
    }
    let expected = PagedReadView::new(&storage, Some(&fast))
        .scan_table("items")
        .unwrap();
    storage.commit_transaction(&fast).unwrap();
    let reopened = PagedStorage::open(storage.into_device()).unwrap();
    assert_eq!(reopened.scan_table("items").unwrap(), expected);
}

#[test]
fn budget_failures_match_full_validation_and_do_not_consume_capacity() {
    let storage = storage();
    let mut fast = PagedTransaction::new(storage.revision());
    let mut fallback = PagedTransaction::new(storage.revision());
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
    }
    let id = failed_id.expect("individually valid rows must reach a cumulative byte limit");
    assert!(id > 2);
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
fn transaction_script_savepoints_restore_staged_rows_and_claims() {
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
    // A failed script with mixed changes must restore the original rows and claims.
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
            row(json!({"id": 0.0, "email": "base"})),
            row(json!({"id": 1.0, "email": "updated"})),
            row(json!({"id": 2.0, "email": "two"})),
            row(json!({"id": 3.0, "email": "changed"})),
            row(json!({"id": 4.0, "email": "one"})),
            row(json!({"id": 5.0, "email": "five"})),
        ]
    );
}

/// Generated sequences of inserts, updates, deletes and reverts over two unique indexes, including
/// swaps of unique values and rows changed back to their committed state, stage exactly as
/// validating the whole write set does.
#[test]
fn generated_mixed_statements_stage_as_full_validation_does() {
    struct Random(u64);
    impl Random {
        fn below(&mut self, bound: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % bound
        }
    }
    let mut storage = storage();
    let mut engine = PagedEngine::open(storage.into_device()).unwrap();
    engine
        .exec_sql(
            "INSERT INTO items VALUES (1, 'a', 1, 'p'), (2, 'b', 1, 'p'), (3, 'c', 2, 'p'), \
             (4, NULL, 2, 'p'), (5, 'e', NULL, 'p')",
        )
        .unwrap();
    storage = PagedStorage::open(engine.into_device()).unwrap();
    let emails = [
        json!("a"),
        json!("b"),
        json!("c"),
        json!("e"),
        json!("f"),
        Value::Null,
    ];
    let mut random = Random(0x5e7);
    let mut failures = 0;
    for _ in 0..200 {
        let mut staged = PagedTransaction::new(storage.revision());
        let mut reference = PagedTransaction::new(storage.revision());
        for _ in 0..12 {
            let changes = (0..1 + random.below(3))
                .map(|_| {
                    let id = 1 + random.below(7) as i64;
                    if random.below(4) == 0 {
                        delete(id)
                    } else {
                        let email = emails[random.below(emails.len())].clone();
                        let group = [json!(1), json!(2), Value::Null][random.below(3)].clone();
                        RowChange::Upsert {
                            table: "items".to_owned(),
                            row: row(json!({
                                "id": crate::storage::float_value(&json!(id)),
                                "email": email,
                                "group_id": group,
                                "payload": "p",
                            })),
                        }
                    }
                })
                .collect::<Vec<_>>();
            if compare_stage(&storage, &mut staged, &mut reference, changes).is_some() {
                failures += 1;
            }
        }
    }
    assert!(failures > 20, "only {failures} statements failed");
}
