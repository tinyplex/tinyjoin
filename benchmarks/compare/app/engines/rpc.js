// The smallest request/response channel between a page and a database Worker,
// shared by the SQLite and PGlite adapters. TinyJoin brings its own.
export const connect = (worker) => {
  const pending = new Map();
  let nextId = 0;
  worker.addEventListener('message', ({data: {id, result, error}}) => {
    const request = pending.get(id);
    pending.delete(id);
    if (error == null) request.resolve(result);
    else request.reject(new Error(error));
  });
  worker.addEventListener('error', (event) => {
    for (const request of pending.values()) request.reject(new Error(event.message));
    pending.clear();
  });
  return (op, args = {}) =>
    new Promise((resolve, reject) => {
      const id = nextId++;
      pending.set(id, {resolve, reject});
      worker.postMessage({id, op, ...args});
    });
};

export const serve = (handlers) => {
  self.addEventListener('message', async ({data: {id, op, ...args}}) => {
    try {
      self.postMessage({id, result: await handlers[op](args)});
    } catch (error) {
      self.postMessage({id, error: String(error?.message ?? error)});
    }
  });
};

// Every adapter exposes the same surface: open, exec (parameter-free script),
// query (one statement), prepare/run (a repeated statement) and transaction.
// SQLite and PGlite use SQL BEGIN/COMMIT, one round trip each, just as each
// TinyJoin statement inside transaction() is its own round trip.
export const createRemoteAdapter = (name, createWorker) => {
  let worker, call;
  const run = (handle, params) => call('query', {sql: handle.sql, params});
  return {
    name,
    async open(storage, database) {
      worker = createWorker();
      call = connect(worker);
      return call('open', {storage, database});
    },
    exec: (sql) => call('exec', {sql}),
    query: (sql, params) => call('query', {sql, params}),
    prepare: async (sql) => ({sql}),
    run,
    async transaction(callback) {
      await call('exec', {sql: 'BEGIN'});
      try {
        await callback({run, query: (sql, params) => call('query', {sql, params})});
      } catch (error) {
        await call('exec', {sql: 'ROLLBACK'});
        throw error;
      }
      await call('exec', {sql: 'COMMIT'});
    },
    async close() {
      await call('close');
      worker.terminate();
    },
  };
};
