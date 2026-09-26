//! Native engine benchmark, run with `npm run bench:engine -- [workload ...]`.
//!
//! This mirrors the browser comparison workloads in `benchmarks/compare`, but drives the public
//! `PagedEngine` API directly over an in-memory page device, so an engine change can be measured
//! in seconds instead of a browser run. Every workload reports the best time of several runs and
//! its heap allocations, which are deterministic and make a sensitive regression signal.
//!
//! Setup is untimed. Each run starts from a fresh copy of the same device, and opening it is untimed
//! too, except in the workloads that measure opening.

use serde_json::{Value, json};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tinyjoin_core::{PAGE_SIZE, PageDevice, PageId, PagedEngine, Result};

struct CountingAllocator;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

// SAFETY: every call is forwarded unchanged to the system allocator.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// Instructions this process has retired. Unlike wall or CPU time, the count barely changes when
/// other work shares the machine or the scheduler moves between core types, so it is the fairer
/// comparison between two builds. It is available from the macOS kernel; elsewhere it reads zero.
#[cfg(target_os = "macos")]
fn instructions() -> u64 {
    // `struct rusage_info_v4` from <sys/resource.h>: a UUID, 29 counters, then the instruction
    // and cycle counts, then 4 more counters.
    #[repr(C)]
    struct UsageInfo {
        uuid: [u8; 16],
        counters: [u64; 29],
        instructions: u64,
        cycles: u64,
        rest: [u64; 4],
    }
    unsafe extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut UsageInfo) -> i32;
    }
    let mut info = std::mem::MaybeUninit::<UsageInfo>::zeroed();
    // SAFETY: flavor 4 (RUSAGE_INFO_V4) fills a struct with the layout above.
    unsafe {
        proc_pid_rusage(std::process::id() as i32, 4, info.as_mut_ptr());
        info.assume_init().instructions
    }
}

#[cfg(not(target_os = "macos"))]
fn instructions() -> u64 {
    0
}

#[derive(Clone, Default)]
struct MemoryDevice(Vec<Box<[u8; PAGE_SIZE]>>);

impl PageDevice for MemoryDevice {
    fn page_count(&self) -> PageId {
        self.0.len() as PageId
    }

    fn read_page(&mut self, id: PageId, destination: &mut [u8]) -> Result<()> {
        destination.copy_from_slice(&self.0[id as usize][..]);
        Ok(())
    }

    fn write_page(&mut self, id: PageId, source: &[u8]) -> Result<()> {
        if id as usize == self.0.len() {
            self.0.push(Box::new([0; PAGE_SIZE]));
        }
        self.0[id as usize].copy_from_slice(source);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

type Engine = PagedEngine<MemoryDevice>;

const ROWS: u64 = 10_000;
const ORDERS: u64 = 5_000;
const TABLE: &str = "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, \
    b INTEGER NOT NULL, c TEXT NOT NULL, g INTEGER NOT NULL)";
const INSERT_ONE: &str = "INSERT INTO t (id, a, b, c, g) VALUES ($1, $2, $3, $4, $5)";

const ONES: [&str; 20] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];
const TENS: [&str; 10] = [
    "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];

/// English words for a number, as in the classic SQLite speed comparison.
fn words(number: u64) -> String {
    let rest = |value: u64, words: String| {
        if value == 0 {
            words
        } else {
            format!("{words} {}", self::words(value))
        }
    };
    match number {
        0..=19 => ONES[number as usize].into(),
        20..=99 => rest(number % 10, TENS[(number / 10) as usize].into()),
        100..=999 => rest(
            number % 100,
            format!("{} hundred", ONES[(number / 100) as usize]),
        ),
        _ => rest(number % 1000, format!("{} thousand", words(number / 1000))),
    }
}

/// A deterministic xorshift generator, so every run sees the same data.
struct Random(u64);

impl Random {
    fn next(&mut self, below: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % below
    }
}

fn row(id: u64, random: &mut Random) -> Vec<Value> {
    let b = random.next(100_000);
    vec![
        json!(id),
        json!(id - 1),
        json!(b),
        json!(words(b)),
        json!(random.next(100)),
    ]
}

fn open(device: MemoryDevice) -> Engine {
    PagedEngine::open(device).expect("the benchmark device opens")
}

/// A table of `ROWS` rows, loaded with 200-row statements in one transaction.
fn table(indexes: &str) -> MemoryDevice {
    let mut engine = open(MemoryDevice::default());
    engine.exec_sql(TABLE).unwrap();
    if !indexes.is_empty() {
        engine.exec_sql(indexes).unwrap();
    }
    let values = (0..200)
        .map(|row| {
            let first = row * 5;
            format!(
                "(${}, ${}, ${}, ${}, ${})",
                first + 1,
                first + 2,
                first + 3,
                first + 4,
                first + 5
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let insert = engine
        .prepare_sql(&format!("INSERT INTO t (id, a, b, c, g) VALUES {values}"))
        .unwrap();
    let mut random = Random(0x9e37_79b9_7f4a_7c15);
    engine.begin_transaction().unwrap();
    for batch in 0..ROWS / 200 {
        let params = (1..=200)
            .flat_map(|offset| row(batch * 200 + offset, &mut random))
            .collect::<Vec<_>>();
        engine.execute_prepared(insert, &params).unwrap();
    }
    engine.commit_transaction().unwrap();
    engine.into_device()
}

fn empty_table() -> MemoryDevice {
    let mut engine = open(MemoryDevice::default());
    engine.exec_sql(TABLE).unwrap();
    engine.into_device()
}

fn join_tables() -> MemoryDevice {
    let mut engine = open(MemoryDevice::default());
    engine
        .exec_sql(
            "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT NOT NULL); \
             CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL, \
             total INTEGER NOT NULL); CREATE INDEX orders_customer ON orders (customer_id)",
        )
        .unwrap();
    let customer = engine
        .prepare_sql("INSERT INTO customers (id, name) VALUES ($1, $2)")
        .unwrap();
    let order = engine
        .prepare_sql("INSERT INTO orders (id, customer_id, total) VALUES ($1, $2, $3)")
        .unwrap();
    let mut random = Random(7);
    engine.begin_transaction().unwrap();
    for id in 1..=100 {
        engine
            .execute_prepared(
                customer,
                &[json!(id), json!(format!("customer {}", words(id)))],
            )
            .unwrap();
    }
    for id in 1..=ORDERS {
        let (customer_id, total) = (1 + random.next(100), random.next(1000));
        engine
            .execute_prepared(order, &[json!(id), json!(customer_id), json!(total)])
            .unwrap();
    }
    engine.commit_transaction().unwrap();
    engine.into_device()
}

/// Distinct pseudo-random primary keys, so that no write revisits a row.
fn spread(count: u64) -> impl Iterator<Item = u64> {
    (0..count).map(|index| (index * 7919) % ROWS + 1)
}

fn prepared(engine: &mut Engine, sql: &str) -> tinyjoin_core::PreparedStatementId {
    engine.prepare_sql(sql).unwrap()
}

fn rows(engine: &mut Engine, id: tinyjoin_core::PreparedStatementId, params: &[Value]) -> usize {
    engine.execute_prepared(id, params).unwrap().rows.len()
}

fn in_transaction(engine: &mut Engine, run: impl FnOnce(&mut Engine)) {
    engine.begin_transaction().unwrap();
    run(engine);
    engine.commit_transaction().unwrap();
}

struct Workload {
    id: &'static str,
    label: &'static str,
    setup: fn() -> MemoryDevice,
    run: Run,
}

enum Run {
    /// Runs against an engine opened on the device before timing starts.
    Engine(fn(&mut Engine) -> usize),
    /// Runs on the device itself, so that opening it is timed.
    Device(fn(MemoryDevice) -> usize),
}

fn workloads() -> Vec<Workload> {
    vec![
        Workload {
            id: "insert-autocommit",
            label: "1,000 INSERTs, each committed alone",
            setup: empty_table,
            run: Run::Engine(|engine| {
                let insert = prepared(engine, INSERT_ONE);
                let mut random = Random(1);
                for id in 1..=1000 {
                    rows(engine, insert, &row(id, &mut random));
                }
                1000
            }),
        },
        Workload {
            id: "insert-transaction",
            label: "One transaction of 10,000 INSERTs",
            setup: empty_table,
            run: Run::Engine(|engine| {
                let insert = prepared(engine, INSERT_ONE);
                let mut random = Random(1);
                in_transaction(engine, |engine| {
                    for id in 1..=ROWS {
                        rows(engine, insert, &row(id, &mut random));
                    }
                });
                ROWS as usize
            }),
        },
        Workload {
            id: "insert-batch",
            label: "One transaction of 50 INSERTs, 200 rows each",
            setup: empty_table,
            run: Run::Device(|_| {
                table("");
                ROWS as usize
            }),
        },
        Workload {
            id: "select-pk",
            label: "1,000 SELECTs by primary key",
            setup: || table(""),
            run: Run::Engine(|engine| {
                let select = prepared(engine, "SELECT * FROM t WHERE id = $1");
                spread(1000)
                    .map(|id| rows(engine, select, &[json!(id)]))
                    .sum()
            }),
        },
        Workload {
            id: "select-scan",
            label: "100 range aggregates, no index",
            setup: || table(""),
            run: Run::Engine(|engine| {
                let select = prepared(
                    engine,
                    "SELECT count(*) AS n, avg(b) AS m FROM t WHERE b >= $1 AND b < $2",
                );
                (0..100)
                    .map(|i| rows(engine, select, &[json!(i * 100), json!(i * 100 + 1000)]))
                    .sum()
            }),
        },
        Workload {
            id: "select-like",
            label: "100 LIKE aggregates on a text column",
            setup: || table(""),
            run: Run::Engine(|engine| {
                let select = prepared(
                    engine,
                    "SELECT count(*) AS n, avg(b) AS m FROM t WHERE c LIKE $1",
                );
                (1..=100)
                    .map(|i| rows(engine, select, &[json!(format!("%{}%", words(i)))]))
                    .sum()
            }),
        },
        Workload {
            id: "select-indexed",
            label: "100 range aggregates, indexed column",
            setup: || table("CREATE INDEX t_b ON t (b)"),
            run: Run::Engine(|engine| {
                let select = prepared(
                    engine,
                    "SELECT count(*) AS n, avg(b) AS m FROM t WHERE b >= $1 AND b < $2",
                );
                (0..100)
                    .map(|i| rows(engine, select, &[json!(i * 1000), json!(i * 1000 + 100)]))
                    .sum()
            }),
        },
        Workload {
            id: "select-all",
            label: "Read all 10,000 rows in order",
            setup: || table(""),
            run: Run::Engine(|engine| {
                let select = prepared(engine, "SELECT * FROM t ORDER BY id");
                rows(engine, select, &[])
            }),
        },
        Workload {
            id: "order-limit",
            label: "100 SELECTs of the first 10 rows in order",
            setup: || table(""),
            run: Run::Engine(|engine| {
                let select = prepared(engine, "SELECT * FROM t ORDER BY id LIMIT 10");
                (0..100).map(|_| rows(engine, select, &[])).sum()
            }),
        },
        Workload {
            id: "count",
            label: "100 counts of 10,000 rows",
            setup: || table(""),
            run: Run::Engine(|engine| {
                let select = prepared(engine, "SELECT count(*) AS n FROM t");
                (0..100).map(|_| rows(engine, select, &[])).sum()
            }),
        },
        Workload {
            id: "group-by",
            label: "10 GROUP BY aggregates over 10,000 rows",
            setup: || table(""),
            run: Run::Engine(|engine| {
                let select = prepared(
                    engine,
                    "SELECT g, count(*) AS n, sum(b) AS s FROM t GROUP BY g ORDER BY g",
                );
                (0..10).map(|_| rows(engine, select, &[])).sum()
            }),
        },
        Workload {
            id: "join",
            label: "100 joins: one customer's orders from 5,000",
            setup: join_tables,
            run: Run::Engine(|engine| {
                let select = prepared(
                    engine,
                    "SELECT o.id AS id, c.name AS name, o.total AS total FROM customers AS c \
                     JOIN orders AS o ON o.customer_id = c.id WHERE c.id = $1 ORDER BY o.id",
                );
                (1..=100).map(|id| rows(engine, select, &[json!(id)])).sum()
            }),
        },
        Workload {
            id: "update-pk",
            label: "One transaction of 1,000 UPDATEs by primary key",
            setup: || table(""),
            run: Run::Engine(|engine| {
                let update = prepared(engine, "UPDATE t SET c = $1 WHERE id = $2");
                in_transaction(engine, |engine| {
                    for id in spread(1000) {
                        rows(engine, update, &[json!("updated"), json!(id)]);
                    }
                });
                1000
            }),
        },
        Workload {
            id: "update-scan",
            label: "One transaction of 100 range UPDATEs, no index",
            setup: || table(""),
            run: Run::Engine(|engine| {
                let update = prepared(engine, "UPDATE t SET c = $1 WHERE b >= $2 AND b < $3");
                in_transaction(engine, |engine| {
                    for i in 0..100 {
                        rows(
                            engine,
                            update,
                            &[json!("updated"), json!(i * 1000), json!(i * 1000 + 100)],
                        );
                    }
                });
                100
            }),
        },
        Workload {
            id: "upsert",
            label: "One transaction of 1,000 upserts, half of them new",
            setup: || table(""),
            run: Run::Engine(|engine| {
                let upsert = prepared(
                    engine,
                    &format!("{INSERT_ONE} ON CONFLICT (id) DO UPDATE SET c = EXCLUDED.c"),
                );
                let mut random = Random(3);
                in_transaction(engine, |engine| {
                    for id in ROWS - 499..=ROWS + 500 {
                        rows(engine, upsert, &row(id, &mut random));
                    }
                });
                1000
            }),
        },
        Workload {
            id: "delete-pk",
            label: "One transaction of 1,000 DELETEs by primary key",
            setup: || table(""),
            run: Run::Engine(|engine| {
                let delete = prepared(engine, "DELETE FROM t WHERE id = $1");
                in_transaction(engine, |engine| {
                    for id in spread(1000) {
                        rows(engine, delete, &[json!(id)]);
                    }
                });
                1000
            }),
        },
        Workload {
            id: "delete-like",
            label: "One DELETE matching a LIKE pattern",
            setup: || table(""),
            run: Run::Engine(|engine| {
                engine
                    .execute_sql("DELETE FROM t WHERE c LIKE $1", &[json!("%fifty%")])
                    .unwrap()
                    .row_count
            }),
        },
        Workload {
            id: "delete-range",
            label: "One DELETE of 8,000 rows by indexed range",
            setup: || table("CREATE INDEX t_a ON t (a)"),
            run: Run::Engine(|engine| {
                engine
                    .execute_sql(
                        "DELETE FROM t WHERE a >= $1 AND a < $2",
                        &[json!(1000), json!(9000)],
                    )
                    .unwrap()
                    .row_count
            }),
        },
        Workload {
            id: "create-index",
            label: "Create two indexes over 10,000 rows",
            setup: || table(""),
            run: Run::Engine(|engine| {
                engine
                    .exec_sql("CREATE INDEX t_b ON t (b); CREATE INDEX t_c ON t (c)")
                    .unwrap()
                    .len()
            }),
        },
        Workload {
            id: "reopen",
            label: "Reopen a 10,000-row database",
            setup: || table(""),
            run: Run::Device(|device| {
                open(device);
                1
            }),
        },
        Workload {
            id: "reopen-indexed",
            label: "Reopen a 10,000-row database with one index",
            setup: || table("CREATE INDEX t_b ON t (b)"),
            run: Run::Device(|device| {
                open(device);
                1
            }),
        },
    ]
}

fn main() {
    let mut runs = 3;
    let mut json_output = false;
    let mut selected = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--runs" => {
                runs = args
                    .next()
                    .and_then(|runs| runs.parse().ok())
                    .unwrap_or(runs)
            }
            "--json" => json_output = true,
            _ => selected.push(arg),
        }
    }
    let all = workloads();
    for id in &selected {
        assert!(
            all.iter().any(|workload| workload.id == id),
            "Unknown workload: {id}"
        );
    }
    let mut results = Vec::new();
    for workload in all
        .iter()
        .filter(|workload| selected.is_empty() || selected.iter().any(|id| id == workload.id))
    {
        let device = (workload.setup)();
        let mut best = f64::INFINITY;
        let mut fewest_instructions = u64::MAX;
        let mut allocations = 0;
        let mut count = 0;
        for _ in 0..runs {
            let copy = device.clone();
            let mut engine = matches!(workload.run, Run::Engine(_)).then(|| open(copy.clone()));
            let before = ALLOCATIONS.load(Ordering::Relaxed);
            let retired = instructions();
            let start = Instant::now();
            count = match workload.run {
                Run::Engine(run) => run(engine.as_mut().expect("the engine was opened above")),
                Run::Device(run) => run(copy),
            };
            best = best.min(start.elapsed().as_secs_f64() * 1000.0);
            fewest_instructions = fewest_instructions.min(instructions() - retired);
            allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
        }
        if json_output {
            results.push(json!({
                "id": workload.id,
                "ms": best,
                "instructions": fewest_instructions,
                "allocations": allocations,
                "count": count,
            }));
        } else {
            println!(
                "{:<20} {:>10.2} ms {:>9.1}M instr {:>11} allocs  {}",
                workload.id,
                best,
                fewest_instructions as f64 / 1e6,
                allocations,
                workload.label
            );
        }
    }
    if json_output {
        println!("{}", Value::Array(results));
    }
}
