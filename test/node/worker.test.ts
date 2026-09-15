import type {EventEmitter} from 'node:events';
import type {WorkerOptions} from 'node:worker_threads';
import {afterEach, beforeEach, describe, expect, it, vi, type Mock} from 'vitest';

import {createWorkerRpc} from '../../src/client/rpc.ts';
import {createNodeWorker} from '../../src/node/worker.ts';
import {PROTOCOL_VERSION, type WorkerRequest} from '../../src/protocol.ts';

interface FakeNodeWorker extends EventEmitter {
  url: string | URL;
  options: WorkerOptions | undefined;
  postMessage: Mock<(message: unknown) => void>;
  terminate: Mock<() => Promise<number>>;
}

const instances = vi.hoisted(() => [] as FakeNodeWorker[]);

vi.mock('node:worker_threads', async () => {
  const {EventEmitter} = await import('node:events');
  return {
    Worker: class extends EventEmitter {
      postMessage = vi.fn<(message: unknown) => void>();
      terminate = vi.fn(async () => {
        this.emit('exit', 1);
        return 1;
      });

      constructor(
        readonly url: string | URL,
        readonly options: WorkerOptions | undefined,
      ) {
        super();
        instances.push(this);
      }
    },
  };
});

const latestWorker = (): FakeNodeWorker => instances.at(-1)!;

describe('Node Worker adapter', () => {
  beforeEach(() => {
    instances.length = 0;
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('carries requests and responses over Node Worker messages', async () => {
    const adapter = createNodeWorker();
    const worker = latestWorker();
    const rpc = createWorkerRpc(adapter);
    const pending = rpc.request('init', {storage: {kind: 'memory'}});
    const request = worker.postMessage.mock.calls[0]![0] as WorkerRequest;

    expect(worker.url).toBeInstanceOf(URL);
    expect(request).toMatchObject({
      v: PROTOCOL_VERSION,
      method: 'init',
      params: {storage: {kind: 'memory'}},
    });
    worker.emit('message', {
      v: PROTOCOL_VERSION,
      id: request.id,
      ok: true,
      result: {revision: 0},
    });

    await expect(pending).resolves.toEqual({revision: 0});
    rpc.dispose();
    expect(worker.terminate).toHaveBeenCalledOnce();
  });

  it('removes browser-style message and error listeners', () => {
    const adapter = createNodeWorker();
    const worker = latestWorker();
    const onMessage = vi.fn();
    const onError = vi.fn();
    const onMessageError = vi.fn();
    adapter.addEventListener('message', onMessage);
    adapter.addEventListener('error', onError);
    adapter.addEventListener('messageerror', onMessageError);
    adapter.removeEventListener('message', onMessage);
    adapter.removeEventListener('error', onError);
    adapter.removeEventListener('messageerror', onMessageError);

    worker.emit('message', {unused: true});
    worker.emit('error', new Error('unused failure'));
    worker.emit('messageerror', new Error('unused clone failure'));

    expect(onMessage).not.toHaveBeenCalled();
    expect(onError).not.toHaveBeenCalled();
    expect(onMessageError).not.toHaveBeenCalled();
    adapter.terminate?.();
  });

  it('rejects all pending requests when the Node Worker crashes', async () => {
    const rpc = createWorkerRpc(createNodeWorker());
    const worker = latestWorker();
    const first = rpc.request('init', {storage: {kind: 'memory'}});
    const second = rpc.request('executeSql', {sql: 'SELECT 1', params: []});
    const assertions = [first, second].map((pending) =>
      expect(pending).rejects.toMatchObject({
        code: 'WORKER_ERROR',
        message: 'Node worker startup failed',
      }),
    );

    worker.emit('error', new Error('Node worker startup failed'));

    await Promise.all(assertions);
    expect(worker.terminate).toHaveBeenCalledOnce();
    await expect(rpc.request('close', undefined)).rejects.toMatchObject({
      code: 'WORKER_TERMINATED',
    });
  });

  it('rejects pending requests when Node cannot deserialize a message', async () => {
    const rpc = createWorkerRpc(createNodeWorker());
    const worker = latestWorker();
    const pending = rpc.request('init', {storage: {kind: 'memory'}});
    const assertion = expect(pending).rejects.toMatchObject({
      code: 'WORKER_MESSAGE_ERROR',
    });

    worker.emit('messageerror', new Error('deserialization failed'));

    await assertion;
    expect(worker.terminate).toHaveBeenCalledOnce();
  });

  it.each([0, 1])(
    'rejects pending requests when the Worker exits unexpectedly with code %i',
    async (exitCode) => {
      const rpc = createWorkerRpc(createNodeWorker());
      const worker = latestWorker();
      const pending = rpc.request('init', {storage: {kind: 'memory'}});
      const assertion = expect(pending).rejects.toMatchObject({
        code: 'WORKER_ERROR',
      });

      worker.emit('exit', exitCode);

      await assertion;
    },
  );

  it('terminates once and suppresses the resulting exit notification', () => {
    const adapter = createNodeWorker();
    const worker = latestWorker();
    const onError = vi.fn();
    adapter.addEventListener('error', onError);

    adapter.terminate?.();
    adapter.terminate?.();

    expect(worker.terminate).toHaveBeenCalledOnce();
    expect(onError).not.toHaveBeenCalled();
  });

  it('omits inherited input-type flags while preserving other Node options', () => {
    const execArgv = [
      '--enable-source-maps',
      '--input-type=module',
      '--trace-warnings',
      '--input-type',
      'module',
    ];
    const originalExecArgv = process.execArgv;
    process.execArgv = execArgv;
    try {
      const adapter = createNodeWorker();
      expect(latestWorker().options?.execArgv).toEqual([
        '--enable-source-maps',
        '--trace-warnings',
      ]);
      adapter.terminate?.();
    } finally {
      process.execArgv = originalExecArgv;
    }
    expect(execArgv).toEqual([
      '--enable-source-maps',
      '--input-type=module',
      '--trace-warnings',
      '--input-type',
      'module',
    ]);
  });
});
