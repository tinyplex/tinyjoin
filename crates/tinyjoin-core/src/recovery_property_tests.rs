//! Deterministic model checks across real SQL, index, overflow, and pager boundaries.
//! The fault device models both flush durability and partial persistence on failed I/O.
//!
//! Reproduce with `node scripts/cargo.mjs test -p tinyjoin-core recovery_property_tests -- --nocapture`.

use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

use serde_json::json;

use crate::{
    EngineError, FIRST_DATA_PAGE_ID, PAGE_SIZE, PageDevice, PageId, PagedEngine, Result, Row,
};

#[derive(Clone, Copy, Debug)]
enum Cut {
    Before,
    After,
    PartialDurable,
}

#[derive(Clone, Copy, Debug)]
enum Io {
    Write(PageId),
    Flush,
}

struct DeviceState {
    working: Vec<[u8; PAGE_SIZE]>,
    durable: Vec<[u8; PAGE_SIZE]>,
    trace: Vec<Io>,
    cut: Option<(usize, Cut)>,
    fired: bool,
    salt: u64,
}

#[derive(Clone)]
struct FaultDevice(Rc<RefCell<DeviceState>>);

impl FaultDevice {
    fn from_pages(pages: Vec<[u8; PAGE_SIZE]>, salt: u64) -> Self {
        Self(Rc::new(RefCell::new(DeviceState {
            working: pages.clone(),
            durable: pages,
            trace: Vec::new(),
            cut: None,
            fired: false,
            salt,
        })))
    }

    fn arm(&self, cut: Option<(usize, Cut)>) {
        let mut state = self.0.borrow_mut();
        state.trace.clear();
        state.cut = cut;
        state.fired = false;
    }

    fn crash(&self) {
        let mut state = self.0.borrow_mut();
        state.working = state.durable.clone();
        state.cut = None;
    }

    fn failure(state: &mut DeviceState, operation: Io) -> Option<Cut> {
        state.trace.push(operation);
        if let Some((at, cut)) = state.cut
            && at == state.trace.len()
        {
            state.cut = None;
            state.fired = true;
            Some(cut)
        } else {
            None
        }
    }
}

fn interrupted() -> EngineError {
    EngineError::new("INJECTED_IO", "deterministic interrupted storage operation")
}

impl PageDevice for FaultDevice {
    fn page_count(&self) -> PageId {
        self.0.borrow().working.len() as PageId
    }

    fn read_page(&mut self, id: PageId, destination: &mut [u8]) -> Result<()> {
        let state = self.0.borrow();
        let page = state.working.get(id as usize).ok_or_else(|| {
            EngineError::new("TEST_DEVICE", "attempted to read past the physical file")
        })?;
        assert_eq!(destination.len(), PAGE_SIZE);
        destination.copy_from_slice(page);
        Ok(())
    }

    fn write_page(&mut self, id: PageId, source: &[u8]) -> Result<()> {
        assert_eq!(source.len(), PAGE_SIZE);
        let mut state = self.0.borrow_mut();
        let cut = Self::failure(&mut state, Io::Write(id));
        if matches!(cut, Some(Cut::Before)) {
            return Err(interrupted());
        }
        let id = id as usize;
        assert!(id <= state.working.len(), "the engine must append densely");
        if id == state.working.len() {
            state.working.push([0; PAGE_SIZE]);
        }
        if matches!(cut, Some(Cut::PartialDurable)) {
            // Failed writes may modify and persist part of the target. Unlike a
            // flush-only fault model, this exposes overwrites of live old pages.
            let split = [1, PAGE_SIZE / 2, PAGE_SIZE - 1][state.salt as usize % 3];
            state.working[id][..split].copy_from_slice(&source[..split]);
            if state.durable.len() <= id {
                state.durable.resize(id + 1, [0; PAGE_SIZE]);
            }
            state.durable[id][..split].copy_from_slice(&source[..split]);
            return Err(interrupted());
        }
        state.working[id].copy_from_slice(source);
        if matches!(cut, Some(Cut::After)) {
            Err(interrupted())
        } else {
            Ok(())
        }
    }

    fn flush(&mut self) -> Result<()> {
        let mut state = self.0.borrow_mut();
        let cut = Self::failure(&mut state, Io::Flush);
        if matches!(cut, Some(Cut::Before)) {
            return Err(interrupted());
        }
        if matches!(cut, Some(Cut::PartialDurable)) {
            // A failed flush can persist a subset of pages, not necessarily a
            // prefix. Prior successful flushes remain durable.
            let length = state.working.len();
            state.durable.resize(length, [0; PAGE_SIZE]);
            for id in 0..length {
                if !(id as u64 + state.salt).is_multiple_of(3) {
                    state.durable[id] = state.working[id];
                }
            }
            return Err(interrupted());
        }
        state.durable = state.working.clone();
        if matches!(cut, Some(Cut::After)) {
            Err(interrupted())
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug)]
struct Item {
    bucket: Option<i64>,
    code: Option<String>,
    payload: String,
}

#[derive(Clone, Debug)]
struct Model {
    items: BTreeMap<i64, Item>,
    ledger: BTreeMap<i64, i64>,
}

impl Model {
    fn initial(seed: u64) -> Self {
        let mut random = seed;
        let items = (0..(8 + seed % 5) as i64)
            .map(|id| {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                (
                    id,
                    Item {
                        bucket: (id % 4 != 0).then_some((random % 3) as i64),
                        code: (id % 5 != 0).then(|| format!("code-{seed}-{id}")),
                        payload: format!(
                            "seed-{seed}-row-{id}-{}",
                            "x".repeat(if id % 3 == 0 {
                                5_000 + random as usize % 300
                            } else {
                                20
                            })
                        ),
                    },
                )
            })
            .collect();
        Self {
            items,
            ledger: BTreeMap::from([(0, seed as i64)]),
        }
    }

    fn rows(&self) -> Vec<Row> {
        self.items
            .iter()
            .map(|(id, item)| {
                json!({"id": id, "bucket": item.bucket, "code": item.code, "payload": item.payload})
                    .as_object()
                    .unwrap()
                    .clone()
            })
            .collect()
    }
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn insert(id: i64, item: &Item) -> String {
    format!(
        "INSERT INTO items VALUES ({id}, {}, {}, {})",
        item.bucket
            .map_or_else(|| "NULL".to_owned(), |value| value.to_string()),
        item.code
            .as_deref()
            .map_or_else(|| "NULL".to_owned(), quote),
        quote(&item.payload),
    )
}

fn mutation(seed: u64, model: &mut Model) -> Vec<String> {
    let first = (seed % model.items.len() as u64) as i64;
    let second = (first + 1) % model.items.len() as i64;
    let updated = (first + 2) % model.items.len() as i64;
    let moved = (first + 3) % model.items.len() as i64;
    model.items.remove(&first).unwrap();
    model.items.remove(&second).unwrap();
    let item = model.items.get_mut(&updated).unwrap();
    item.bucket = Some(3);
    item.code = Some(format!("changed-{seed}"));
    item.payload = format!("updated-{seed}-{}", "u".repeat(7_000));
    let mut statements = vec![
        format!("DELETE FROM items WHERE id IN ({first}, {second})"),
        format!(
            "UPDATE items SET bucket = 3, code = {}, payload = {} WHERE id = {updated}",
            quote(item.code.as_deref().unwrap()),
            quote(&item.payload)
        ),
    ];
    let item = model.items.remove(&moved).unwrap();
    let moved_to = 200 + seed as i64;
    model.items.insert(moved_to, item);
    statements.push(format!(
        "UPDATE items SET id = {moved_to} WHERE id = {moved}"
    ));
    let inserted = 100 + seed as i64;
    let item = Item {
        bucket: Some(2),
        code: Some(format!("inserted-{seed}")),
        payload: "n".repeat(6_000),
    };
    statements.push(insert(inserted, &item));
    model.items.insert(inserted, item);
    model.ledger.insert(0, seed as i64 * 7);
    model.ledger.insert(1, inserted);
    statements.push(format!(
        "UPDATE ledger SET amount = {} WHERE id = 0",
        seed * 7
    ));
    statements.push(format!("INSERT INTO ledger VALUES (1, {inserted})"));
    // A staged insertion followed by removal exercises net-zero changes mixed
    // with real changes, without making the oracle depend on engine results.
    statements.push(insert(
        999,
        &Item {
            bucket: None,
            code: None,
            payload: "transient".repeat(500),
        },
    ));
    statements.push("DELETE FROM items WHERE id = 999".to_owned());
    statements
}

fn assert_model(engine: &PagedEngine<FaultDevice>, model: &Model, codes: &[String], context: &str) {
    assert_eq!(
        engine
            .query_sql("SELECT * FROM items ORDER BY id", &[])
            .unwrap_or_else(|error| panic!("cannot read items: {context}: {error}"))
            .rows,
        model.rows(),
        "items differ: {context}"
    );
    let ledger = model
        .ledger
        .iter()
        .map(|(id, amount)| {
            json!({"id": id, "amount": amount})
                .as_object()
                .unwrap()
                .clone()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        engine
            .query_sql("SELECT * FROM ledger ORDER BY id", &[])
            .unwrap_or_else(|error| panic!("cannot read ledger: {context}: {error}"))
            .rows,
        ledger,
        "ledger differs: {context}"
    );
    // Exercise secondary lookup as well as the full scan, including values
    // removed/changed by the candidate so stale postings cannot hide in it.
    for bucket in 0..=3 {
        let expected = model
            .items
            .iter()
            .filter(|(_, item)| item.bucket == Some(bucket))
            .map(|(id, _)| json!({"id": id}).as_object().unwrap().clone())
            .collect::<Vec<_>>();
        assert_eq!(
            engine
                .query_sql(
                    "SELECT id FROM items WHERE bucket = $1 ORDER BY id",
                    &[json!(bucket)]
                )
                .unwrap_or_else(|error| {
                    panic!("cannot read bucket index {bucket}: {context}: {error}")
                })
                .rows,
            expected,
            "bucket index {bucket} differs: {context}"
        );
    }
    for code in codes {
        let expected = model
            .items
            .iter()
            .filter(|(_, item)| item.code.as_ref() == Some(code))
            .map(|(id, _)| json!({"id": id}).as_object().unwrap().clone())
            .collect::<Vec<_>>();
        assert_eq!(
            engine
                .query_sql(
                    "SELECT id FROM items WHERE code = $1 ORDER BY id",
                    &[json!(code)]
                )
                .unwrap_or_else(|error| {
                    panic!("cannot read code index {code}: {context}: {error}")
                })
                .rows,
            expected,
            "code index {code} differs: {context}"
        );
    }
}

#[derive(Clone, Copy, Debug)]
enum Execution {
    Transaction,
    Script,
}

fn execute(
    engine: &mut PagedEngine<FaultDevice>,
    mode: Execution,
    statements: &[String],
) -> Result<()> {
    match mode {
        Execution::Transaction => {
            engine.begin_transaction()?;
            for sql in statements {
                engine.execute_sql(sql, &[])?;
            }
            engine.commit_transaction().map(|_| ())
        }
        Execution::Script => engine.exec_sql(&statements.join(";")).map(|_| ()),
    }
}

fn check_recovery(mode: Execution) {
    let mut cases = 0;
    for seed in [3, 29] {
        let before = Model::initial(seed);
        let device = FaultDevice::from_pages(Vec::new(), seed);
        let mut engine = PagedEngine::open(device.clone()).unwrap();
        let mut setup = vec![
            "CREATE TABLE items (id INTEGER PRIMARY KEY, bucket INTEGER, code TEXT, payload TEXT NOT NULL)".to_owned(),
            "CREATE INDEX items_bucket ON items (bucket)".to_owned(),
            "CREATE UNIQUE INDEX items_code ON items (code)".to_owned(),
            "CREATE TABLE ledger (id INTEGER PRIMARY KEY, amount INTEGER NOT NULL)".to_owned(),
            format!("INSERT INTO ledger VALUES (0, {seed})"),
        ];
        setup.extend(before.items.iter().map(|(id, item)| insert(*id, item)));
        engine.exec_sql(&setup.join(";")).unwrap();
        // Prime multiple generations and freed overflow pages before faults;
        // recovery must also work when new candidates reuse the physical file.
        engine
            .execute_sql(
                "UPDATE items SET payload = $1 WHERE id = 0",
                &[json!("warm".repeat(1_700))],
            )
            .unwrap();
        engine
            .execute_sql(
                "UPDATE items SET payload = $1 WHERE id = 0",
                &[json!(before.items[&0].payload)],
            )
            .unwrap();
        let revision = engine.revision();
        drop(engine);
        let baseline = device.0.borrow().durable.clone();
        let mut after = before.clone();
        let statements = mutation(seed, &mut after);
        let codes = before
            .items
            .values()
            .chain(after.items.values())
            .filter_map(|item| item.code.clone())
            .collect::<Vec<_>>();

        let probe = FaultDevice::from_pages(baseline.clone(), seed);
        let mut engine = PagedEngine::open(probe.clone()).unwrap();
        probe.arm(None);
        execute(&mut engine, mode, &statements).unwrap();
        assert_eq!(engine.revision(), revision + 1);
        assert_model(
            &engine,
            &after,
            &codes,
            &format!("seed={seed}, mode={mode:?}, successful probe"),
        );
        let trace = probe.0.borrow().trace.clone();
        assert!(trace.iter().filter(|io| matches!(io, Io::Flush)).count() >= 3);
        assert!(trace.iter().any(|io| matches!(io, Io::Write(id) if *id >= FIRST_DATA_PAGE_ID && (*id as usize) < baseline.len())), "the candidate must exercise reused data pages");

        for operation in 1..=trace.len() {
            for cut in [Cut::Before, Cut::After, Cut::PartialDurable] {
                cases += 1;
                let context = format!(
                    "seed={seed}, mode={mode:?}, operation={operation}/{}, io={:?}, cut={cut:?}",
                    trace.len(),
                    trace[operation - 1]
                );
                let device = FaultDevice::from_pages(baseline.clone(), seed + operation as u64);
                let mut engine = PagedEngine::open(device.clone()).unwrap();
                device.arm(Some((operation, cut)));
                let outcome = execute(&mut engine, mode, &statements);
                assert!(device.0.borrow().fired, "fault not reached: {context}");
                assert!(
                    outcome.is_err(),
                    "interrupted operation was acknowledged: {context}"
                );
                drop(engine);
                device.crash();
                let mut recovered = PagedEngine::open(device.clone())
                    .unwrap_or_else(|error| panic!("cannot reopen {context}: {error}"));
                let recovered_revision = recovered.revision();
                assert!(
                    recovered_revision == revision || recovered_revision == revision + 1,
                    "unexpected revision {recovered_revision}: {context}"
                );
                let mut expected = if recovered_revision == revision {
                    before.clone()
                } else {
                    after.clone()
                };
                if outcome.unwrap_err().code != "RECOVERY_REQUIRED" {
                    assert_eq!(
                        recovered_revision, revision,
                        "definite prepublication failure published: {context}"
                    );
                }
                assert_model(&recovered, &expected, &codes, &context);
                // Confirm that recovery did not merely produce a readable but
                // unusable tree/allocation state. Continue, commit, and reopen.
                recovered
                    .execute_sql("INSERT INTO ledger VALUES (9, 123)", &[])
                    .unwrap_or_else(|error| panic!("cannot continue {context}: {error}"));
                expected.ledger.insert(9, 123);
                drop(recovered);
                device.crash();
                let reopened = PagedEngine::open(device).unwrap_or_else(|error| {
                    panic!("cannot reopen continuation {context}: {error}")
                });
                assert_eq!(reopened.revision(), recovered_revision + 1, "{context}");
                assert_model(
                    &reopened,
                    &expected,
                    &codes,
                    &format!("continuation: {context}"),
                );
            }
        }
    }
    eprintln!("{mode:?}: verified {cases} interrupted-publication/recovery cases");
}

#[test]
fn indexed_transaction_recovery_never_exposes_a_partial_model() {
    check_recovery(Execution::Transaction);
}

#[test]
fn multi_statement_script_recovery_never_exposes_a_partial_model() {
    check_recovery(Execution::Script);
}
