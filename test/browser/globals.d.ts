interface Window {
  __tinygresTest?: {
    benchmark(iterations: number): Promise<number[]>;
    closeSupabaseProbe(): Promise<void>;
    openSupabaseProbe(options: {
      databaseName?: string;
      publishableKey: string;
      url: string;
    }): Promise<SupabaseProbeReport>;
    persistenceProbe(
      databaseName: string,
      rowCount: number,
    ): Promise<{
      crashReopenMs: number;
      conflictErrorCode: string;
      differentNameOpened: boolean;
      emptyTableRows: number;
      gracefulReopenMs: number;
      initialCommitMs: number;
      lockErrorCode: string;
      mutationCommitMs: number;
      revision: number;
      rowCount: number;
      updatedTitle: string;
    }>;
    writableDatabaseProbe(databaseName: string): Promise<{
      committedRevision: number;
      insertRows: Array<{done: boolean; id: number; title: string}>;
      invalidations: Array<{revision: number; tables: string[]}>;
      reopenedRevision: number;
      reopenedRows: Array<{done: boolean; id: number; title: string}>;
      rollbackCode: string;
      stagedRows: Array<{done: boolean; id: number; title: string}>;
    }>;
    readBrowserRestartFixture(databaseName: string): Promise<{
      emptyTableRows: number;
      revision: number;
      rowCount: number;
    }>;
    readSupabaseProbe(): Promise<SupabaseProbeReport>;
    waitForSupabaseProbe(): Promise<SupabaseProbeReport>;
    writeBrowserRestartFixture(
      databaseName: string,
      rowCount: number,
    ): Promise<{revision: number}>;
  };
}

interface SupabaseProbeReport {
  revision: number;
  rows: Array<{id: number; title: string}>;
  state: import('../../src/protocol.js').SyncState;
  states: import('../../src/protocol.js').SyncState[];
}
