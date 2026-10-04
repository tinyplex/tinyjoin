import {create} from 'tinyjoin';

// TinyJoin's default create() owns its Worker, so this adapter only maps the
// shared benchmark surface onto the public Client API.
let db;
const rows = async (results) => (await results).rows;

export default {
  name: 'tinyjoin',
  async open(database) {
    db = await create(`opfs://${database}`);
    return {engineVersion: null};
  },
  exec: async (sql) => {
    await db.exec(sql);
  },
  query: (sql, params) => rows(db.query(sql, params)),
  prepare: (sql) => db.prepare(sql),
  run: (statement, params) => rows(statement.execute(params)),
  transaction: (callback) =>
    db.transaction((tx) =>
      callback({
        run: (statement, params) => rows(tx.execute(statement, params)),
        query: (sql, params) => rows(tx.query(sql, params)),
      }),
    ),
  close: () => db.close(),
};
