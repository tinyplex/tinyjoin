import {PGlite} from '@electric-sql/pglite';
import {serve} from './rpc.js';

// opfs-ahp is PGlite's OPFS file system, which must run in a Worker. The
// default (strict) durability is kept, as it is for the other two engines.
let db;

serve({
  async open({storage, database}) {
    db = await PGlite.create(storage === 'opfs' ? `opfs-ahp://${database}` : 'memory://');
    const {rows} = await db.query('SHOW server_version');
    return {engineVersion: `PostgreSQL ${rows[0].server_version}`};
  },
  async exec({sql}) {
    await db.exec(sql);
  },
  async query({sql, params}) {
    return (await db.query(sql, params)).rows;
  },
  async close() {
    await db.close();
  },
});
