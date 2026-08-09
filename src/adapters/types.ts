import type {
  ApplyOutcome,
  ChangeBatch,
  Row,
  SyncState,
  TableSchema,
} from '../protocol.js';

export interface SourceCapabilities {
  snapshotConsistency: 'eventual' | 'cursor-aligned';
  changes: 'best-effort' | 'durable';
  resume: 'none' | 'cursor';
  atomicity: 'row' | 'transaction';
  writes: boolean;
}

export interface ReplicaSourceContext {
  readonly signal: AbortSignal;
  defineTable(schema: TableSchema): Promise<void>;
  replaceTable(
    schema: TableSchema,
    rows: Row[],
  ): Promise<ApplyOutcome>;
  applyBatch(batch: ChangeBatch): Promise<ApplyOutcome>;
  setSyncState(state: SyncState): void;
}

export interface ReplicaSource {
  readonly id: string;
  readonly capabilities: SourceCapabilities;
  start(context: ReplicaSourceContext): Promise<void>;
  close?(): Promise<void>;
}
