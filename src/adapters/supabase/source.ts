import type {
  ReplicaSource,
  ReplicaSourceContext,
  SourceCapabilities,
} from '../types.js';
import type {Row, SerializedError, SyncPhase} from '../../protocol.js';
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

const DEFAULT_RETRY_DELAYS_MS = [250, 500, 1_000, 2_000, 5_000, 10_000] as const;
const MAX_RETRY_DELAY_MS = 60_000;

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
  readonly #tablesByRelation = new Map<string, NormalizedSupabaseTable>();
  readonly #expectedColumns = new Map<string, readonly string[]>();
  readonly #snapshotting = new Set<string>();
  readonly #dirty = new Set<string>();
  readonly #abortController = new AbortController();
  readonly #retryDelaysMs: readonly number[];
  readonly #sleep: (delayMs: number, signal: AbortSignal) => Promise<void>;
  #snapshotReader: SupabaseRestSnapshotReader | undefined;
  #context: ReplicaSourceContext | undefined;
  #connection: SupabaseRealtimeConnection | undefined;
  #snapshotAbortController: AbortController | undefined;
  #externalAbortListener: (() => void) | undefined;
  #eventQueue: Promise<void> = Promise.resolve();
  #reconcilePromise: Promise<void> | undefined;
  #reconcileAgain = false;
  #generation = 0;
  #subscribed = false;
  #live = false;
  #connecting = false;
  #closed = false;
  #started = false;

  constructor(options: CreateSupabaseSourceOptions) {
    this.#options = options;
    this.#config = normalizeSupabaseConfig(options);
    this.id = this.#config.id;
    this.#retryDelaysMs = normalizeRetryDelays(
      options.retryDelaysMs ?? DEFAULT_RETRY_DELAYS_MS,
    );
    this.#sleep = options.sleep ?? abortableSleep;
    for (const table of this.#config.tables) {
      this.#tablesByRelation.set(relationKey(table.schema, table.table), table);
      if (table.columns) {
        this.#expectedColumns.set(
          relationKey(table.schema, table.table),
          table.columns,
        );
      }
    }
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

    const accessToken = await this.#sampleAccessToken();
    this.#snapshotReader = new SupabaseRestSnapshotReader({
      url: this.#config.url,
      publishableKey: this.#config.publishableKey,
      pageSize: this.#config.pageSize,
      ...(this.#options.fetch ? {fetch: this.#options.fetch} : {}),
      getAccessToken: () => accessToken,
    });

    let attempt = 0;
    while (!this.#closed && !this.#abortController.signal.aborted) {
      try {
        this.#connecting = true;
        this.#subscribed = false;
        this.#live = false;
        this.#generation += 1;
        this.#connection = await this.#options.realtime.connect(
          {
            sourceId: this.id,
            tables: this.#config.tables,
            accessToken,
            signal: this.#abortController.signal,
          },
          {
            payload: (payload) => this.#receivePayload(payload),
            status: (status, error) => this.#receiveStatus(status, error),
          },
        );
        this.#connecting = false;
        if (!this.#subscribed) {
          throw new SupabaseSourceError(
            'SUPABASE_REALTIME_CONNECT_FAILED',
            'Supabase Realtime connected without confirming its subscriptions',
            true,
          );
        }
        await this.#reconcile('snapshotting');
        return;
      } catch (error) {
        this.#connecting = false;
        if (this.#closed || this.#abortController.signal.aborted) {
          throw abortedError();
        }
        await this.#discardConnection();
        this.#invalidateGeneration();
        if (!isRetryable(error)) {
          throw error;
        }
        this.#setFailureState(error, 'stale');
        await this.#sleep(
          retryDelay(this.#retryDelaysMs, attempt++),
          this.#abortController.signal,
        );
        if (!this.#closed && !this.#abortController.signal.aborted) {
          context.setSyncState({phase: 'connecting', sourceId: this.id});
        }
      }
    }
    throw abortedError();
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
    this.#live = false;
    this.#subscribed = false;
    this.#snapshotAbortController?.abort();
    this.#abortController.abort();
    if (this.#context && this.#externalAbortListener) {
      this.#context.signal.removeEventListener(
        'abort',
        this.#externalAbortListener,
      );
    }
    await this.#discardConnection();
    await this.#eventQueue.catch(() => undefined);
    await this.#reconcilePromise?.catch(() => undefined);
  }

  #receivePayload(payload: unknown): void {
    if (this.#closed || !this.#context || !this.#subscribed) {
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
      if (!this.#live || this.#snapshotting.has(relation!)) {
        this.#dirty.add(relation!);
        if (!this.#snapshotting.has(relation!)) {
          this.#reconcileAgain = true;
        }
        return;
      }

      const generation = this.#generation;
      this.#eventQueue = this.#eventQueue
        .then(async () => {
          if (
            this.#closed ||
            !this.#live ||
            !this.#subscribed ||
            generation !== this.#generation
          ) {
            this.#dirty.add(relation!);
            return;
          }
          await this.#context!.applyBatch(batch);
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
        return;
      }
      this.#subscribed = true;
      this.#generation += 1;
      if (!this.#connecting) {
        this.#requestReconcile();
      }
      return;
    }
    if (
      status === 'CHANNEL_ERROR' ||
      status === 'TIMED_OUT' ||
      status === 'CLOSED'
    ) {
      this.#invalidateGeneration();
      this.#setFailureState(
        error ??
          new SupabaseSourceError(
            'SUPABASE_REALTIME_DISCONNECTED',
            `Supabase Realtime entered ${status}`,
            true,
          ),
        isExplicitPermanent(error) ? 'error' : 'stale',
      );
    }
  }

  #handleIntegrityError(error: unknown): void {
    if (this.#closed || !this.#context) {
      return;
    }
    const remainedSubscribed = this.#subscribed;
    this.#live = false;
    this.#generation += 1;
    this.#snapshotAbortController?.abort();
    this.#markAllDirty();
    // An untrusted delta is repairable by a complete REST baseline even when
    // the malformed payload itself is not retryable as an incremental event.
    this.#setFailureState(error, 'stale');
    if (remainedSubscribed) {
      this.#requestReconcile();
    }
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
    this.#reconcilePromise = this.#runReconcileLoop(phase).finally(() => {
      this.#reconcilePromise = undefined;
    });
    return this.#reconcilePromise;
  }

  async #runReconcileLoop(
    initialPhase: Extract<SyncPhase, 'snapshotting' | 'resyncing'>,
  ): Promise<void> {
    let phase = initialPhase;
    let attempt = 0;
    while (!this.#closed && !this.#abortController.signal.aborted) {
      if (!this.#subscribed) {
        throw realtimeGapError();
      }
      this.#reconcileAgain = false;
      try {
        await this.#performReconcile(phase);
        if (!this.#reconcileAgain) {
          return;
        }
        phase = 'resyncing';
        attempt = 0;
      } catch (error) {
        if (this.#closed || this.#abortController.signal.aborted) {
          throw abortedError();
        }
        this.#live = false;
        const retryable = isRetryable(error);
        this.#setFailureState(error, retryable ? 'stale' : 'error');
        if (!retryable || !this.#subscribed) {
          throw error;
        }
        await this.#sleep(
          retryDelay(this.#retryDelaysMs, attempt++),
          this.#abortController.signal,
        );
        phase = 'resyncing';
      }
    }
    throw abortedError();
  }

  async #performReconcile(
    phase: Extract<SyncPhase, 'snapshotting' | 'resyncing'>,
  ): Promise<void> {
    const context = this.#context;
    if (!context || !this.#snapshotReader) {
      throw new SupabaseSourceError(
        'SUPABASE_SOURCE_NOT_STARTED',
        'The Supabase source is not ready to snapshot',
      );
    }
    const generation = this.#generation;
    this.#assertCurrentGeneration(generation);
    this.#live = false;
    context.setSyncState({phase, sourceId: this.id});

    const snapshotAbortController = new AbortController();
    this.#snapshotAbortController = snapshotAbortController;
    const abortSnapshot = () => snapshotAbortController.abort();
    this.#abortController.signal.addEventListener('abort', abortSnapshot, {
      once: true,
    });
    try {
      for (const table of this.#config.tables) {
        this.#assertCurrentGeneration(generation);
        await this.#snapshotTable(
          table,
          generation,
          snapshotAbortController.signal,
        );
      }
      this.#assertCurrentGeneration(generation);
      if (this.#dirty.size > 0 || this.#reconcileAgain) {
        throw new SupabaseSourceError(
          'SUPABASE_SNAPSHOT_DIRTY',
          'Supabase changed while the replica baseline was being rebuilt',
          true,
        );
      }
      this.#live = true;
      context.setSyncState({
        phase: 'live-best-effort',
        sourceId: this.id,
        lastReconciledAt: this.#options.now?.() ?? new Date().toISOString(),
      });
    } catch (error) {
      if (
        snapshotAbortController.signal.aborted &&
        !this.#abortController.signal.aborted
      ) {
        throw realtimeGapError();
      }
      throw error;
    } finally {
      this.#abortController.signal.removeEventListener(
        'abort',
        abortSnapshot,
      );
      if (this.#snapshotAbortController === snapshotAbortController) {
        this.#snapshotAbortController = undefined;
      }
    }
  }

  async #snapshotTable(
    table: NormalizedSupabaseTable,
    generation: number,
    signal: AbortSignal,
  ): Promise<void> {
    const relation = relationKey(table.schema, table.table);
    for (let pass = 1; pass <= this.#config.maxSnapshotPasses; pass += 1) {
      this.#assertCurrentGeneration(generation);
      this.#dirty.delete(relation);
      this.#snapshotting.add(relation);
      try {
        const rows: Row[] = [];
        for await (const page of this.#snapshotReader!.snapshot(table, signal)) {
          this.#assertCurrentGeneration(generation);
          rows.push(...page.rows);
          if (!this.#expectedColumns.has(relation) && page.rows[0]) {
            this.#expectedColumns.set(relation, Object.keys(page.rows[0]));
          }
        }
        this.#assertCurrentGeneration(generation);
        if (this.#dirty.has(relation)) {
          continue;
        }
        await this.#context!.replaceTable(toLocalSchema(table), rows);
        this.#assertCurrentGeneration(generation);
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

  #assertCurrentGeneration(generation: number): void {
    if (!this.#subscribed || generation !== this.#generation) {
      throw realtimeGapError();
    }
  }

  #invalidateGeneration(): void {
    this.#subscribed = false;
    this.#live = false;
    this.#generation += 1;
    this.#snapshotAbortController?.abort();
    this.#markAllDirty();
  }

  #markAllDirty(): void {
    for (const relation of this.#tablesByRelation.keys()) {
      this.#dirty.add(relation);
    }
    this.#reconcileAgain = true;
  }

  #setFailureState(
    error: unknown,
    phase: Extract<SyncPhase, 'stale' | 'error'>,
  ): void {
    this.#context?.setSyncState({
      phase,
      sourceId: this.id,
      error: serializeSourceError(error),
    });
  }

  async #sampleAccessToken(): Promise<string | null> {
    try {
      return (await this.#options.getAccessToken?.()) ?? null;
    } catch {
      throw new SupabaseSourceError(
        'SUPABASE_ACCESS_TOKEN_FAILED',
        'Could not obtain the Supabase access token',
      );
    }
  }

  async #discardConnection(): Promise<void> {
    const connection = this.#connection;
    this.#connection = undefined;
    if (connection) {
      await Promise.resolve()
        .then(() => connection.close())
        .catch(() => undefined);
    }
  }
}

export function createSupabaseSource(
  options: CreateSupabaseSourceOptions,
): SupabaseSource {
  return new SupabaseSource(options);
}

function serializeSourceError(error: unknown): SerializedError {
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

function isRetryable(error: unknown): boolean {
  return (
    typeof error === 'object' &&
    error !== null &&
    'retryable' in error &&
    error.retryable === true
  );
}

function isExplicitPermanent(error: unknown): boolean {
  return error instanceof SupabaseSourceError && !error.retryable;
}

function normalizeRetryDelays(delays: readonly number[]): readonly number[] {
  if (delays.length === 0) {
    throw new SupabaseSourceError(
      'SUPABASE_INVALID_CONFIG',
      'retryDelaysMs must contain at least one delay',
    );
  }
  return delays.map((delay) => {
    if (
      !Number.isSafeInteger(delay) ||
      delay < 0 ||
      delay > MAX_RETRY_DELAY_MS
    ) {
      throw new SupabaseSourceError(
        'SUPABASE_INVALID_CONFIG',
        `Source retry delays must be integers from 0 to ${MAX_RETRY_DELAY_MS}`,
      );
    }
    return delay;
  });
}

function retryDelay(delays: readonly number[], attempt: number): number {
  return delays[Math.min(attempt, delays.length - 1)]!;
}

function abortableSleep(delayMs: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal.aborted) {
      reject(abortedError());
      return;
    }
    const timeout = globalThis.setTimeout(finish, delayMs);
    signal.addEventListener('abort', abort, {once: true});

    function finish(): void {
      signal.removeEventListener('abort', abort);
      resolve();
    }

    function abort(): void {
      globalThis.clearTimeout(timeout);
      reject(abortedError());
    }
  });
}

function realtimeGapError(): SupabaseSourceError {
  return new SupabaseSourceError(
    'SUPABASE_REALTIME_GAP',
    'Supabase Realtime disconnected while rebuilding the replica baseline',
    true,
  );
}

function abortedError(): SupabaseSourceError {
  return new SupabaseSourceError(
    'SUPABASE_SOURCE_ABORTED',
    'Supabase synchronization was aborted',
  );
}
