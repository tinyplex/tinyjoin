import {describe, expect, it, vi} from 'vitest';

import type {ReplicaSource} from '../../src/adapters/types.ts';
import {
  PROTOCOL_VERSION,
  type ApplyOutcome,
  type WorkerEvent,
  type WorkerRequest,
  type WorkerResponse,
} from '../../src/protocol.ts';
import type {WorkerEngine} from '../../src/worker/engine.ts';
import {
  startWorker,
  type WorkerScope,
} from '../../src/worker/host.ts';

class FakeScope implements WorkerScope {
  readonly posted: Array<WorkerResponse | WorkerEvent> = [];
  closed = false;
  readonly #listeners = new Set<(event: MessageEvent<unknown>) => void>();

  postMessage(message: WorkerResponse | WorkerEvent): void {
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

  close(): void {
    this.closed = true;
  }

  send(message: unknown): void {
    const event = {data: message} as MessageEvent<unknown>;
    for (const listener of this.#listeners) {
      listener(event);
    }
  }
}

function mockEngine() {
  let revision = 0;
  const outcome = (table: string): ApplyOutcome => ({
    revision: ++revision,
    tables: [table],
  });
  const engine: WorkerEngine = {
    defineTable: vi.fn(),
    replaceTable: vi.fn((table) => outcome(table)),
    applyBatch: vi.fn((batch) => outcome(batch.changes[0]?.table ?? 'none')),
    query: vi.fn(() => ({revision, rows: [{id: 1}]})),
    querySql: vi.fn(() => ({revision, rows: [{id: 1}]})),
    revision: () => revision,
    close: vi.fn(),
  };
  return engine;
}

async function waitForPosted(scope: FakeScope, count: number): Promise<void> {
  await vi.waitFor(() => expect(scope.posted.length).toBeGreaterThanOrEqual(count));
}

describe('startWorker', () => {
  it('routes requests through the engine and returns versioned responses', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    startWorker({scope, engineFactory: async () => engine});

    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {schemas: [{name: 'posts', primaryKey: ['id']}]},
    } satisfies WorkerRequest);
    scope.send({
      v: PROTOCOL_VERSION,
      id: 2,
      method: 'querySql',
      params: {sql: 'select * from posts', params: []},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 2);

    expect(engine.defineTable).toHaveBeenCalledWith({
      name: 'posts',
      primaryKey: ['id'],
    });
    expect(engine.querySql).toHaveBeenCalledWith('select * from posts', []);
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      id: 2,
      ok: true,
      result: {revision: 0, rows: [{id: 1}]},
    });
  });

  it('microbatches table invalidations from adjacent mutations', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    startWorker({scope, engineFactory: async () => engine});

    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'replaceTable',
      params: {table: 'posts', rows: []},
    } satisfies WorkerRequest);
    scope.send({
      v: PROTOCOL_VERSION,
      id: 2,
      method: 'replaceTable',
      params: {table: 'users', rows: []},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 3);

    expect(
      scope.posted.filter(
        (message): message is WorkerEvent => 'event' in message,
      ),
    ).toEqual([
      {
        v: PROTOCOL_VERSION,
        event: 'tablesChanged',
        payload: {revision: 2, tables: ['posts', 'users']},
      },
    ]);
  });

  it('rejects malformed messages without invoking the engine', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    startWorker({scope, engineFactory: async () => engine});

    scope.send({v: 99, id: 8, method: 'query', params: {}});
    await waitForPosted(scope, 1);

    expect(scope.posted[0]).toEqual({
      v: PROTOCOL_VERSION,
      id: 8,
      ok: false,
      error: {
        code: 'PROTOCOL_MISMATCH',
        message: 'The worker received an invalid TinyGres protocol request',
      },
    });
    expect(engine.query).not.toHaveBeenCalled();
  });

  it('rejects well-shaped methods with unsafe parameters', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    startWorker({scope, engineFactory: async () => engine});

    scope.send({
      v: PROTOCOL_VERSION,
      id: 9,
      method: 'replaceTable',
      params: {table: 'posts', rows: [{id: 1n}]},
    });
    await waitForPosted(scope, 1);

    expect(scope.posted[0]).toMatchObject({
      id: 9,
      ok: false,
      error: {code: 'PROTOCOL_MISMATCH'},
    });
    expect(engine.replaceTable).not.toHaveBeenCalled();
  });

  it('starts a future source adapter behind the worker boundary', async () => {
    const scope = new FakeScope();
    const engine = mockEngine();
    const source: ReplicaSource = {
      id: 'test-source',
      capabilities: {
        snapshotConsistency: 'eventual',
        changes: 'best-effort',
        resume: 'none',
        atomicity: 'row',
        writes: false,
      },
      async start(context) {
        await context.replaceTable(
          {name: 'posts', primaryKey: ['id']},
          [{id: 1, title: 'from source'}],
        );
        context.setSyncState({
          phase: 'live-best-effort',
          sourceId: 'test-source',
        });
      },
    };
    startWorker({scope, source, engineFactory: async () => engine});

    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {schemas: []},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 4);

    expect(engine.replaceTable).toHaveBeenCalledWith('posts', [
      {id: 1, title: 'from source'},
    ]);
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      event: 'syncStateChanged',
      payload: {phase: 'connecting', sourceId: 'test-source'},
    });
    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      event: 'syncStateChanged',
      payload: {phase: 'live-best-effort', sourceId: 'test-source'},
    });
  });

  it('reports structured source failures without crashing the worker', async () => {
    const scope = new FakeScope();
    const source: ReplicaSource = {
      id: 'failing-source',
      capabilities: {
        snapshotConsistency: 'eventual',
        changes: 'best-effort',
        resume: 'none',
        atomicity: 'row',
        writes: false,
      },
      async start() {
        throw Object.assign(new Error('temporarily offline'), {
          code: 'SOURCE_CONNECT_FAILED',
          retryable: true,
        });
      },
    };
    startWorker({
      scope,
      source,
      engineFactory: async () => mockEngine(),
    });

    scope.send({
      v: PROTOCOL_VERSION,
      id: 1,
      method: 'init',
      params: {schemas: []},
    } satisfies WorkerRequest);
    await waitForPosted(scope, 3);

    expect(scope.posted).toContainEqual({
      v: PROTOCOL_VERSION,
      event: 'syncStateChanged',
      payload: {
        phase: 'error',
        sourceId: 'failing-source',
        error: {
          code: 'SOURCE_CONNECT_FAILED',
          message: 'temporarily offline',
          retryable: true,
        },
      },
    });
  });
});
