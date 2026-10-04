import sqlite3InitModule from '@sqlite.org/sqlite-wasm';
import {serve} from './rpc.js';

// opfs-sahpool is the SQLite project's fastest OPFS VFS. Like TinyJoin's
// storage it holds its files exclusively, and it needs no COOP/COEP headers.
let db;
const statements = new Map();

serve({
  async open({database}) {
    const sqlite3 = await sqlite3InitModule();
    const pool = await sqlite3.installOpfsSAHPoolVfs({name: 'bench-sahpool'});
    db = new pool.OpfsSAHPoolDb(`/${database}.sqlite3`);
    return {engineVersion: sqlite3.version.libVersion};
  },
  exec({sql}) {
    db.exec(sql);
  },
  // Statements are cached by SQL text, which is what an application wrapper
  // around the oo1 API would do. $n placeholders become SQLite's ?n form.
  query({sql, params}) {
    let statement = statements.get(sql);
    if (statement == null) {
      statement = db.prepare(sql.replace(/\$(\d+)/g, '?$1'));
      statements.set(sql, statement);
    }
    try {
      if (params?.length) statement.bind(params);
      const rows = [];
      while (statement.step()) rows.push(statement.get({}));
      return rows;
    } finally {
      statement.reset(true);
    }
  },
  close() {
    for (const statement of statements.values()) statement.finalize();
    statements.clear();
    db.close();
  },
});
