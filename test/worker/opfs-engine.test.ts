import {describe, expect, it, vi} from 'vitest';

import type {WorkerEngine} from '../../src/worker/engine.ts';
import {
  createOpfsWasmEngine,
  type OpfsEngineDependencies,
} from '../../src/worker/opfs-engine.ts';
import type {PageDevice} from '../../src/worker/page-device.ts';
import type {OpfsPageStorageSession} from '../../src/worker/page-storage.ts';

const pageDevice = {
  pageCount: vi.fn(() => 0),
  readPage: vi.fn(() => 4096),
  writePage: vi.fn(() => 4096),
  flush: vi.fn(),
  close: vi.fn(),
} satisfies PageDevice;

function engine(): WorkerEngine {
  return {
    defineTable: vi.fn(),
    defineTables: vi.fn(),
    replaceTableSnapshot: vi.fn(() => ({revision: 0, tables: []})),
    applyBatch: vi.fn(() => ({revision: 0, tables: []})),
    query: vi.fn(() => ({revision: 0, fields: [], rows: []})),
    executeSql: vi.fn(() => ({
      command: 'SELECT',
      fields: [],
      revision: 0,
      rowCount: 0,
      rows: [],
      tables: [],
    })),
    prepareSql: vi.fn(() => 1),
    executePrepared: vi.fn(() => ({
      command: 'SELECT',
      fields: [],
      revision: 0,
      rowCount: 0,
      rows: [],
      tables: [],
    })),
    closePrepared: vi.fn(),
    execSql: vi.fn(() => []),
    beginTransaction: vi.fn(),
    commitTransaction: vi.fn(() => ({revision: 0, tables: []})),
    rollbackTransaction: vi.fn(),
    inTransaction: vi.fn(() => false),
    revision: vi.fn(() => 0),
    close: vi.fn(),
  };
}

function session(): OpfsPageStorageSession {
  return {pageDevice, close: vi.fn(() => pageDevice.close())};
}

function dependencies(
  selectedSession: OpfsPageStorageSession,
  selectedEngine: WorkerEngine,
): OpfsEngineDependencies {
  return {
    createSession: vi.fn(async () => selectedSession),
    createPageEngine: vi.fn(async () => selectedEngine),
  };
}

describe('page-only OPFS engine', () => {
  it('opens and returns the default page engine directly', async () => {
    const opened = session();
    const selected = engine();
    const deps = dependencies(opened, selected);

    const result = await createOpfsWasmEngine(
      'direct-pages',
      undefined,
      deps,
    );
    expect(deps.createSession).toHaveBeenCalledWith('direct-pages', undefined);
    expect(deps.createPageEngine).toHaveBeenCalledWith(pageDevice);
    expect(result).toBe(selected);
    expect(selected.defineTables).not.toHaveBeenCalled();
    expect(opened.close).not.toHaveBeenCalled();
  });

  it('preserves page-engine construction failure and closes the OPFS session', async () => {
    const failure = new Error('page engine construction failed');
    const opened = session();
    vi.mocked(opened.close).mockImplementation(() => {
      throw new Error('secondary close failure');
    });
    const deps = dependencies(opened, engine());
    vi.mocked(deps.createPageEngine).mockRejectedValue(failure);

    await expect(
      createOpfsWasmEngine('constructor-failure', undefined, deps),
    ).rejects.toBe(failure);
    expect(opened.close).toHaveBeenCalledOnce();
  });

  it('does not attempt page-engine construction when storage cannot open', async () => {
    const failure = new Error('storage failed');
    const createPageEngine = vi.fn();
    await expect(
      createOpfsWasmEngine('storage-failure', undefined, {
        createSession: vi.fn(async () => {
          throw failure;
        }),
        createPageEngine,
      }),
    ).rejects.toBe(failure);
    expect(createPageEngine).not.toHaveBeenCalled();
  });
});
