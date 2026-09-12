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
export const createOpfsWasmEngine = async (
  name: string,
  provider: OpfsPageStorageProvider | undefined,
  dependencies: OpfsEngineDependencies,
): Promise<WorkerEngine> => {
  const session = await (
    dependencies.createSession ?? createOpfsPageStorageSession
  )(name, provider);
  try {
    return await dependencies.createPageEngine(session.pageDevice);
  } catch (error) {
    try {
      session.close();
    } catch {
      // Preserve the page-engine construction failure.
    }
    throw error;
  }
};
