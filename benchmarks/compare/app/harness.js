import {startup, workloads} from './workloads.js';

// Each engine is a separate chunk, so a page only downloads the engine it is
// asked to load, and the runner can attribute every fetched byte to it.
const engines = {
  tinyjoin: () => import('./engines/tinyjoin.js'),
  sqlite: () => import('./engines/sqlite.js'),
  pglite: () => import('./engines/pglite.js'),
  turso: () => import('./engines/turso.js'),
};
const loader = (engine) => async () => (await engines[engine]()).default;

window.bench = {
  coldOpen: (engine, storage) => startup.coldOpen(loader(engine), storage),
  seedReopen: (engine, storage) => startup.seedReopen(loader(engine), storage),
  reopen: (engine, storage) => startup.reopen(loader(engine), storage),
  async run(engine, id, storage) {
    const workload = workloads.find((candidate) => candidate.id === id);
    const db = await loader(engine)();
    const info = await db.open(storage, `bench-${id}`);
    try {
      await workload.setup(db);
      const start = performance.now();
      const result = await workload.run(db);
      const ms = performance.now() - start;
      return {ms, info, check: await workload.check(db, result)};
    } finally {
      await db.close();
    }
  },
};
window.benchReady = true;
