import type {Change, ChangeBatch, Row} from '../../protocol.js';
import {relationKey} from './config.js';
import {assertPrimaryKey, isRow} from './rest.js';
import type {
  NormalizedSupabaseTable,
  SupabaseRealtimeConnection,
  SupabaseRealtimeObserver,
  SupabaseRealtimePayload,
  SupabaseRealtimeTransport,
} from './types.js';
import {SupabaseSourceError} from './types.js';

let channelSequence = 0;

export interface NormalizeSupabaseChangeOptions {
  readonly sourceId: string;
  readonly expectedColumns?: readonly string[];
}

export function normalizeSupabaseChange(
  value: unknown,
  table: NormalizedSupabaseTable,
  options: NormalizeSupabaseChangeOptions,
): ChangeBatch {
  if (!isPayload(value)) {
    throw invalidPayload('Realtime payload is not an object');
  }
  if (value.schema !== table.schema || value.table !== table.table) {
    throw invalidPayload(
      `Received change for unexpected relation \`${String(value.schema)}.${String(value.table)}\``,
    );
  }
  if (hasPayloadErrors(value.errors)) {
    throw invalidPayload(
      `Supabase reported a Realtime decoding error for \`${table.schema}.${table.table}\``,
    );
  }

  const eventType = value.eventType ?? value.type;
  const newRow = value.new ?? value.record;
  const oldRow = value.old ?? value.old_record;
  const committedAt =
    typeof value.commit_timestamp === 'string'
      ? value.commit_timestamp
      : undefined;

  let changes: Change[];
  switch (eventType) {
    case 'INSERT': {
      const row = projectRow(
        requireRow(newRow, table, options.expectedColumns),
        table.columns,
      );
      changes = [{type: 'upsert', table: table.localName, row}];
      break;
    }
    case 'UPDATE': {
      const previous = requireUpdateKey(oldRow, table);
      const row = projectRow(
        requireRow(newRow, table, options.expectedColumns),
        table.columns,
      );
      changes = [];
      const oldKey = keyRow(table, previous);
      const newKey = keyRow(table, row);
      if (!keysEqual(table, oldKey, newKey)) {
        changes.push({type: 'delete', table: table.localName, key: oldKey});
      }
      changes.push({type: 'upsert', table: table.localName, row});
      break;
    }
    case 'DELETE': {
      const row = requireRow(oldRow, table);
      changes = [
        {type: 'delete', table: table.localName, key: keyRow(table, row)},
      ];
      break;
    }
    default:
      throw invalidPayload(
        `Unsupported Supabase Realtime event type \`${String(eventType)}\``,
      );
  }

  return {
    sourceId: options.sourceId,
    ...(committedAt ? {committedAt} : {}),
    changes,
  };
}

/**
 * Wraps the public supabase-js channel API without importing it. Keeping the
 * SDK injected avoids making TinyGres core depend on Supabase or duplicating a
 * Supabase client already present in the host application.
 */
export function createSupabaseJsRealtimeTransport<
  Client extends {channel(name: string): unknown},
>(client: Client): SupabaseRealtimeTransport {
  return {
    async connect(options, observer) {
      const dynamicClient = client as unknown as {
        channel(name: string): unknown;
        realtime?: {setAuth?(token?: string): unknown};
      };
      if (dynamicClient.realtime?.setAuth) {
        await dynamicClient.realtime.setAuth(options.accessToken ?? undefined);
      }

      const channelName = `tinygres:${sanitizeTopic(options.sourceId)}:${++channelSequence}`;
      const channel = requireChannel(dynamicClient.channel(channelName));
      for (const table of options.tables) {
        channel.on(
          'postgres_changes',
          {
            event: '*',
            schema: table.schema,
            table: table.table,
            ...(table.columns ? {select: [...table.columns]} : {}),
          },
          (payload) => observer.payload(payload),
        );
      }
      return await subscribeChannel(channel, observer, options.signal);
    },
  };
}

interface ChannelLike {
  on(
    type: 'postgres_changes',
    filter: {
      event: '*';
      schema: string;
      table: string;
      select?: string[];
    },
    callback: (payload: unknown) => void,
  ): unknown;
  subscribe(callback: (status: string, error?: unknown) => void): unknown;
  unsubscribe(): unknown;
}

function subscribeChannel(
  channel: ChannelLike,
  observer: SupabaseRealtimeObserver,
  signal: AbortSignal,
): Promise<SupabaseRealtimeConnection> {
  return new Promise((resolve, reject) => {
    let settled = false;
    let closed = false;

    const close = async (): Promise<void> => {
      if (closed) {
        return;
      }
      closed = true;
      signal.removeEventListener('abort', onAbort);
      await channel.unsubscribe();
    };

    const onAbort = (): void => {
      void close();
      if (!settled) {
        settled = true;
        reject(
          new SupabaseSourceError(
            'SUPABASE_SOURCE_ABORTED',
            'Supabase Realtime connection was aborted',
          ),
        );
      }
    };

    if (signal.aborted) {
      onAbort();
      return;
    }
    signal.addEventListener('abort', onAbort, {once: true});

    try {
      channel.subscribe((status, error) => {
        if (closed) {
          return;
        }
        observer.status(status, error);
        if (status === 'SUBSCRIBED' && !settled) {
          settled = true;
          resolve({close});
          return;
        }
        if (
          !settled &&
          (status === 'CHANNEL_ERROR' ||
            status === 'TIMED_OUT' ||
            status === 'CLOSED')
        ) {
          settled = true;
          void close();
          reject(
            new SupabaseSourceError(
              'SUPABASE_REALTIME_CONNECT_FAILED',
              `Supabase Realtime connection entered ${status}${formatError(error)}`,
              true,
            ),
          );
        }
      });
    } catch (error) {
      void close();
      reject(
        new SupabaseSourceError(
          'SUPABASE_REALTIME_CONNECT_FAILED',
          `Could not subscribe to Supabase Realtime${formatError(error)}`,
          true,
        ),
      );
    }
  });
}

function requireRow(
  value: unknown,
  table: NormalizedSupabaseTable,
  expectedColumns?: readonly string[],
): Row {
  if (!isRow(value)) {
    throw invalidPayload(
      `Realtime change for \`${table.schema}.${table.table}\` does not contain a valid JSON row`,
    );
  }
  assertPrimaryKey(table, value);
  if (expectedColumns) {
    const missing = expectedColumns.filter((column) => !(column in value));
    if (missing.length > 0) {
      throw invalidPayload(
        `Realtime change for \`${table.schema}.${table.table}\` is missing columns: ${missing.join(', ')}`,
      );
    }
  }
  return value;
}

function projectRow(row: Row, columns?: readonly string[]): Row {
  return columns
    ? Object.fromEntries(columns.map((column) => [column, row[column]!]))
    : row;
}

function requireUpdateKey(
  value: unknown,
  table: NormalizedSupabaseTable,
): Row {
  if (!isRow(value) || !hasCompletePrimaryKey(table, value)) {
    throw invalidPayload(
      `Realtime UPDATE for \`${table.schema}.${table.table}\` does not contain the previous primary key; enable REPLICA IDENTITY FULL so TinyGres can reconcile key changes`,
    );
  }
  return value;
}

function hasCompletePrimaryKey(
  table: NormalizedSupabaseTable,
  row: Row,
): boolean {
  return table.primaryKey.every((column) => column in row && row[column] !== null);
}

function keyRow(table: NormalizedSupabaseTable, row: Row): Row {
  assertPrimaryKey(table, row);
  return Object.fromEntries(
    table.primaryKey.map((column) => [column, row[column]!]),
  );
}

function keysEqual(
  table: NormalizedSupabaseTable,
  left: Row,
  right: Row,
): boolean {
  return table.primaryKey.every(
    (column) => JSON.stringify(left[column]) === JSON.stringify(right[column]),
  );
}

function isPayload(value: unknown): value is SupabaseRealtimePayload {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function hasPayloadErrors(value: unknown): boolean {
  if (value === undefined || value === null || value === '') {
    return false;
  }
  return !Array.isArray(value) || value.length > 0;
}

function invalidPayload(message: string): SupabaseSourceError {
  return new SupabaseSourceError('SUPABASE_INVALID_REALTIME_PAYLOAD', message);
}

function requireChannel(value: unknown): ChannelLike {
  if (
    typeof value !== 'object' ||
    value === null ||
    !('on' in value) ||
    typeof value.on !== 'function' ||
    !('subscribe' in value) ||
    typeof value.subscribe !== 'function' ||
    !('unsubscribe' in value) ||
    typeof value.unsubscribe !== 'function'
  ) {
    throw new SupabaseSourceError(
      'SUPABASE_REALTIME_CLIENT_INVALID',
      'Injected Supabase client does not expose the expected channel API',
    );
  }
  return value as ChannelLike;
}

function sanitizeTopic(value: string): string {
  return value.replace(/[^A-Za-z0-9:_-]/g, '_').slice(0, 80);
}

function formatError(error: unknown): string {
  if (error instanceof Error && error.message) {
    return `: ${error.message}`;
  }
  return error === undefined ? '' : `: ${String(error)}`;
}

export function payloadRelation(value: unknown): string | undefined {
  if (!isPayload(value)) {
    return undefined;
  }
  return typeof value.schema === 'string' && typeof value.table === 'string'
    ? relationKey(value.schema, value.table)
    : undefined;
}
