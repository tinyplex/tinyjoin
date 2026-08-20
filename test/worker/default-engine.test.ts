import {beforeEach, describe, expect, it, vi} from 'vitest';

import {PROTOCOL_VERSION, type WorkerRequest} from '../../src/protocol.ts';
import type {WorkerEngine} from '../../src/worker/engine.ts';
import {startWorker, type WorkerScope} from '../../src/worker/host.ts';

const runtime = vi.hoisted(() => ({
  memoryCreates: 0,
  opfsCreates: 0,
}));

vi.mock('../../src/worker/engine.ts', async (loadActual) => {
  const actual = await loadActual<typeof import('../../src/worker/engine.ts')>();
  return {
    ...actual,
    createMemoryWasmEngine: async () => {
      runtime.memoryCreates += 1;
      return mockEngine();
    },
  };
});

vi.mock('../../src/worker/opfs-loader.ts', () => ({
  createOpfsWasmEngine: async () => {
    runtime.opfsCreates += 1;
    return mockEngine();
  },
}));

class FakeScope implements WorkerScope {
  readonly posted: unknown[] = [];
  readonly #listeners = new Set<(event: MessageEvent<unknown>) => void>();

  postMessage(message: unknown): void {
    this.posted.push(message);
  }

  addEventListener(
    _type: 'message',
    listener: (event: MessageEvent<unknown>) => void,
  ): void {
    this.#listeners.add(listener);
  }

  removeEventListener(
    _type: 'message',
    listener: (event: MessageEvent<unknown>) => void,
  ): void {
    this.#listeners.delete(listener);
  }

  close(): void {}

  send(request: WorkerRequest): void {
    for (const listener of this.#listeners) {
      listener({data: request} as MessageEvent<unknown>);
    }
  }
}

beforeEach(() => {
  runtime.memoryCreates = 0;
  runtime.opfsCreates = 0;
});

describe('default engine selection', () => {
  it('opens memory through only the page-native factory', async () => {
    const scope = new FakeScope();
    startWorker({scope});
    scope.send(initRequest({kind: 'memory'}));
    await vi.waitFor(() => expect(scope.posted).toHaveLength(1));

    expect(runtime).toEqual({
      memoryCreates: 1,
      opfsCreates: 0,
    });
  });

  it('opens OPFS through only the page-storage runtime', async () => {
    const scope = new FakeScope();
    startWorker({scope});
    scope.send(initRequest({kind: 'opfs', name: 'page-test'}));
    await vi.waitFor(() => expect(scope.posted).toHaveLength(1));

    expect(runtime).toEqual({
      memoryCreates: 0,
      opfsCreates: 1,
    });
    expect(scope.posted[0]).toMatchObject({id: 1, ok: true});
  });
});

function initRequest(
  storage: {kind: 'memory'} | {kind: 'opfs'; name: string},
): WorkerRequest {
  return {
    v: PROTOCOL_VERSION,
    id: 1,
    method: 'init',
    params: {schemas: [], storage},
  };
}

function mockEngine(): WorkerEngine {
  return {
    defineTable: vi.fn(),
    defineTables: vi.fn(),
    replaceTableSnapshot: vi.fn(() => ({revision: 0, tables: []})),
    applyBatch: vi.fn(() => ({revision: 0, tables: []})),
    query: vi.fn(() => ({revision: 0, rows: []})),
    querySql: vi.fn(() => ({revision: 0, rows: []})),
    executeSql: vi.fn(() => ({
      command: 'SELECT',
      revision: 0,
      rowCount: 0,
      rows: [],
      tables: [],
    })),
    beginTransaction: vi.fn(),
    commitTransaction: vi.fn(() => ({revision: 0, tables: []})),
    rollbackTransaction: vi.fn(),
    inTransaction: vi.fn(() => false),
    revision: vi.fn(() => 0),
    close: vi.fn(),
  };
}
