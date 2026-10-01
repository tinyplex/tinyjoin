import {connect} from '@tursodatabase/database-wasm';

// Turso's browser build runs its engine on the page's thread, on threaded
// WebAssembly, and sends OPFS reads and writes to a Worker of its own. Hosted
// in a Worker instead, its thread pool deadlocks waiting for a nested Worker
// to start, so this adapter, unlike the others, calls it in the page. Threads
// need SharedArrayBuffer, so the runner serves Turso's page cross-origin
// isolated.
let db;
const statements = new Map();

// Statements are cached by SQL text, as in the SQLite adapter, and $n
// placeholders become SQLite's ?n form.
const prepare = (sql) => {
  let statement = statements.get(sql);
  if (statement == null) {
    statement = db.prepare(sql.replace(/\$(\d+)/g, '?$1'));
    statements.set(sql, statement);
  }
  return statement;
};
const query = (sql, params = []) => prepare(sql).all(...params);
const run = (handle, params) => query(handle.sql, params);

export default {
  name: 'turso',
  async open(storage, database) {
    db = await connect(storage === 'opfs' ? `${database}.db` : ':memory:');
    const [{version}] = await db.prepare('SELECT sqlite_version() AS version').all();
    return {engineVersion: `SQLite ${version} compatible`};
  },
  exec: async (sql) => {
    await db.exec(sql);
  },
  query,
  prepare: async (sql) => ({sql}),
  run,
  // SQL BEGIN/COMMIT, as for SQLite and PGlite.
  async transaction(callback) {
    await db.exec('BEGIN');
    try {
      await callback({run, query});
    } catch (error) {
      await db.exec('ROLLBACK');
      throw error;
    }
    await db.exec('COMMIT');
  },
  async close() {
    for (const statement of statements.values()) statement.close();
    statements.clear();
    await db.close();
  },
};
