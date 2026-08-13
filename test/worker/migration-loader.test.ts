import {describe, expect, it, vi} from 'vitest';

import type {WorkerEngine} from '../../src/worker/engine.ts';
import {createOpfsMigrationEngine} from '../../src/worker/migration-loader.ts';

describe('migration loader boundary', () => {
  it('forwards only absolute runtime, glue, and WASM asset URLs', async () => {
    const engine = {} as WorkerEngine;
    const create = vi.fn(
      async (_name: string, _glueUrl: string, _wasmUrl: string) => engine,
    );
    const load = vi.fn(async (_url: string) => ({
      createMigrationConfiguredEngine: create,
    }));

    await expect(createOpfsMigrationEngine('legacy-name', load)).resolves.toBe(
      engine,
    );
    expect(load).toHaveBeenCalledOnce();
    expect(new URL(load.mock.calls[0]![0]).pathname).toMatch(
      /\/worker-migration\/tinygres_migration_runtime\.js$/,
    );
    expect(create).toHaveBeenCalledOnce();
    const [name, glueUrl, wasmUrl] = create.mock.calls[0]!;
    expect(name).toBe('legacy-name');
    expect(new URL(glueUrl).pathname).toMatch(
      /\/wasm-migration\/tinygres_migration_wasm\.js$/,
    );
    expect(new URL(wasmUrl).pathname).toMatch(
      /\/wasm-migration\/tinygres_migration_wasm_bg\.wasm$/,
    );
  });

  it('surfaces runtime import failure before opening migration storage', async () => {
    const failure = Object.assign(new Error('migration runtime unavailable'), {
      code: 'MIGRATION_RUNTIME_UNAVAILABLE',
    });
    await expect(
      createOpfsMigrationEngine('legacy-name', async () => {
        throw failure;
      }),
    ).rejects.toBe(failure);
  });
});
