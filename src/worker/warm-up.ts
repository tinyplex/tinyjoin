import type {WorkerEngine} from './engine.js';

const TEXT = ['one', 'two', 'fifty', 'hundred', 'thousand', 'seven', 'nine'];

// Five rows' placeholders for a multi-row INSERT into `w`.
const ROWS = Array.from(
  {length: 5},
  (_, row) =>
    `(${Array.from({length: 6}, (_, column) => `$${row * 6 + column + 1}`).join(', ')})`,
).join(', ');

/** A row of `w` for `id`: an indexed integer, a float, text, a boolean, and JSON. */
const row = (id: number): (number | string | boolean | null)[] => [
  id,
  id % 97,
  (id * 7919) % 10_000 / 4,
  `${TEXT[id % 7]} ${TEXT[(id >> 3) % 7]} ${id}`,
  id % 3 === 0,
  id % 5 === 0 ? null : id,
];

// Statements each run for every id below, as reads, writes, and scans reach them.
const READS: [sql: string, params: (i: number) => (number | string)[]][] = [
  ['SELECT * FROM w WHERE id = $1', (i) => [i]],
  ['SELECT count(*) AS n, avg(b) AS m, max(c) AS x FROM w WHERE b >= $1 AND b < $2', (i) => [i, i + 500]],
  ['SELECT count(*) AS n FROM w WHERE c LIKE $1', (i) => [`%${TEXT[i % 7]}%`]],
  ['SELECT d, count(*) AS n, sum(a) AS s FROM w GROUP BY d ORDER BY d', () => []],
  ['SELECT w.id AS id, v.n AS n FROM w JOIN v ON v.wid = w.id WHERE w.id = $1 ORDER BY v.id', (i) => [i]],
  ['SELECT id, c FROM w WHERE a BETWEEN $1 AND $2 ORDER BY id DESC LIMIT 5', (i) => [i % 90, (i % 90) + 5]],
  ['SELECT DISTINCT d FROM w WHERE a IN ($1, $2)', (i) => [i % 97, (i + 1) % 97]],
];

/**
 * Runs the engine's common statements on a scratch database, so that the WebAssembly code they
 * reach is compiled, and the hottest of it optimized, before a database needs it. Chromium
 * compiles each function only when it is first called, and Workers that instantiate the same
 * module share the code either one compiles. Statements run once each first, so that every path
 * compiles early, and then often enough for their loops to be optimized.
 */
export const warmUp = (engine: WorkerEngine): void => {
  engine.execSql(
    'CREATE TABLE w (id INTEGER PRIMARY KEY, a INTEGER NOT NULL, b FLOAT, c TEXT NOT NULL, d BOOLEAN, e JSON);' +
      'CREATE TABLE v (id INTEGER PRIMARY KEY, wid INTEGER NOT NULL, n INTEGER NOT NULL);' +
      'CREATE INDEX w_a ON w (a); CREATE INDEX v_wid ON v (wid)',
  );
  const insertRows = engine.prepareSql(`INSERT INTO w (id, a, b, c, d, e) VALUES ${ROWS}`);
  const insert = engine.prepareSql('INSERT INTO w (id, a, b, c, d, e) VALUES ($1, $2, $3, $4, $5, $6)');
  const child = engine.prepareSql('INSERT INTO v (id, wid, n) VALUES ($1, $2, $3)');
  const reads = READS.map(([sql, params]) => [engine.prepareSql(sql), params] as const);
  const update = engine.prepareSql('UPDATE w SET c = $1, e = $2 WHERE id = $3');
  const upsert = engine.prepareSql(
    'INSERT INTO w (id, a, b, c, d, e) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id) DO UPDATE SET c = EXCLUDED.c',
  );
  const remove = engine.prepareSql('DELETE FROM w WHERE id = $1');
  let next = 1;
  // The second pass runs each loop just long enough to have it optimized. A longer one finished
  // later, while a database's first statements were running, and left those statements slower.
  const passes = [5, 300];
  for (const [pass, count] of passes.entries()) {
    engine.beginTransaction();
    for (let i = 0; i < count; i += 5) {
      engine.executePrepared(insertRows, [0, 1, 2, 3, 4].flatMap((offset) => row(next + offset)));
      next += 5;
    }
    for (let i = 0; i < count; i++) {
      engine.executePrepared(insert, row(next));
      engine.executePrepared(child, [next, next % 50, i]);
      next++;
    }
    engine.commitTransaction();
    engine.executeSql('INSERT INTO w (id, a, b, c, d, e) VALUES ($1, $2, $3, $4, $5, $6)', row(next++));
    for (const [statement, params] of reads) {
      for (let i = 0; i < (pass ? 40 : 1); i++) engine.executePrepared(statement, params(i + 1));
    }
    engine.beginTransaction();
    for (let i = 1; i <= count; i++) {
      engine.executePrepared(update, [`updated ${i}`, {pass, i}, i]);
      engine.executePrepared(upsert, row(next - i));
      if (i % 4 === 0) engine.executePrepared(remove, [i]);
    }
    engine.commitTransaction();
    engine.executeSql('UPDATE w SET d = $1 WHERE b >= $2 AND b < $3', [true, 100, 300]);
    engine.executeSql('DELETE FROM w WHERE c LIKE $1', [`%${TEXT[pass + 1]}%`]);
    engine.executeSql('DELETE FROM w WHERE a >= $1 AND a < $2', [10, 20]);
  }
  engine.beginTransaction();
  engine.executeSql('DELETE FROM v WHERE wid < $1', [10]);
  engine.rollbackTransaction();
  engine.execSql(
    'CREATE INDEX w_c ON w (c); ALTER TABLE w ADD COLUMN f TEXT DEFAULT \'x\';' +
      'SELECT * FROM w ORDER BY id; DROP INDEX w_c; DROP TABLE v',
  );
};
