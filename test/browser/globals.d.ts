interface Window {
  __tinygresTest?: {
    benchmark(iterations: number): Promise<number[]>;
    closeSupabaseProbe(): Promise<void>;
    joinDatabaseProbe(): Promise<{
      innerRows: Array<{
        author_id: number;
        author_name: string;
        article_id: number;
        article_title: string;
      }>;
      leftRows: Array<{
        author_id: number;
        author_name: string;
        article_id: number | null;
      }>;
    }>;
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
      journalWriteBytes: number;
      revision: number;
      rowCount: number;
      updatedTitle: string;
    }>;
    writableDatabaseProbe(databaseName: string): Promise<{
      aggregateRows: Array<{done: boolean; task_count: number}>;
      committedRevision: number;
      insertRows: Array<{done: boolean; id: number; title: string}>;
      invalidations: Array<{revision: number; tables: string[]}>;
      orderedRows: Array<{done: boolean; id: number; title: string}>;
      reopenedPriorities: Array<{id: number; priority: number}>;
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
