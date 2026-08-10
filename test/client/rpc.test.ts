import {describe, expect, it, vi} from 'vitest';

import {WorkerRpc} from '../../src/client/rpc.ts';
import {PROTOCOL_VERSION, type WorkerRequest} from '../../src/protocol.ts';
import {FakeWorker} from '../helpers/fake-worker.ts';

describe('WorkerRpc', () => {
  it('matches out-of-order responses to their requests', async () => {
    const worker = new FakeWorker();
    const rpc = new WorkerRpc(worker);
    const first = rpc.request('querySql', {sql: 'first', params: []});
    const second = rpc.request('querySql', {sql: 'second', params: []});
    const [firstRequest, secondRequest] = worker.posted as WorkerRequest[];

    worker.respond({
      v: PROTOCOL_VERSION,
      id: secondRequest!.id,
      ok: true,
      result: {revision: 2, rows: [{id: 2}]},
    });
    worker.respond({
      v: PROTOCOL_VERSION,
      id: firstRequest!.id,
      ok: true,
      result: {revision: 1, rows: [{id: 1}]},
    });

    await expect(first).resolves.toEqual({revision: 1, rows: [{id: 1}]});
    await expect(second).resolves.toEqual({revision: 2, rows: [{id: 2}]});
  });

  it('turns structured worker failures into TinyGres errors', async () => {
    const worker = new FakeWorker();
    const rpc = new WorkerRpc(worker);
    const request = rpc.request('querySql', {sql: 'bad', params: []});
    const [message] = worker.posted as WorkerRequest[];

    worker.respond({
      v: PROTOCOL_VERSION,
      id: message!.id,
      ok: false,
      error: {code: 'UNSUPPORTED_SQL', message: 'No joins yet'},
    });

    await expect(request).rejects.toMatchObject({
      code: 'UNSUPPORTED_SQL',
      message: 'No joins yet',
    });
  });

  it('delivers events without consuming pending responses', async () => {
    const worker = new FakeWorker();
    const rpc = new WorkerRpc(worker);
    const listener = vi.fn();
    rpc.onEvent(listener);
    const request = rpc.request('querySql', {sql: 'select', params: []});
    const [message] = worker.posted as WorkerRequest[];

    worker.respond({
      v: PROTOCOL_VERSION,
      event: 'tablesChanged',
      payload: {revision: 3, tables: ['posts']},
    });
    worker.respond({
      v: PROTOCOL_VERSION,
      id: message!.id,
      ok: true,
      result: {revision: 3, rows: []},
    });

    expect(listener).toHaveBeenCalledOnce();
    await expect(request).resolves.toEqual({revision: 3, rows: []});
  });

  it('rejects every pending request after a protocol mismatch', async () => {
    const worker = new FakeWorker();
    const rpc = new WorkerRpc(worker);
    const request = rpc.request('querySql', {sql: 'select', params: []});

    worker.emitInvalidMessage({v: 999, id: 1, ok: true, result: []});

    await expect(request).rejects.toMatchObject({code: 'PROTOCOL_MISMATCH'});
    expect(worker.terminated).toBe(true);
  });

  it('rejects pending work when the worker crashes', async () => {
    const worker = new FakeWorker();
    const rpc = new WorkerRpc(worker);
    const request = rpc.request('querySql', {sql: 'select', params: []});

    worker.emitError('boom');

    await expect(request).rejects.toMatchObject({
      code: 'WORKER_ERROR',
      message: 'boom',
    });
  });
});
