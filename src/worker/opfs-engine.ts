import type {createPageWasmEngine, WorkerEngine} from './engine.js';
import {
  createOpfsPageStorageSession,
  type OpfsPageStorageProvider,
} from './page-storage.js';

export interface OpfsEngineDependencies {
  createSession?: typeof createOpfsPageStorageSession;
  createPageEngine: typeof createPageWasmEngine;
}

/** Opens the default page engine directly on its single OPFS page file. */
export async function createOpfsWasmEngine(
  name: string,
  provider: OpfsPageStorageProvider | undefined,
  dependencies: OpfsEngineDependencies,
): Promise<WorkerEngine> {
  const session = await (
    dependencies.createSession ?? createOpfsPageStorageSession
  )(name, provider);
  let engine: WorkerEngine | undefined;
  try {
    engine = await dependencies.createPageEngine(session.pageDevice);
    return engine;
  } catch (error) {
    try {
      engine?.close();
    } catch {
      // Preserve the page-engine construction failure.
    }
    if (engine === undefined) {
      try {
        session.close();
      } catch {
        // Preserve the page-engine construction failure.
      }
    }
    throw error;
  }
}
