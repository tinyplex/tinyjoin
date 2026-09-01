interface Window {
  __tinyjoinTest?: {
    benchmark(iterations: number): Promise<number[]>;
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
      manyToManyRows: Array<{
        post_id: number;
        post_title: string;
        tag_id: number;
        tag_name: string;
      }>;
    }>;
    pageTransactionDdlProbe(): Promise<{
      committedRows: number;
      ddlCodes: string[];
      rejectedScriptRows: number;
      revisionAfter: number;
      revisionBefore: number;
      stagedRows: number;
    }>;
    persistenceProbe(
      databaseName: string,
      rowCount: number,
    ): Promise<{
      crashReopenMs: number;
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
      aggregateRows: Array<{done: boolean; task_count: number}>;
      bootstrapCommands: Array<string | undefined>;
      bootstrapRevisions: number[];
      committedRevision: number;
      insertRows: Array<{done: boolean; id: number; title: string}>;
      invalidations: Array<{revision: number; tables: string[]}>;
      orderedRows: Array<{done: boolean; id: number; title: string}>;
      preparedClosed: boolean;
      preparedRows: Array<{done: boolean; id: number; title: string}>;
      reopenedPriorities: Array<{id: number; priority: number}>;
      reopenedRevision: number;
      reopenedRows: Array<{done: boolean; id: number; title: string}>;
      rollbackCode: string;
      scriptFailureCode: string;
      scriptTableCode: string;
      stagedRows: Array<{done: boolean; id: number; title: string}>;
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
