import {describe, expect, it, vi} from 'vitest';

import {
  createPageWasmEngine,
  type WorkerEngine,
} from '../../src/worker/engine.ts';
import {createOpfsWasmEngine} from '../../src/worker/opfs-loader.ts';

describe('private OPFS runtime loader', () => {
  it('loads the literal private asset and injects only the page engine', async () => {
    const engine = {} as WorkerEngine;
    const createRuntimeEngine = vi.fn(async () => engine);
    const loadRuntime = vi.fn(async (_url: string) => ({
      createOpfsWasmEngine: createRuntimeEngine,
    }));

    await expect(
      createOpfsWasmEngine('loader-test', loadRuntime),
    ).resolves.toBe(engine);

    expect(loadRuntime).toHaveBeenCalledOnce();
    const [url] = loadRuntime.mock.calls[0]!;
    expect(new URL(url).pathname).toMatch(
      /\/worker-opfs\/tinygres_opfs_runtime\.js$/,
    );
    expect(createRuntimeEngine).toHaveBeenCalledWith(
      'loader-test',
      undefined,
      {
        createPageEngine: createPageWasmEngine,
      },
    );
  });

  it('preserves a runtime-load failure before any OPFS session can open', async () => {
    const failure = new Error('runtime load failed');
    const loadRuntime = vi.fn(async (_url: string) => {
      throw failure;
    });

    await expect(
      createOpfsWasmEngine('unopened', loadRuntime),
    ).rejects.toBe(failure);
    expect(loadRuntime).toHaveBeenCalledOnce();
  });

  it('preserves a runtime construction failure', async () => {
    const failure = new Error('session construction failed');
    const createRuntimeEngine = vi.fn(async () => {
      throw failure;
    });

    await expect(
      createOpfsWasmEngine('construction-failure', async () => ({
        createOpfsWasmEngine: createRuntimeEngine,
      })),
    ).rejects.toBe(failure);
    expect(createRuntimeEngine).toHaveBeenCalledOnce();
  });
});
