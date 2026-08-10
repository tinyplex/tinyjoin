interface Window {
  __tinygresTest?: {
    benchmark(iterations: number): Promise<number[]>;
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
    readBrowserRestartFixture(databaseName: string): Promise<{
      emptyTableRows: number;
      revision: number;
      rowCount: number;
    }>;
    writeBrowserRestartFixture(
      databaseName: string,
      rowCount: number,
    ): Promise<{revision: number}>;
  };
}
