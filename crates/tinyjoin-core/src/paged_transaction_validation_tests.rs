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

/// What an entry retains, found from the maps its rows decode to, independently of the estimates
/// staging makes as it encodes them.
fn decoded_retained_bytes(
    storage: &PagedStorage<MemoryPageDevice>,
    table: &str,
    key: &[u8],
    entry: &OverlayEntry,
) -> usize {
    let paged = storage.table(table).unwrap();
    let decoded = |value: &[u8]| {
        estimated_row_bytes(&paged.record(key, value).unwrap().to_row().unwrap()).unwrap()
    };
    let base_bytes = match &entry.row.old {
        None => 0,
        Some(HeldRow::Map(row)) => estimated_row_bytes(row).unwrap(),
        Some(HeldRow::Stored(stored)) => decoded(stored.value()),
    };
    let next_bytes = entry.row.next.as_deref().map_or(0, decoded);
    retained_bytes(table, key, base_bytes, next_bytes).unwrap()
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
            retain_entry(
                &mut keys,
                &mut bytes,
                decoded_retained_bytes(storage, table, key, entry),
            )?;
        }
    }
    storage.validate_row_write_set(&changes_from_entries(
        storage,
        entries.iter().flat_map(|(table, entries)| {
            entries
                .iter()
                .map(move |(key, entry)| (table.as_str(), key.as_slice(), entry))
        }),
    ))?;
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
    let before = staged.changes(storage);
    let touched = staged.touched_tables();
    let actual = staged.stage(storage, changes.clone(), Vec::new());
    let expected = stage_with_full_validation(storage, reference, changes);
    assert_eq!(
        actual.as_ref().err().map(|error| &error.code),
        expected.as_ref().err().map(|error| &error.code)
    );
    assert_eq!(staged.changes(storage), reference.changes(storage));
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
        assert_eq!(staged.changes(storage), before);
        assert_eq!(staged.touched_tables(), touched);
    }
    // The whole-write-set validator is an independent oracle for every total the transaction keeps
    // statement by statement: the three retention estimates, row, index and catalog operations,
    // unique claims, and the overlay's own retention.
    let totals = &staged.totals;
    let full = storage
        .validate_row_write_set(&staged.changes(storage))
        .unwrap();
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
    // A value a row gave up stays in the claims without a holder.
    assert_eq!(
        totals
            .claims
            .iter()
            .filter(|(_, holder)| holder.is_some())
            .map(|(slot, _)| slot.clone())
            .collect::<UniquePrefixes>(),
        full.unique_prefixes
    );
    let (mut keys, mut bytes) = (0, 0);
    for (table, entries) in &staged.entries {
        for (key, entry) in entries {
            keys += 1;
            bytes += decoded_retained_bytes(storage, table, key, entry);
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

/// Plans as a reader without record layouts does: every inserted row as a map.
struct MapsOnly<'a>(PagedReadView<'a, MemoryPageDevice>);

impl StorageReader for MapsOnly<'_> {
    fn charge_work(&self, operations: usize) -> Result<()> {
        self.0.charge_work(operations)
    }

    fn visit_table(
        &self,
        table: &str,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.0.visit_table(table, visitor)
    }

    fn table_row_count(&self, table: &str) -> Result<usize> {
        self.0.table_row_count(table)
    }

    fn lookup_primary_key(&self, table: &str, key: &Row) -> Result<Option<Row>> {
        self.0.lookup_primary_key(table, key)
    }

    fn visit_primary_key(
        &self,
        table: &str,
        key: &Row,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<VisitOutcome> {
        self.0.visit_primary_key(table, key, visitor)
    }

    fn index_definition(&self, name: &str) -> Option<IndexDefinition> {
        self.0.index_definition(name)
    }

    fn indexes_for_table(&self, table: &str) -> Result<Vec<IndexDefinition>> {
        self.0.indexes_for_table(table)
    }

    fn visit_index(
        &self,
        table: &str,
        columns: &[String],
        key: &Row,
        visitor: &mut dyn FnMut(&RowRef<'_>) -> Result<VisitControl>,
    ) -> Result<Option<VisitOutcome>> {
        self.0.visit_index(table, columns, key, visitor)
    }

    fn table_schema(&self, table: &str) -> Result<Rc<TableDefinition>> {
        self.0.table_schema(table)
    }

    fn revision(&self) -> u64 {
        self.0.revision()
    }
}

/// What a transaction stages and keeps, to compare two transactions entry by entry.
fn overlay_state(transaction: &PagedTransaction) -> String {
    let entries = transaction
        .entries
        .iter()
        .map(|(table, entries)| {
            let entries = entries
                .iter()
                .map(|(key, entry)| {
                    (
                        key,
                        &entry.row.old,
                        &entry.row.next,
                        entry.changed,
                        entry.retained,
                        &entry.cost,
                    )
                })
                .collect::<Vec<_>>();
            (table, entries)
        })
        .collect::<Vec<_>>();
    let totals = &transaction.totals;
    format!(
        "{entries:?} {} {} {:?} {:?} {:?} {:?}",
        totals.overlay_keys,
        totals.overlay_bytes,
        totals.usage,
        totals.changed_tables,
        totals.claims,
        transaction.touched_tables
    )
}

/// Generated INSERTs of valid and invalid values, defaults, respelled FLOATs, canonical key
/// aliases, JSON, and text past the key and row limits plan and stage as rows planned as stored
/// records exactly as they do as maps: the same errors, work, changes, entries, and totals.
#[test]
fn inserts_planned_as_records_match_inserts_planned_as_maps() {
    struct Random(u64);
    impl Random {
        fn below(&mut self, bound: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % bound
        }
        fn pick(&mut self, values: &[Value]) -> Value {
            values[self.below(values.len())].clone()
        }
    }
    let mut storage = PagedStorage::open(MemoryPageDevice::new(0).unwrap()).unwrap();
    let script = "CREATE TABLE wide (id INTEGER PRIMARY KEY, name TEXT NOT NULL, \
                  score FLOAT DEFAULT 1, flag BOOLEAN, doc JSON, note TEXT DEFAULT 'none');\
                  CREATE UNIQUE INDEX wide_name ON wide (name);\
                  CREATE INDEX wide_flag ON wide (flag, note);\
                  CREATE TABLE pairs (a TEXT, b FLOAT, v TEXT DEFAULT 'v', PRIMARY KEY (a, b));\
                  INSERT INTO wide (id, name) VALUES (1, 'one'), (2, 'two');\
                  INSERT INTO pairs (a, b) VALUES ('x', 1), ('y', 2.5);";
    let statements = crate::sql_script::split(script)
        .unwrap()
        .into_iter()
        .map(|sql| crate::statement::parse(sql, &[]).unwrap())
        .collect();
    storage.execute_script(statements).unwrap();

    fn long(length: usize, fill: char) -> Value {
        Value::String(fill.to_string().repeat(length))
    }
    // Mostly valid values, and now and then one the column cannot hold. Text past the key limit
    // fails in a key; long or escaped text and JSON arrays and objects are planned as maps.
    fn value(random: &mut Random, column: &str) -> Value {
        let invalid = random.below(24) == 0;
        match (column, invalid) {
            ("id", false) => json!(1 + random.below(300)),
            ("id", true) => random.pick(&[
                Value::Null,
                json!("7"),
                json!(1.5),
                json!(9_007_199_254_740_992_u64),
            ]),
            ("name", false) => match random.below(24) {
                0 => json!("c\u{0}\n\""),
                1 => long(200_000, 'r'),
                2 => long(150_000, '\u{1}'),
                3 => long(600_000, 'r'),
                _ => json!(format!("n{}", random.below(1_000))),
            },
            ("name", true) => random.pick(&[Value::Null, json!(7), long(1_100, 'k')]),
            ("score", false) => random.pick(&[
                json!(1),
                json!(1.0),
                json!(-0.0),
                json!(2.5),
                json!(1e300),
                json!(-9_007_199_254_740_993_i64),
                Value::Null,
            ]),
            ("score", true) => json!("x"),
            ("flag", false) => random.pick(&[json!(true), json!(false), Value::Null]),
            ("flag", true) => json!(1),
            ("doc", _) => random.pick(&[
                Value::Null,
                json!(1),
                json!("s"),
                json!(true),
                json!([1, "two"]),
                json!({"a": {"b": [null]}}),
            ]),
            ("note", false) => match random.below(12) {
                0 => long(600_000, 'n'),
                1 => Value::Null,
                _ => json!("n"),
            },
            ("note", true) => json!(false),
            ("a", false) => match random.below(6) {
                0 => json!("\u{0}"),
                1 => json!("x"),
                _ => json!(format!("a{}", random.below(50))),
            },
            ("a", true) => random.pick(&[long(1_030, 'a'), Value::Null]),
            ("b", false) => {
                let half = json!(random.below(4) as f64 / 2.0);
                random.pick(&[
                    json!(1),
                    json!(1.0),
                    json!(0),
                    json!(-0.0),
                    json!(0.0),
                    json!(2.5),
                    half,
                ])
            }
            ("b", true) => Value::Null,
            ("v", _) => random.pick(&[json!("w"), Value::Null]),
            _ => unreachable!("no column {column}"),
        }
    }
    let wide = ["id", "name", "score", "flag", "doc", "note"];
    let pairs = ["a", "b", "v"];

    let mut random = Random(0xd1ff);
    let (mut records, mut maps) = (0, 0);
    let (mut failures, mut staged_failures, mut unbound) = (0, 0, 0);
    for _ in 0..60 {
        let mut with_records = PagedTransaction::new(storage.revision());
        let mut with_maps = PagedTransaction::new(storage.revision());
        for _ in 0..10 {
            let (table, columns) = if random.below(3) == 0 {
                ("pairs", &pairs[..])
            } else {
                ("wide", &wide[..])
            };
            // Named columns in a random order, some left out for their defaults.
            let mut named = columns
                .iter()
                .filter(|_| random.below(10) != 0)
                .collect::<Vec<_>>();
            for index in (1..named.len()).rev() {
                named.swap(index, random.below(index + 1));
            }
            let mut params = Vec::new();
            let rows = (0..1 + random.below(3))
                .map(|_| {
                    let values = named
                        .iter()
                        .map(|column| {
                            if random.below(16) == 0 {
                                "DEFAULT".to_owned()
                            } else {
                                params.push(value(&mut random, column));
                                format!("${}", params.len())
                            }
                        })
                        .collect::<Vec<_>>();
                    format!("({})", values.join(", "))
                })
                .collect::<Vec<_>>();
            let sql = if named.is_empty() {
                format!("INSERT INTO {table} DEFAULT VALUES")
            } else {
                let names = named.iter().map(|name| **name).collect::<Vec<_>>();
                format!(
                    "INSERT INTO {table} ({}) VALUES {}",
                    names.join(", "),
                    rows.join(", ")
                )
            };
            // Parameters past their own limits fail to bind, before either planning.
            let statement = match crate::statement::parse(&sql, &params) {
                Ok(crate::statement::Statement::Write(statement)) => statement,
                Ok(_) => unreachable!("an INSERT is a write"),
                Err(error) => {
                    assert_eq!(error.code, "BIND_ERROR");
                    unbound += 1;
                    continue;
                }
            };

            let (records_work, maps_work) = (Cell::new(0), Cell::new(0));
            let planned_records = crate::statement::plan_dml(
                &PagedReadView::with_work_budget(&storage, Some(&with_records), &records_work),
                &statement,
            );
            let planned_maps = crate::statement::plan_dml(
                &MapsOnly(PagedReadView::with_work_budget(
                    &storage,
                    Some(&with_maps),
                    &maps_work,
                )),
                &statement,
            );
            assert_eq!(records_work.get(), maps_work.get(), "{sql}");
            let (planned_records, planned_maps) = match (planned_records, planned_maps) {
                (Err(left), Err(right)) => {
                    assert_eq!((left.code, left.message), (right.code, right.message));
                    failures += 1;
                    continue;
                }
                (Ok(left), Ok(right)) => (left, right),
                (left, right) => panic!(
                    "{sql}: {:?} vs {:?}",
                    left.err().map(|error| error.message),
                    right.err().map(|error| error.message)
                ),
            };
            assert_eq!(
                format!(
                    "{:?} {:?}",
                    planned_records.outcome, planned_records.previous
                ),
                format!("{:?} {:?}", planned_maps.outcome, planned_maps.previous)
            );
            // Each record decodes to the map the other planning normalized, under the key it
            // encodes to.
            let decoded = planned_records
                .changes
                .iter()
                .map(|change| match change {
                    RowChange::Put { table, key, record } => {
                        records += 1;
                        let paged = storage.table(table).unwrap();
                        let row = paged.record(key, record).unwrap().to_row().unwrap();
                        assert_eq!(*key, encode_primary_key(&paged.schema, &row).unwrap());
                        RowChange::Upsert {
                            table: table.clone(),
                            row,
                        }
                    }
                    other => {
                        maps += 1;
                        other.clone()
                    }
                })
                .collect::<Vec<_>>();
            assert_eq!(decoded, planned_maps.changes);

            let staged_records =
                with_records.stage(&storage, planned_records.changes, planned_records.previous);
            let staged_maps =
                with_maps.stage(&storage, planned_maps.changes, planned_maps.previous);
            if staged_records.is_err() {
                staged_failures += 1;
            }
            assert_eq!(
                staged_records.map_err(|error| (error.code, error.message)),
                staged_maps.map_err(|error| (error.code, error.message))
            );
            assert_eq!(overlay_state(&with_records), overlay_state(&with_maps));
        }
        // A transaction's view visits the rows it stages after the committed ones.
        let mut expected = PagedReadView::new(&storage, Some(&with_maps))
            .scan_table("wide")
            .unwrap();
        expected.sort_by_key(|row| row["id"].as_i64());
        storage.commit_transaction(&with_records).unwrap();
        assert_eq!(storage.scan_table("wide").unwrap(), expected);
    }
    assert!(
        records > 50 && maps > 5,
        "{records} records and {maps} maps"
    );
    assert!(
        failures > 50 && staged_failures > 5 && unbound < 40,
        "{failures} and {staged_failures} failed, {unbound} did not bind"
    );
}
