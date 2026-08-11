import {
  createServer,
  type IncomingMessage,
  type ServerResponse,
} from 'node:http';
import type { AddressInfo } from 'node:net';
import { expect, type Page, test } from '@playwright/test';
import { type RawData, WebSocket, WebSocketServer } from 'ws';

const publishableKey = 'sb_publishable_browser_test';
const subscriptionId = 47;

test('snapshots, streams, reconciles, and reopens a source-bound OPFS cache', async ({ page }) => {
  const initialSnapshot = snapshotStep([
    { id: 1, title: 'initial remote snapshot' },
    { id: 2, title: 'second remote row' },
  ]);
  const reconnectSnapshot = snapshotStep([
    { id: 1, title: 'reconciled after socket gap' },
    { id: 2, title: 'second remote row' },
  ]);
  reconnectSnapshot.release.resolve();
  const reopenSnapshot = snapshotStep([
    { id: 1, title: 'next remote baseline' },
    { id: 2, title: 'second remote row' },
  ]);
  const server = await startSupabaseServer([
    initialSnapshot,
    reconnectSnapshot,
    reopenSnapshot,
  ]);
  try {
    await page.goto('/');
    await expect(page.getByTestId('state')).toHaveText('Ready');
    const databaseName = `supabase-${Date.now()}-${
      Math.floor(Math.random() * 1_000_000)
    }`;
    const options = {
      databaseName,
      publishableKey,
      url: server.url,
    };

    const beforeBaseline = await page.evaluate(
      (probeOptions) => window.__tinygresTest!.openSupabaseProbe(probeOptions),
      options,
    );
    expect(beforeBaseline.revision).toBe(0);
    expect(beforeBaseline.rows).toEqual([]);
    expect(beforeBaseline.state.phase).not.toBe('live-best-effort');

    await initialSnapshot.requested.promise;
    expect(server.restRequests).toHaveLength(1);
    initialSnapshot.release.resolve();

    const initial = await page.evaluate(() =>
      window.__tinygresTest!.waitForSupabaseProbe()
    );
    expect(initial.revision).toBe(1);
    expect(initial.rows).toEqual(initialSnapshot.rows);
    expect(initial.state.phase).toBe('live-best-effort');
    expect(initial.states.map(({ phase }) => phase)).toEqual(
      expect.arrayContaining([
        'connecting',
        'snapshotting',
        'live-best-effort',
      ]),
    );

    await expect.poll(() => server.sessions.length).toBe(1);
    sendUpdate(server.sessions[0]!, { id: 1, title: 'applied live update' });
    await expect
      .poll(() => readProbe(page))
      .toMatchObject({
        revision: 2,
        rows: [
          { id: 1, title: 'applied live update' },
          { id: 2, title: 'second remote row' },
        ],
        state: { phase: 'live-best-effort' },
      });

    server.sessions[0]!.socket.close(1012, 'deterministic test restart');
    await expect
      .poll(() => server.sessions.length, { timeout: 10_000 })
      .toBe(2);
    await reconnectSnapshot.requested.promise;
    const reconciled = await page.evaluate(() =>
      window.__tinygresTest!.waitForSupabaseProbe()
    );
    expect(reconciled.revision).toBe(3);
    expect(reconciled.rows).toEqual(reconnectSnapshot.rows);
    expect(reconciled.state.phase).toBe('live-best-effort');
    expect(reconciled.states.map(({ phase }) => phase)).toEqual(
      expect.arrayContaining(['stale', 'resyncing', 'live-best-effort']),
    );
    expect(server.restRequests).toHaveLength(4);

    await page.evaluate(() => window.__tinygresTest!.closeSupabaseProbe());
    const restoredBeforeBaseline = await page.evaluate(
      (probeOptions) => window.__tinygresTest!.openSupabaseProbe(probeOptions),
      options,
    );
    expect(restoredBeforeBaseline.revision).toBe(3);
    expect(restoredBeforeBaseline.rows).toEqual(reconnectSnapshot.rows);
    expect(restoredBeforeBaseline.state.phase).not.toBe('live-best-effort');

    await reopenSnapshot.requested.promise;
    expect(server.restRequests).toHaveLength(5);
    const stillRestored = await readProbe(page);
    expect(stillRestored.revision).toBe(3);
    expect(stillRestored.rows).toEqual(reconnectSnapshot.rows);
    reopenSnapshot.release.resolve();

    const afterReopenBaseline = await page.evaluate(() =>
      window.__tinygresTest!.waitForSupabaseProbe()
    );
    expect(afterReopenBaseline.revision).toBe(4);
    expect(afterReopenBaseline.rows).toEqual(reopenSnapshot.rows);
    expect(afterReopenBaseline.state.phase).toBe('live-best-effort');

    expect(server.restRequests).toEqual([
      expectedRestRequest(),
      expectedRestRequest('2-501'),
      expectedRestRequest(),
      expectedRestRequest('2-501'),
      expectedRestRequest(),
      expectedRestRequest('2-501'),
    ]);
    expect(server.sessions).toHaveLength(3);
    for (const session of server.sessions) {
      expect(session.endpoint).toEqual({
        apikey: publishableKey,
        path: '/realtime/v1/websocket',
        vsn: '1.0.0',
      });
      expect(session.join.payload).toMatchObject({
        config: {
          broadcast: { ack: false, replication_ready: true, self: false },
          presence: { enabled: false },
          private: false,
          postgres_changes: [
            {
              event: '*',
              schema: 'public',
              table: 'posts',
              select: ['id', 'title'],
            },
          ],
        },
      });
    }
  } finally {
    initialSnapshot.release.resolve();
    reconnectSnapshot.release.resolve();
    reopenSnapshot.release.resolve();
    await page
      .evaluate(() => window.__tinygresTest?.closeSupabaseProbe())
      .catch(() => undefined);
    await server.close();
  }
});

type SnapshotRow = { id: number; title: string };

type SnapshotStep = {
  rows: SnapshotRow[];
  requested: Deferred<void>;
  release: Deferred<void>;
};

type RestRequest = {
  acceptProfile: string | undefined;
  apikey: string | undefined;
  authorization: string | undefined;
  order: string | null;
  range: string | undefined;
  rangeUnit: string | undefined;
  select: string | null;
};

type PhoenixMessage = {
  event: string;
  join_ref: string | null;
  payload: Record<string, unknown>;
  ref: string | null;
  topic: string;
};

type RealtimeSession = {
  endpoint: {
    apikey: string | null;
    path: string;
    vsn: string | null;
  };
  join: PhoenixMessage & { ref: string };
  socket: WebSocket;
};

type SupabaseServer = {
  close(): Promise<void>;
  restRequests: RestRequest[];
  sessions: RealtimeSession[];
  url: string;
};

type Deferred<Value> = {
  promise: Promise<Value>;
  resolve(value: Value): void;
};

function snapshotStep(rows: SnapshotRow[]): SnapshotStep {
  return {
    rows,
    requested: deferred<void>(),
    release: deferred<void>(),
  };
}

async function startSupabaseServer(
  snapshots: SnapshotStep[],
): Promise<SupabaseServer> {
  const restRequests: RestRequest[] = [];
  const sessions: RealtimeSession[] = [];
  const snapshotState = {index: 0};
  const webSockets = new WebSocketServer({ noServer: true });
  const http = createServer((request, response) => {
    void handlePostgrest(
      request,
      response,
      snapshots,
      snapshotState,
      restRequests,
    ).catch(
      (error: unknown) => {
        if (!response.headersSent) {
          response.writeHead(500, {
            'Access-Control-Allow-Origin': '*',
            'Content-Type': 'application/json',
          });
        }
        if (!response.writableEnded) {
          response.end(
            JSON.stringify({
              message: error instanceof Error ? error.message : String(error),
            }),
          );
        }
      },
    );
  });

  http.on('upgrade', (request, socket, head) => {
    const endpoint = new URL(request.url ?? '/', 'http://localhost');
    if (endpoint.pathname !== '/realtime/v1/websocket') {
      socket.destroy();
      return;
    }
    webSockets.handleUpgrade(request, socket, head, (webSocket) => {
      webSockets.emit('connection', webSocket, request);
    });
  });
  webSockets.on('connection', (socket, request) => {
    installRealtimeSession(socket, request, sessions);
  });

  await new Promise<void>((resolve, reject) => {
    const onError = (error: Error) => reject(error);
    http.once('error', onError);
    http.listen(0, '127.0.0.1', () => {
      http.off('error', onError);
      resolve();
    });
  });
  const address = http.address() as AddressInfo;

  return {
    restRequests,
    sessions,
    url: `http://127.0.0.1:${address.port}`,
    async close() {
      for (const socket of webSockets.clients) {
        socket.terminate();
      }
      await new Promise<void>((resolve) => webSockets.close(() => resolve()));
      await new Promise<void>((resolve, reject) => {
        http.close((error) => (error ? reject(error) : resolve()));
      });
    },
  };
}

async function handlePostgrest(
  request: IncomingMessage,
  response: ServerResponse,
  snapshots: SnapshotStep[],
  state: {index: number},
  requests: RestRequest[],
): Promise<void> {
  const corsHeaders = {
    'Access-Control-Allow-Headers':
      'accept, accept-profile, apikey, authorization, range, range-unit',
    'Access-Control-Allow-Methods': 'GET, OPTIONS',
    'Access-Control-Allow-Origin': '*',
  };
  if (request.method === 'OPTIONS') {
    response.writeHead(204, corsHeaders);
    response.end();
    return;
  }

  const url = new URL(request.url ?? '/', 'http://localhost');
  if (request.method !== 'GET' || url.pathname !== '/rest/v1/posts') {
    response.writeHead(404, corsHeaders);
    response.end();
    return;
  }

  const step = snapshots[state.index];
  if (!step) {
    response.writeHead(500, {
      ...corsHeaders,
      'Content-Type': 'application/json',
    });
    response.end(JSON.stringify({ message: 'Unexpected additional snapshot' }));
    return;
  }
  const restRequest = {
    acceptProfile: header(request, 'accept-profile'),
    apikey: header(request, 'apikey'),
    authorization: header(request, 'authorization'),
    order: url.searchParams.get('order'),
    range: header(request, 'range'),
    rangeUnit: header(request, 'range-unit'),
    select: url.searchParams.get('select'),
  };
  requests.push(restRequest);
  const from = Number.parseInt(restRequest.range?.split('-', 1)[0] ?? '', 10);
  let rows: SnapshotRow[];
  if (from === 0) {
    step.requested.resolve();
    await step.release.promise;
    rows = step.rows;
  } else if (from === step.rows.length) {
    rows = [];
    state.index += 1;
  } else {
    response.writeHead(500, {
      ...corsHeaders,
      'Content-Type': 'application/json',
    });
    response.end(
      JSON.stringify({message: `Unexpected snapshot range ${restRequest.range}`}),
    );
    return;
  }
  if (response.destroyed || response.writableEnded) {
    return;
  }
  response.writeHead(200, {
    ...corsHeaders,
    'Content-Type': 'application/json',
  });
  response.end(JSON.stringify(rows));
}

function installRealtimeSession(
  socket: WebSocket,
  request: IncomingMessage,
  sessions: RealtimeSession[],
): void {
  const endpoint = new URL(request.url ?? '/', 'http://localhost');
  socket.on('message', (data) => {
    const message = parsePhoenixMessage(data);
    if (message.event === 'heartbeat') {
      socket.send(
        JSON.stringify({
          topic: 'phoenix',
          event: 'phx_reply',
          payload: { status: 'ok', response: {} },
          ref: message.ref,
          join_ref: null,
        }),
      );
      return;
    }
    if (message.event !== 'phx_join' || typeof message.ref !== 'string') {
      return;
    }
    sessions.push({
      endpoint: {
        apikey: endpoint.searchParams.get('apikey'),
        path: endpoint.pathname,
        vsn: endpoint.searchParams.get('vsn'),
      },
      join: { ...message, ref: message.ref },
      socket,
    });
    socket.send(
      JSON.stringify({
        topic: message.topic,
        event: 'phx_reply',
        payload: {
          status: 'ok',
          response: {
            postgres_changes: [
              {
                id: subscriptionId,
                event: '*',
                schema: 'public',
                table: 'posts',
              },
            ],
          },
        },
        ref: message.ref,
        join_ref: message.ref,
      }),
    );
    for (const payload of [
      {
        status: 'ok',
        extension: 'postgres_changes',
        message: 'Subscribed to PostgreSQL',
      },
      {
        status: 'ok',
        extension: 'system',
        message: 'Replication connection established',
      },
    ]) {
      socket.send(
        JSON.stringify({
          topic: message.topic,
          event: 'system',
          payload,
          ref: null,
          join_ref: message.ref,
        }),
      );
    }
  });
  socket.on('error', () => undefined);
}

function sendUpdate(session: RealtimeSession, row: SnapshotRow): void {
  session.socket.send(
    JSON.stringify({
      topic: session.join.topic,
      event: 'postgres_changes',
      payload: {
        ids: [subscriptionId],
        data: {
          schema: 'public',
          table: 'posts',
          type: 'UPDATE',
          record: row,
          old_record: { id: row.id },
          commit_timestamp: '2026-08-11T00:00:00.000Z',
          errors: null,
        },
      },
      ref: null,
      join_ref: session.join.ref,
    }),
  );
}

function parsePhoenixMessage(value: RawData): PhoenixMessage {
  return JSON.parse(value.toString()) as PhoenixMessage;
}

function expectedRestRequest(range = '0-499'): RestRequest {
  return {
    acceptProfile: 'public',
    apikey: publishableKey,
    authorization: undefined,
    order: 'id.asc',
    range,
    rangeUnit: 'items',
    select: 'id,title',
  };
}

function header(request: IncomingMessage, name: string): string | undefined {
  const value = request.headers[name];
  return Array.isArray(value) ? value.join(', ') : value;
}

function deferred<Value>(): Deferred<Value> {
  let resolvePromise!: (value: Value) => void;
  const promise = new Promise<Value>((resolve) => {
    resolvePromise = resolve;
  });
  return { promise, resolve: resolvePromise };
}

function readProbe(page: Page) {
  return page.evaluate(() => window.__tinygresTest!.readSupabaseProbe());
}
