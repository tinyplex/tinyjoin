import type {
  ReplicaSource,
  ReplicaSourceContext,
  SourceCapabilities,
} from '../types.js';
import type {Row, SerializedTinygresError, SyncPhase} from '../../protocol.js';
import {
  normalizeSupabaseConfig,
  relationKey,
  toLocalSchema,
  type NormalizedSupabaseSourceConfig,
} from './config.js';
import {
  normalizeSupabaseChange,
  payloadRelation,
} from './realtime.js';
import {SupabaseRestSnapshotReader} from './rest.js';
import type {
  CreateSupabaseSourceOptions,
  NormalizedSupabaseTable,
  SupabaseRealtimeConnection,
} from './types.js';
import {SupabaseSourceError} from './types.js';

export const SUPABASE_SOURCE_CAPABILITIES = Object.freeze({
  snapshotConsistency: 'eventual',
  changes: 'best-effort',
  resume: 'none',
  atomicity: 'row',
  writes: false,
} satisfies SourceCapabilities);

export class SupabaseSource implements ReplicaSource {
  readonly id: string;
  readonly capabilities = SUPABASE_SOURCE_CAPABILITIES;
  readonly #options: CreateSupabaseSourceOptions;
  readonly #config: NormalizedSupabaseSourceConfig;
  readonly #snapshotReader: SupabaseRestSnapshotReader;
  readonly #tablesByRelation = new Map<string, NormalizedSupabaseTable>();
  readonly #expectedColumns = new Map<string, readonly string[]>();
  readonly #snapshotting = new Set<string>();
  readonly #dirty = new Set<string>();
  readonly #abortController = new AbortController();
  #context: ReplicaSourceContext | undefined;
  #connection: SupabaseRealtimeConnection | undefined;
  #externalAbortListener: (() => void) | undefined;
  #eventQueue: Promise<void> = Promise.resolve();
  #reconcilePromise: Promise<void> | undefined;
  #reconcileAgain = false;
  #subscribed = false;
  #closed = false;
  #started = false;

  constructor(options: CreateSupabaseSourceOptions) {
    this.#options = options;
    this.#config = normalizeSupabaseConfig(options);
    this.id = this.#config.id;
    for (const table of this.#config.tables) {
      this.#tablesByRelation.set(relationKey(table.schema, table.table), table);
      if (table.columns) {
        this.#expectedColumns.set(
          relationKey(table.schema, table.table),
          table.columns,
        );
      }
    }
    this.#snapshotReader = new SupabaseRestSnapshotReader({
      url: this.#config.url,
      publishableKey: this.#config.publishableKey,
      pageSize: this.#config.pageSize,
      ...(options.fetch ? {fetch: options.fetch} : {}),
      ...(options.getAccessToken
        ? {getAccessToken: options.getAccessToken}
        : {}),
    });
  }

  async start(context: ReplicaSourceContext): Promise<void> {
    if (this.#started) {
      throw new SupabaseSourceError(
        'SUPABASE_SOURCE_ALREADY_STARTED',
        'A Supabase source can only be started once',
      );
    }
    this.#started = true;
    this.#context = context;
    if (context.signal.aborted) {
      this.#abortController.abort();
      throw abortedError();
    }
    this.#externalAbortListener = () => this.#abortController.abort();
    context.signal.addEventListener('abort', this.#externalAbortListener, {
      once: true,
    });

    for (const table of this.#config.tables) {
      await context.defineTable(toLocalSchema(table));
    }

    const accessToken = await this.#options.getAccessToken?.();
    this.#connection = await this.#options.realtime.connect(
      {
        sourceId: this.id,
        tables: this.#config.tables,
        accessToken: accessToken ?? null,
        signal: this.#abortController.signal,
      },
      {
        payload: (payload) => this.#receivePayload(payload),
        status: (status, error) => this.#receiveStatus(status, error),
      },
    );

    await this.#reconcile('snapshotting');
  }

  async reconcile(): Promise<void> {
    if (!this.#context || !this.#started) {
      throw new SupabaseSourceError(
        'SUPABASE_SOURCE_NOT_STARTED',
        'The Supabase source must be started before it can reconcile',
      );
    }
    await this.#reconcile('resyncing');
  }

  async close(): Promise<void> {
    if (this.#closed) {
      return;
    }
    this.#closed = true;
    this.#abortController.abort();
    if (this.#context && this.#externalAbortListener) {
      this.#context.signal.removeEventListener(
        'abort',
        this.#externalAbortListener,
      );
    }
    await this.#connection?.close();
    await this.#eventQueue.catch(() => undefined);
  }

  #receivePayload(payload: unknown): void {
    if (this.#closed || !this.#context) {
      return;
    }
    const relation = payloadRelation(payload);
    const table = relation ? this.#tablesByRelation.get(relation) : undefined;
    if (!table) {
      this.#handleIntegrityError(
        new SupabaseSourceError(
          'SUPABASE_INVALID_REALTIME_PAYLOAD',
          'Received a Supabase change for an unconfigured relation',
        ),
      );
      return;
    }

    try {
      const expectedColumns = this.#expectedColumns.get(relation!);
      const batch = normalizeSupabaseChange(payload, table, {
        sourceId: this.id,
        ...(expectedColumns ? {expectedColumns} : {}),
      });
      if (this.#snapshotting.has(relation!)) {
        this.#dirty.add(relation!);
        return;
      }
      this.#eventQueue = this.#eventQueue
        .then(async () => {
          if (!this.#closed) {
            await this.#context!.applyBatch(batch);
          }
        })
        .catch((error: unknown) => this.#handleIntegrityError(error));
    } catch (error) {
      this.#handleIntegrityError(error);
    }
  }

  #receiveStatus(status: string, error?: unknown): void {
    if (this.#closed || !this.#context) {
      return;
    }
    if (status === 'SUBSCRIBED') {
      if (this.#subscribed) {
        this.#requestReconcile();
      } else {
        this.#subscribed = true;
      }
      return;
    }
    if (
      status === 'CHANNEL_ERROR' ||
      status === 'TIMED_OUT' ||
      status === 'CLOSED'
    ) {
      this.#context.setSyncState({
        phase: 'stale',
        sourceId: this.id,
        error: serializeSourceError(
          error ??
            new SupabaseSourceError(
              'SUPABASE_REALTIME_DISCONNECTED',
              `Supabase Realtime entered ${status}`,
              true,
            ),
        ),
      });
    }
  }

  #handleIntegrityError(error: unknown): void {
    if (this.#closed || !this.#context) {
      return;
    }
    this.#context.setSyncState({
      phase: 'stale',
      sourceId: this.id,
      error: serializeSourceError(error),
    });
    this.#requestReconcile();
  }

  #requestReconcile(): void {
    void this.#reconcile('resyncing').catch(() => undefined);
  }

  #reconcile(
    phase: Extract<SyncPhase, 'snapshotting' | 'resyncing'>,
  ): Promise<void> {
    if (this.#reconcilePromise) {
      this.#reconcileAgain = true;
      return this.#reconcilePromise;
    }
    this.#reconcilePromise = this.#runReconcileLoop(phase)
      .catch((error: unknown) => {
        if (
          !this.#closed &&
          !this.#abortController.signal.aborted &&
          this.#context
        ) {
          this.#context.setSyncState({
            phase: 'stale',
            sourceId: this.id,
            error: serializeSourceError(error),
          });
        }
        throw error;
      })
      .finally(() => {
        this.#reconcilePromise = undefined;
      });
    return this.#reconcilePromise;
  }

  async #runReconcileLoop(
    initialPhase: Extract<SyncPhase, 'snapshotting' | 'resyncing'>,
  ): Promise<void> {
    let phase = initialPhase;
    do {
      this.#reconcileAgain = false;
      await this.#performReconcile(phase);
      phase = 'resyncing';
    } while (this.#reconcileAgain && !this.#abortController.signal.aborted);
  }

  async #performReconcile(
    phase: Extract<SyncPhase, 'snapshotting' | 'resyncing'>,
  ): Promise<void> {
    const context = this.#context;
    if (!context || this.#closed || this.#abortController.signal.aborted) {
      throw abortedError();
    }
    context.setSyncState({phase, sourceId: this.id});

    for (const table of this.#config.tables) {
      await this.#snapshotTable(table);
    }

    context.setSyncState({
      phase: 'live-best-effort',
      sourceId: this.id,
      lastReconciledAt: this.#options.now?.() ?? new Date().toISOString(),
    });
  }

  async #snapshotTable(table: NormalizedSupabaseTable): Promise<void> {
    const relation = relationKey(table.schema, table.table);
    for (let pass = 1; pass <= this.#config.maxSnapshotPasses; pass += 1) {
      this.#dirty.delete(relation);
      this.#snapshotting.add(relation);
      try {
        const rows: Row[] = [];
        for await (const page of this.#snapshotReader.snapshot(
          table,
          this.#abortController.signal,
        )) {
          rows.push(...page.rows);
          if (!this.#expectedColumns.has(relation) && page.rows[0]) {
            this.#expectedColumns.set(relation, Object.keys(page.rows[0]));
          }
        }
        if (this.#dirty.has(relation)) {
          continue;
        }
        await this.#context!.replaceTable(toLocalSchema(table), rows);
        if (!this.#dirty.has(relation)) {
          return;
        }
      } finally {
        this.#snapshotting.delete(relation);
      }
    }

    throw new SupabaseSourceError(
      'SUPABASE_SNAPSHOT_NEVER_SETTLED',
      `Supabase relation \`${relation}\` changed during every snapshot attempt`,
      true,
    );
  }
}

export function createSupabaseSource(
  options: CreateSupabaseSourceOptions,
): SupabaseSource {
  return new SupabaseSource(options);
}

function serializeSourceError(error: unknown): SerializedTinygresError {
  if (error instanceof SupabaseSourceError) {
    return {
      code: error.code,
      message: error.message,
      retryable: error.retryable,
    };
  }
  if (error instanceof Error) {
    return {code: 'SUPABASE_SOURCE_FAILED', message: error.message};
  }
  return {code: 'SUPABASE_SOURCE_FAILED', message: String(error)};
}

function abortedError(): SupabaseSourceError {
  return new SupabaseSourceError(
    'SUPABASE_SOURCE_ABORTED',
    'Supabase synchronization was aborted',
  );
}
