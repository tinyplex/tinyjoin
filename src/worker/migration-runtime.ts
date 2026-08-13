import type {WorkerEngine} from './engine.js';
import {createMigrationWasmEngine} from './migration-engine.js';
import {createPersistentEngine} from './persistent-engine.js';
import {createOpfsSnapshotStore} from './snapshot-store.js';

/**
 * Self-contained lazy runtime entry for the temporary legacy OPFS path.
 * The library build bundles this entry into one internal ESM asset.
 */
export async function createMigrationConfiguredEngine(
  name: string,
  glueUrl: string,
  wasmUrl: string,
): Promise<WorkerEngine> {
  const engine = await createMigrationWasmEngine(glueUrl, wasmUrl);
  let store: import('./snapshot-store.js').SnapshotStore | undefined;
  try {
    store = await createOpfsSnapshotStore(name);
    return createPersistentEngine(engine, store);
  } catch (error) {
    try {
      store?.close();
    } catch {
      // Preserve the initialization failure while still attempting all cleanup.
    }
    try {
      engine.close();
    } catch {
      // There is no usable engine to return or retry after initialization fails.
    }
    throw error;
  }
}
