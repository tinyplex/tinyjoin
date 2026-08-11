import {isRecord, type Row} from '../../protocol.js';
import type {
  MaybePromise,
  NormalizedSupabaseTable,
  SupabaseRestSnapshotOptions,
  SupabaseSnapshotPage,
} from './types.js';
import {SupabaseSourceError} from './types.js';

export class SupabaseRestSnapshotReader {
  readonly #apiRoot: URL;
  readonly #publishableKey: string;
  readonly #pageSize: number;
  readonly #fetch: typeof globalThis.fetch;
  readonly #getAccessToken: () => MaybePromise<string | null>;
  #accessTokenPromise: Promise<string | null> | undefined;

  constructor(options: SupabaseRestSnapshotOptions) {
    this.#apiRoot = new URL(
      'rest/v1/',
      `${options.url.replace(/\/+$/, '')}/`,
    );
    this.#publishableKey = options.publishableKey;
    this.#pageSize = options.pageSize ?? 500;
    this.#fetch = options.fetch ?? globalThis.fetch;
    this.#getAccessToken = options.getAccessToken ?? (() => null);
    if (typeof this.#fetch !== 'function') {
      throw new SupabaseSourceError(
        'SUPABASE_FETCH_UNAVAILABLE',
        'Supabase snapshots require the Fetch API',
      );
    }
  }

  async *snapshot(
    table: NormalizedSupabaseTable,
    signal: AbortSignal,
  ): AsyncGenerator<SupabaseSnapshotPage> {
    let page = 0;
    while (true) {
      throwIfAborted(signal);
      const from = page * this.#pageSize;
      const to = from + this.#pageSize - 1;
      const rows = await this.#fetchPage(table, from, to, signal);
      const done = rows.length < this.#pageSize;
      yield {page, rows, done};
      if (done) {
        return;
      }
      page += 1;
    }
  }

  async #fetchPage(
    table: NormalizedSupabaseTable,
    from: number,
    to: number,
    signal: AbortSignal,
  ): Promise<Row[]> {
    const url = new URL(encodeURIComponent(table.table), this.#apiRoot);
    url.searchParams.set('select', table.columns?.join(',') ?? '*');
    url.searchParams.set(
      'order',
      table.primaryKey.map((column) => `${column}.asc`).join(','),
    );

    const accessToken = await this.#accessToken();
    throwIfAborted(signal);
    let response: Response;
    try {
      response = await this.#fetch(url, {
        cache: 'no-store',
        headers: {
          Accept: 'application/json',
          'Accept-Profile': table.schema,
          Range: `${from}-${to}`,
          'Range-Unit': 'items',
          apikey: this.#publishableKey,
          ...(accessToken ? {Authorization: `Bearer ${accessToken}`} : {}),
        },
        signal,
      });
    } catch {
      throwIfAborted(signal);
      throw new SupabaseSourceError(
        'SUPABASE_SNAPSHOT_FAILED',
        `Supabase snapshot for \`${table.schema}.${table.table}\` could not reach the Data API`,
        true,
      );
    }
    throwIfAborted(signal);

    if (!response.ok) {
      throw new SupabaseSourceError(
        'SUPABASE_SNAPSHOT_FAILED',
        `Supabase snapshot for \`${table.schema}.${table.table}\` failed with HTTP ${response.status}`,
        response.status >= 500 ||
          response.status === 408 ||
          response.status === 425 ||
          response.status === 429,
      );
    }

    let value: unknown;
    try {
      value = await response.json();
    } catch {
      throwIfAborted(signal);
      throw new SupabaseSourceError(
        'SUPABASE_INVALID_SNAPSHOT',
        `Supabase snapshot for \`${table.schema}.${table.table}\` returned invalid JSON`,
      );
    }
    throwIfAborted(signal);
    if (!Array.isArray(value) || !value.every(isRow)) {
      throw new SupabaseSourceError(
        'SUPABASE_INVALID_SNAPSHOT',
        `Supabase snapshot for \`${table.schema}.${table.table}\` did not return an array of JSON rows`,
      );
    }
    for (const row of value) {
      assertPrimaryKey(table, row);
    }
    return value;
  }

  #accessToken(): Promise<string | null> {
    this.#accessTokenPromise ??= Promise.resolve().then(async () => {
      const token = await this.#getAccessToken();
      return token ?? null;
    });
    return this.#accessTokenPromise;
  }
}

export function isRow(value: unknown): value is Row {
  return isRecord(value) && Object.values(value).every(isJsonValue);
}

export function isJsonValue(value: unknown): boolean {
  if (
    value === null ||
    typeof value === 'boolean' ||
    typeof value === 'string'
  ) {
    return true;
  }
  if (typeof value === 'number') {
    return Number.isFinite(value);
  }
  if (Array.isArray(value)) {
    return value.every(isJsonValue);
  }
  return isRecord(value) && Object.values(value).every(isJsonValue);
}

export function assertPrimaryKey(
  table: NormalizedSupabaseTable,
  row: Row,
): void {
  for (const column of table.primaryKey) {
    if (!(column in row) || row[column] === null) {
      throw new SupabaseSourceError(
        'SUPABASE_INVALID_ROW',
        `Row for \`${table.schema}.${table.table}\` is missing non-null primary-key column \`${column}\``,
      );
    }
  }
}

function throwIfAborted(signal: AbortSignal): void {
  if (signal.aborted) {
    throw new SupabaseSourceError(
      'SUPABASE_SOURCE_ABORTED',
      'Supabase synchronization was aborted',
    );
  }
}
