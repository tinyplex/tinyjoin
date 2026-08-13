import {afterEach, describe, expect, it, vi} from 'vitest';

import type {WorkerEngine} from '../../src/worker/engine.ts';

const calls = vi.hoisted(() => ({
  glueUrl: '',
  wasmUrl: '',
  engineCloses: 0,
  engineCloseThrows: false,
  storeCloses: 0,
  storeCloseThrows: false,
  persistentCreates: 0,
}));

const engine = vi.hoisted(() => ({
  close: () => {
    calls.engineCloses += 1;
    if (calls.engineCloseThrows) {
      throw new Error('engine close failed');
    }
  },
})) as unknown as WorkerEngine;

vi.mock('../../src/worker/migration-engine.ts', () => ({
  createMigrationWasmEngine: async (glueUrl: string, wasmUrl: string) => {
    calls.glueUrl = glueUrl;
    calls.wasmUrl = wasmUrl;
    return engine;
  },
}));

vi.mock('../../src/worker/snapshot-store.ts', () => ({
  createOpfsSnapshotStore: vi.fn(async () => ({
    close: () => {
      calls.storeCloses += 1;
      if (calls.storeCloseThrows) {
        throw new Error('store close failed');
      }
    },
  })),
}));

vi.mock('../../src/worker/persistent-engine.ts', () => ({
  createPersistentEngine: vi.fn((candidate: WorkerEngine) => {
    calls.persistentCreates += 1;
    return candidate;
  }),
}));

afterEach(() => {
  calls.glueUrl = '';
  calls.wasmUrl = '';
  calls.engineCloses = 0;
  calls.engineCloseThrows = false;
  calls.storeCloses = 0;
  calls.storeCloseThrows = false;
  calls.persistentCreates = 0;
  vi.clearAllMocks();
});

describe('migration runtime boundary', () => {
  it('passes absolute glue/WASM URLs into the isolated legacy engine', async () => {
    const {createMigrationConfiguredEngine} = await import(
      '../../src/worker/migration-runtime.ts'
    );
    await expect(
      createMigrationConfiguredEngine(
        'legacy-name',
        'https://example.test/tinygres_migration_wasm.js',
        'https://example.test/tinygres_migration_wasm_bg.wasm',
      ),
    ).resolves.toBe(engine);
    expect(calls).toMatchObject({
      glueUrl: 'https://example.test/tinygres_migration_wasm.js',
      wasmUrl: 'https://example.test/tinygres_migration_wasm_bg.wasm',
      engineCloses: 0,
      persistentCreates: 1,
      storeCloses: 0,
    });
  });

  it('closes the raw engine when opening OPFS fails', async () => {
    const {createOpfsSnapshotStore} = await import(
      '../../src/worker/snapshot-store.ts'
    );
    vi.mocked(createOpfsSnapshotStore).mockRejectedValueOnce(
      new Error('OPFS unavailable'),
    );
    const {createMigrationConfiguredEngine} = await import(
      '../../src/worker/migration-runtime.ts'
    );
    await expect(
      createMigrationConfiguredEngine(
        'failed-name',
        'https://example.test/glue.js',
        'https://example.test/module.wasm',
      ),
    ).rejects.toThrow('OPFS unavailable');
    expect(calls.engineCloses).toBe(1);
    expect(calls.persistentCreates).toBe(0);
  });

  it('closes both store and raw engine when persistence wrapping fails', async () => {
    const {createPersistentEngine} = await import(
      '../../src/worker/persistent-engine.ts'
    );
    vi.mocked(createPersistentEngine).mockImplementationOnce(() => {
      throw new Error('wrapper failed');
    });
    const {createMigrationConfiguredEngine} = await import(
      '../../src/worker/migration-runtime.ts'
    );
    await expect(
      createMigrationConfiguredEngine(
        'failed-wrapper',
        'https://example.test/glue.js',
        'https://example.test/module.wasm',
      ),
    ).rejects.toThrow('wrapper failed');
    expect(calls.engineCloses).toBe(1);
    expect(calls.storeCloses).toBe(1);
  });

  it.each([
    ['store cleanup', true, false],
    ['engine cleanup', false, true],
    ['both cleanup operations', true, true],
  ])(
    'preserves the initialization error when %s throws',
    async (_label, storeCloseThrows, engineCloseThrows) => {
      calls.storeCloseThrows = storeCloseThrows;
      calls.engineCloseThrows = engineCloseThrows;
      const failure = Object.assign(new Error('wrapper failed'), {
        code: 'WRAPPER_FAILED',
      });
      const {createPersistentEngine} = await import(
        '../../src/worker/persistent-engine.ts'
      );
      vi.mocked(createPersistentEngine).mockImplementationOnce(() => {
        throw failure;
      });
      const {createMigrationConfiguredEngine} = await import(
        '../../src/worker/migration-runtime.ts'
      );

      await expect(
        createMigrationConfiguredEngine(
          'failed-cleanup',
          'https://example.test/glue.js',
          'https://example.test/module.wasm',
        ),
      ).rejects.toBe(failure);
      expect(calls.storeCloses).toBe(1);
      expect(calls.engineCloses).toBe(1);
    },
  );
});
