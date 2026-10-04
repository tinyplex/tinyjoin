// Every workload runs against a fresh database with the same deterministic
// data. setup() is untimed; only run() is timed. check() is also untimed, and
// its result must be identical for every engine, or the runner reports it.
//
// Several workloads are adapted from the classic SQLite "speed comparison"
// suite that PGlite and wa-sqlite also publish results for. The adaptations
// give every table a primary key, replace SQL that TinyJoin did not support
// when they were written (arithmetic in SET) or still does not (INSERT ...
// SELECT), and use smaller counts.

export const ROWS = 10_000;
export const TABLE =
  'CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, ' +
  'b INTEGER NOT NULL, c TEXT NOT NULL, g INTEGER NOT NULL)';

// Kept at 5,000 orders across 100 customers, the size of the first published
// results, so that later results stay comparable with them.
const ORDERS = 5_000;

const COLUMNS = 5;
const BATCH = 200; // 1,000 parameters: within every engine's per-statement limit

// Numbers spelled out in English, as in the original suite, give the text
// column realistic, repetitive content for LIKE predicates.
const ONES = ['zero', 'one', 'two', 'three', 'four', 'five', 'six', 'seven',
  'eight', 'nine', 'ten', 'eleven', 'twelve', 'thirteen', 'fourteen',
  'fifteen', 'sixteen', 'seventeen', 'eighteen', 'nineteen'];
const TENS = ['', '', 'twenty', 'thirty', 'forty', 'fifty', 'sixty', 'seventy',
  'eighty', 'ninety'];
export const words = (number) => {
  if (number < 20) return ONES[number];
  if (number < 100) {
    return TENS[Math.floor(number / 10)] +
      (number % 10 ? ` ${ONES[number % 10]}` : '');
  }
  if (number < 1000) {
    return `${ONES[Math.floor(number / 100)]} hundred` +
      (number % 100 ? ` ${words(number % 100)}` : '');
  }
  return `${words(Math.floor(number / 1000))} thousand` +
    (number % 1000 ? ` ${words(number % 1000)}` : '');
};

const random = (seed) => () => {
  seed = (seed + 0x6d2b79f5) | 0;
  let value = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  value = (value + Math.imul(value ^ (value >>> 7), 61 | value)) ^ value;
  return ((value ^ (value >>> 14)) >>> 0) / 4294967296;
};

// [id, a, b, c, g]: a is sequential, b is random below 100,000, c spells b,
// and g places each row in one of 100 groups.
export const makeRows = (count, firstId = 1, seed = firstId) => {
  const next = random(seed);
  return Array.from({length: count}, (_, index) => {
    const id = firstId + index;
    const b = Math.floor(next() * 100_000);
    return [id, id - 1, b, words(b), Math.floor(next() * 100)];
  });
};

const placeholders = (rowCount) =>
  Array.from({length: rowCount}, (_, row) =>
    `(${Array.from({length: COLUMNS}, (_, column) => `$${row * COLUMNS + column + 1}`).join(', ')})`,
  ).join(', ');

const INSERT = 'INSERT INTO t (id, a, b, c, g) VALUES ';

// Untimed bulk load, using the fastest shape all three engines share.
export const insertRows = async (db, rows) => {
  if (rows.length % BATCH) throw new Error(`Seed rows must be a multiple of ${BATCH}`);
  const statement = await db.prepare(INSERT + placeholders(BATCH));
  await db.transaction(async (tx) => {
    for (let offset = 0; offset < rows.length; offset += BATCH) {
      await tx.run(statement, rows.slice(offset, offset + BATCH).flat());
    }
  });
};

const table = async (db, rowCount = ROWS, indexes = '') => {
  await db.exec(TABLE + (indexes ? `; ${indexes}` : ''));
  if (rowCount) await insertRows(db, makeRows(rowCount));
};

const summary = async (db) => {
  const [row] = await db.query('SELECT count(*) AS n, sum(a) AS a, sum(b) AS b FROM t');
  return {rows: Number(row.n), a: Number(row.a ?? 0), b: Number(row.b ?? 0)};
};

const countWhereC = async (db, c) =>
  Number((await db.query('SELECT count(*) AS n FROM t WHERE c = $1', [c]))[0].n);

// Distinct pseudo-random primary keys, so that no write revisits a row.
const spreadIds = (count) => Array.from({length: count}, (_, index) => ((index * 7919) % ROWS) + 1);

// Aggregate results are summed into one checked value. PGlite returns numeric
// averages as strings, and the engines round floats differently.
const addAggregate = (total, row) => ({
  n: total.n + Number(row.n),
  m: Math.round((total.m + Number(row.m ?? 0)) * 1000) / 1000,
});
const round = (value) => Math.round(value * 1000) / 1000;

export const workloads = [
  {
    id: 'insert-autocommit',
    group: 'Create',
    label: '1,000 INSERTs, each committed alone',
    derivedFrom: 1,
    setup: (db) => table(db, 0),
    async run(db) {
      const insert = await db.prepare(INSERT + placeholders(1));
      for (const row of makeRows(1000)) await db.run(insert, row);
    },
    check: summary,
  },
  {
    id: 'insert-transaction',
    group: 'Create',
    label: 'One transaction of 10,000 INSERTs',
    derivedFrom: 2,
    setup: (db) => table(db, 0),
    async run(db) {
      const insert = await db.prepare(INSERT + placeholders(1));
      await db.transaction(async (tx) => {
        for (const row of makeRows(ROWS)) await tx.run(insert, row);
      });
    },
    check: summary,
  },
  {
    id: 'insert-indexed',
    group: 'Create',
    label: 'One transaction of 10,000 INSERTs, indexed text column',
    derivedFrom: 3,
    setup: (db) => table(db, 0, 'CREATE INDEX t_c ON t (c)'),
    async run(db) {
      const insert = await db.prepare(INSERT + placeholders(1));
      await db.transaction(async (tx) => {
        for (const row of makeRows(ROWS)) await tx.run(insert, row);
      });
    },
    check: summary,
  },
  {
    id: 'insert-batch',
    group: 'Create',
    label: 'One transaction of 50 INSERTs, 200 rows each',
    setup: (db) => table(db, 0),
    run: (db) => insertRows(db, makeRows(ROWS)),
    check: summary,
  },
  {
    id: 'select-pk',
    group: 'Read',
    label: '1,000 SELECTs by primary key',
    setup: (db) => table(db),
    async run(db) {
      const select = await db.prepare('SELECT * FROM t WHERE id = $1');
      let total = 0;
      for (const id of spreadIds(1000)) total += (await db.run(select, [id]))[0].b;
      return total;
    },
    check: async (_db, total) => ({total}),
  },
  {
    id: 'select-scan',
    group: 'Read',
    label: '100 range aggregates, no index',
    derivedFrom: 4,
    setup: (db) => table(db),
    async run(db) {
      const select = await db.prepare('SELECT count(*) AS n, avg(b) AS m FROM t WHERE b >= $1 AND b < $2');
      let total = {n: 0, m: 0};
      for (let i = 0; i < 100; i++) total = addAggregate(total, (await db.run(select, [i * 100, i * 100 + 1000]))[0]);
      return total;
    },
    check: async (_db, total) => total,
  },
  {
    id: 'select-like',
    group: 'Read',
    label: '100 LIKE aggregates on a text column',
    derivedFrom: 5,
    setup: (db) => table(db),
    async run(db) {
      const select = await db.prepare('SELECT count(*) AS n, avg(b) AS m FROM t WHERE c LIKE $1');
      let total = {n: 0, m: 0};
      for (let i = 1; i <= 100; i++) total = addAggregate(total, (await db.run(select, [`%${words(i)}%`]))[0]);
      return total;
    },
    check: async (_db, total) => total,
  },
  {
    id: 'select-indexed',
    group: 'Read',
    label: '100 range aggregates, indexed column',
    derivedFrom: 7,
    setup: (db) => table(db, ROWS, 'CREATE INDEX t_b ON t (b)'),
    async run(db) {
      const select = await db.prepare('SELECT count(*) AS n, avg(b) AS m FROM t WHERE b >= $1 AND b < $2');
      let total = {n: 0, m: 0};
      for (let i = 0; i < 100; i++) total = addAggregate(total, (await db.run(select, [i * 1000, i * 1000 + 100]))[0]);
      return total;
    },
    check: async (_db, total) => total,
  },
  {
    id: 'select-all',
    group: 'Read',
    label: 'Read all 10,000 rows in order',
    setup: (db) => table(db),
    async run(db) {
      const rows = await db.query('SELECT * FROM t ORDER BY id');
      return {rows: rows.length, last: rows.at(-1).c};
    },
    check: async (_db, result) => result,
  },
  {
    id: 'group-by',
    group: 'Read',
    label: '10 GROUP BY aggregates over 10,000 rows',
    setup: (db) => table(db),
    async run(db) {
      const select = await db.prepare('SELECT g, count(*) AS n, sum(b) AS s FROM t GROUP BY g ORDER BY g');
      let groups = 0, total = 0;
      for (let i = 0; i < 10; i++) {
        for (const row of await db.run(select, [])) {
          groups++;
          total += Number(row.s);
        }
      }
      return {groups, total};
    },
    check: async (_db, result) => result,
  },
  {
    id: 'join',
    group: 'Read',
    label: '100 joins: one customer’s orders from 5,000',
    async setup(db) {
      await db.exec(
        'CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT NOT NULL); ' +
          'CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL, total INTEGER NOT NULL); ' +
          'CREATE INDEX orders_customer ON orders (customer_id)',
      );
      const customers = await db.prepare('INSERT INTO customers (id, name) VALUES ($1, $2)');
      const orders = await db.prepare('INSERT INTO orders (id, customer_id, total) VALUES ($1, $2, $3)');
      const next = random(7);
      await db.transaction(async (tx) => {
        for (let id = 1; id <= 100; id++) await tx.run(customers, [id, `customer ${words(id)}`]);
        for (let id = 1; id <= ORDERS; id++) await tx.run(orders, [id, 1 + Math.floor(next() * 100), Math.floor(next() * 1000)]);
      });
    },
    async run(db) {
      const select = await db.prepare(
        'SELECT o.id AS id, c.name AS name, o.total AS total FROM customers AS c ' +
          'JOIN orders AS o ON o.customer_id = c.id WHERE c.id = $1 ORDER BY o.id',
      );
      let rows = 0, total = 0;
      for (let id = 1; id <= 100; id++) {
        for (const row of await db.run(select, [id])) {
          rows++;
          total += row.total;
        }
      }
      return {rows, total};
    },
    check: async (_db, result) => result,
  },
  {
    id: 'update-pk',
    group: 'Update',
    label: 'One transaction of 1,000 UPDATEs by primary key',
    derivedFrom: 9,
    setup: (db) => table(db),
    async run(db) {
      const update = await db.prepare('UPDATE t SET c = $1 WHERE id = $2');
      await db.transaction(async (tx) => {
        for (const id of spreadIds(1000)) await tx.run(update, ['updated', id]);
      });
    },
    check: async (db) => ({...(await summary(db)), updated: await countWhereC(db, 'updated')}),
  },
  {
    id: 'update-scan',
    group: 'Update',
    label: 'One transaction of 100 range UPDATEs, no index',
    derivedFrom: 8,
    setup: (db) => table(db),
    async run(db) {
      const update = await db.prepare('UPDATE t SET c = $1 WHERE b >= $2 AND b < $3');
      await db.transaction(async (tx) => {
        for (let i = 0; i < 100; i++) await tx.run(update, ['updated', i * 1000, i * 1000 + 100]);
      });
    },
    check: async (db) => ({...(await summary(db)), updated: await countWhereC(db, 'updated')}),
  },
  {
    id: 'upsert',
    group: 'Update',
    label: 'One transaction of 1,000 upserts, half of them new',
    setup: (db) => table(db),
    async run(db) {
      const upsert = await db.prepare(
        `${INSERT}${placeholders(1)} ON CONFLICT (id) DO UPDATE SET c = EXCLUDED.c`,
      );
      await db.transaction(async (tx) => {
        for (const row of makeRows(1000, ROWS - 499)) await tx.run(upsert, row);
      });
    },
    check: summary,
  },
  {
    id: 'delete-pk',
    group: 'Delete',
    label: 'One transaction of 1,000 DELETEs by primary key',
    setup: (db) => table(db),
    async run(db) {
      const remove = await db.prepare('DELETE FROM t WHERE id = $1');
      await db.transaction(async (tx) => {
        for (const id of spreadIds(1000)) await tx.run(remove, [id]);
      });
    },
    check: summary,
  },
  {
    id: 'delete-like',
    group: 'Delete',
    label: 'One DELETE matching a LIKE pattern',
    derivedFrom: 12,
    setup: (db) => table(db),
    run: (db) => db.query('DELETE FROM t WHERE c LIKE $1', ['%fifty%']),
    check: summary,
  },
  {
    id: 'delete-range',
    group: 'Delete',
    label: 'One DELETE of 8,000 rows by indexed range',
    derivedFrom: 13,
    setup: (db) => table(db, ROWS, 'CREATE INDEX t_a ON t (a)'),
    run: (db) => db.query('DELETE FROM t WHERE a >= $1 AND a < $2', [1000, 9000]),
    check: summary,
  },
  {
    id: 'create-index',
    group: 'Schema',
    label: 'Create two indexes over 10,000 rows',
    derivedFrom: 6,
    setup: (db) => table(db),
    run: (db) => db.exec('CREATE INDEX t_b ON t (b); CREATE INDEX t_c ON t (c)'),
    async check(db) {
      const [row] = await db.query('SELECT count(*) AS n, avg(b) AS m FROM t WHERE b >= $1 AND b < $2', [5000, 6000]);
      return {n: Number(row.n), m: round(Number(row.m))};
    },
  },
];

// Startup is measured separately: from starting to fetch the engine to the
// first query result, on an empty new database and on a populated one.
export const startup = {
  async coldOpen(load) {
    const start = performance.now();
    const db = await load();
    const info = await db.open('cold');
    await db.exec(TABLE);
    const rows = await db.query('SELECT count(*) AS n FROM t');
    const ms = performance.now() - start;
    await db.close();
    return {ms, info, check: {rows: Number(rows[0].n)}};
  },
  async seedReopen(load) {
    const db = await load();
    await db.open('reopen');
    await table(db);
    await db.close();
  },
  async reopen(load) {
    const start = performance.now();
    const db = await load();
    const info = await db.open('reopen');
    const rows = await db.query('SELECT count(*) AS n, sum(b) AS b FROM t');
    const ms = performance.now() - start;
    await db.close();
    return {ms, info, check: {rows: Number(rows[0].n), b: Number(rows[0].b)}};
  },
};
