import {asCodedError, isRecord} from '../common.js';
import type {WorkerRpc} from '../client/rpc.js';
import {
  PROTOCOL_VERSION,
  isWorkerEvent,
  isWorkerRequest,
  isWorkerResponse,
  type SerializedError,
  type StorageOptions,
  type WorkerRequest,
  type WorkerResponse,
} from '../protocol.js';
import {createDatabaseBroker} from './database-broker.js';
import {
  clientChannel,
  COORDINATION_COMPATIBILITY,
  coordinationError,
  databaseChannel,
  leaderChanged,
  MAX_PENDING_REQUESTS,
  MAX_QUEUED_BYTES,
  ownerChannel,
  type Announcement,
  type GroupMessage,
  type RoutedRequest,
} from './coordination-protocol.js';
import {startWorker, type WorkerScope} from './host.js';
import {createLocalRpc} from './local-rpc.js';
import {requestBytes} from './request-size.js';

type Pending = {request: WorkerRequest; bytes: number; epoch?: string};

/** Memory clients keep their private host; persistent clients join one owner. */
export const startCoordinatedWorker = (): void => {
  const scope = globalThis as unknown as WorkerScope;
  const firstMessage = (event: MessageEvent<unknown>): void => {
    scope.removeEventListener('message', firstMessage);
    if (
      isWorkerRequest(event.data) &&
      event.data.method === 'init' &&
      event.data.params.storage.kind === 'opfs'
    ) {
      startCoordinator(scope, event.data);
    } else {
      // Deliver the first message to the newly installed host without altering
      // the public/custom Worker protocol.
      let receive: ((event: MessageEvent<unknown>) => void) | undefined;
      startWorker({
        scope: {
          postMessage: (message) => scope.postMessage(message),
          addEventListener: (type, listener) => {
            receive = listener;
            scope.addEventListener(type, listener);
          },
          removeEventListener: (type, listener) =>
            scope.removeEventListener(type, listener),
          close: () => scope.close(),
        },
      });
      receive?.(event);
    }
  };
  scope.addEventListener('message', firstMessage);
};

const startCoordinator = (
  scope: WorkerScope,
  init: Extract<WorkerRequest, {method: 'init'}>,
): void => {
  const storage = init.params.storage as Extract<
    StorageOptions,
    {kind: 'opfs'}
  >;
  if (!globalThis.navigator?.locks || typeof BroadcastChannel === 'undefined') {
    scope.postMessage({
      v: PROTOCOL_VERSION,
      id: init.id,
      ok: false,
      error: coordinationError(
        'MULTI_TAB_UNAVAILABLE',
        'Persistent TinyJoin requires Web Locks and BroadcastChannel in a secure browser context',
      ),
    });
    scope.close();
    return;
  }
  const client = crypto.randomUUID();
  const group = new BroadcastChannel(databaseChannel(storage.name));
  const inbox = new BroadcastChannel(clientChannel(client));
  const pending = new Map<number, Pending>();
  const prepared = new Map<number, string>();
  const election = new AbortController();
  let leader: Announcement | undefined;
  let outgoing: BroadcastChannel | undefined;
  let ownerInbox: BroadcastChannel | undefined;
  let owner: ReturnType<typeof createDatabaseBroker> | undefined;
  let engine: WorkerRpc | undefined;
  let releaseLeader: (() => void) | undefined;
  let releaseClient: (() => void) | undefined;
  let closed = false;
  let initialized = false;
  let refreshRequested = false;
  let terminalError: SerializedError | undefined;
  let localRevision = 0;
  let announcementSequence = 0;
  let acceptedSequence = 0;
  let pendingBytes = 0;
  let preparedBytes = 0;

  const removePending = (id: number): void => {
    const entry = pending.get(id);
    if (entry) pendingBytes -= entry.bytes;
    pending.delete(id);
  };

  const fail = (id: number, error: SerializedError): void => {
    removePending(id);
    scope.postMessage({v: PROTOCOL_VERSION, id, ok: false, error});
  };
  const failPending = (
    error: SerializedError,
    dispatchedOnly = false,
  ): void => {
    for (const [id, entry] of pending)
      if (!dispatchedOnly || entry.epoch) fail(id, error);
  };
  const resync = (revision: number): void => {
    if (initialized)
      scope.postMessage({
        v: PROTOCOL_VERSION,
        event: 'resync',
        payload: {revision, tables: [], keys: {}},
      });
  };
  const shutdown = async (): Promise<void> => {
    if (closed) return;
    closed = true;
    if (owner && leader) group.postMessage({...leader, ready: false});
    scope.removeEventListener('message', receive);
    election.abort();
    owner?.close();
    ownerInbox?.close();
    outgoing?.close();
    // Close the sync file handle before releasing the election lock.
    try {
      await engine?.request('close', undefined);
    } finally {
      engine?.dispose();
      releaseLeader?.();
      releaseClient?.();
      group.close();
      inbox.close();
    }
  };
  const finishClose = (message: WorkerResponse): void => {
    void shutdown()
      .then(
        () => scope.postMessage(message),
        (error: unknown) =>
          scope.postMessage({
            v: PROTOCOL_VERSION,
            id: message.id,
            ok: false,
            error:
              asCodedError(error) ??
              coordinationError('WORKER_OPERATION_FAILED', String(error)),
          }),
      )
      .finally(() => scope.close());
  };
  const response = (message: WorkerResponse): void => {
    const entry = pending.get(message.id);
    if (!entry) return;
    removePending(message.id);
    if (message.ok) {
      if (entry.request.method === 'init') initialized = true;
      if (
        entry.request.method === 'prepareSql' &&
        isRecord(message.result) &&
        typeof message.result.statementId === 'number'
      ) {
        prepared.set(message.result.statementId, entry.request.params.sql);
        preparedBytes += entry.request.params.sql.length * 2;
      }
      if (entry.request.method === 'closePrepared') {
        const sql = prepared.get(entry.request.params.statementId);
        if (sql !== undefined) preparedBytes -= sql.length * 2;
        prepared.delete(entry.request.params.statementId);
      }
    }
    if (entry.request.method === 'close') {
      finishClose(message);
    } else scope.postMessage(message);
  };
  const dispatch = (entry: Pending): void => {
    if (closed || !releaseClient || entry.epoch || !leader?.ready) return;
    const request = entry.request;
    if (
      request.params &&
      'transactionId' in request.params &&
      request.params.transactionId &&
      !request.params.transactionId.startsWith(`${leader.epoch}/`)
    ) {
      fail(
        request.id,
        coordinationError(
          'TRANSACTION_LOST',
          'The TinyJoin transaction ended when its database owner disconnected',
        ),
      );
      return;
    }
    const sql =
      request.method === 'executePrepared'
        ? prepared.get(request.params.statementId)
        : undefined;
    const message: RoutedRequest = {
      kind: 'request',
      client,
      epoch: leader.epoch,
      compatibility: COORDINATION_COMPATIBILITY,
      request,
      ...(sql !== undefined ? {statementSql: sql} : {}),
      ...(request.method === 'beginTransaction'
        ? {statements: [...prepared]}
        : {}),
    };
    entry.epoch = leader.epoch;
    if (owner) owner.receive(message);
    else outgoing?.postMessage(message);
  };
  const announce = (message: Announcement): void => {
    if (closed) return;
    if (message.compatibility !== COORDINATION_COMPATIBILITY) {
      failPending(leaderChanged(), true);
      terminalError = coordinationError(
        'DATABASE_VERSION_MISMATCH',
        'Another tab is using an incompatible TinyJoin release. Close the old tabs and reload.',
      );
      failPending(terminalError);
      return;
    }
    const changed = leader?.epoch !== message.epoch;
    if (changed) {
      failPending(leaderChanged(), true);
      outgoing?.close();
      outgoing = new BroadcastChannel(ownerChannel(message.epoch));
      refreshRequested = initialized;
    }
    leader = message;
    if (message.ready) {
      for (const entry of pending.values()) dispatch(entry);
      if (refreshRequested) {
        refreshRequested = false;
        resync(message.revision);
      }
    }
  };
  const publish = (message: GroupMessage): void => {
    group.postMessage(message);
    if (message.kind === 'leader') announce(message);
    else onGroup(message);
  };
  const onGroup = (message: unknown): void => {
    if (!isRecord(message)) return;
    if (message.kind === 'discover') {
      if (releaseLeader && leader) group.postMessage(leader);
    } else if (
      message.kind === 'leader' &&
      typeof message.epoch === 'string' &&
      typeof message.compatibility === 'string' &&
      typeof message.ready === 'boolean' &&
      typeof message.revision === 'number' &&
      typeof message.ownerId === 'string'
    ) {
      // Broadcast deliveries from different Workers can race during handover.
      // Accept an announcement only from the current browser lock owner, so a
      // delayed old announcement cannot revive an obsolete epoch or version.
      const sequence = ++announcementSequence;
      void navigator.locks
        .query()
        .then(({held}) => {
          if (
            sequence > acceptedSequence &&
            held?.some(
              (lock) =>
                lock.name === databaseChannel(storage.name) &&
                lock.clientId === message.ownerId,
            )
          ) {
            acceptedSequence = sequence;
            announce(message as Announcement);
          }
        })
        .catch(() => undefined);
    } else if (
      message.kind === 'event' &&
      message.epoch === leader?.epoch &&
      isWorkerEvent(message.event)
    ) {
      localRevision = Math.max(localRevision, message.event.payload.revision);
      if (leader) leader.revision = localRevision;
      scope.postMessage(message.event);
    } else if (
      message.kind === 'failed' &&
      message.epoch === leader?.epoch &&
      isRecord(message.error) &&
      typeof message.error.code === 'string' &&
      typeof message.error.message === 'string'
    ) {
      terminalError = message.error as unknown as SerializedError;
      failPending(terminalError);
    }
  };
  const receive = (event: MessageEvent<unknown>): void => {
    if (closed) return;
    if (isRecord(event.data) && event.data.tinyjoin === 'resync') {
      refreshRequested = true;
      if (owner && leader) {
        resync(localRevision);
        refreshRequested = false;
      } else group.postMessage({kind: 'discover'});
      return;
    }
    if (!isWorkerRequest(event.data)) {
      scope.postMessage({
        v: PROTOCOL_VERSION,
        id:
          isRecord(event.data) && typeof event.data.id === 'number'
            ? event.data.id
            : 0,
        ok: false,
        error: coordinationError(
          'PROTOCOL_MISMATCH',
          'The worker received an invalid TinyJoin protocol request',
        ),
      });
      return;
    }
    const request = event.data;
    if (request.method === 'close' && (terminalError || !leader?.ready)) {
      finishClose({
        v: PROTOCOL_VERSION,
        id: request.id,
        ok: true,
        result: undefined,
      });
      return;
    }
    if (terminalError) {
      fail(request.id, terminalError);
      return;
    }
    if (
      request.method === 'init' &&
      (request.params.storage.kind !== 'opfs' ||
        request.params.storage.name !== storage.name)
    ) {
      fail(
        request.id,
        coordinationError(
          'STORAGE_ALREADY_INITIALIZED',
          'The TinyJoin worker is already initialized with different storage',
        ),
      );
      return;
    }
    if (
      request.method === 'prepareSql' &&
      (prepared.size >= 128 ||
        preparedBytes + request.params.sql.length * 2 > MAX_QUEUED_BYTES)
    ) {
      fail(
        request.id,
        coordinationError(
          'RESOURCE_LIMIT',
          'The TinyJoin client prepared statement registry is full',
        ),
      );
      return;
    }
    const cleanup =
      request.method === 'close' ||
      request.method === 'commitTransaction' ||
      request.method === 'rollbackTransaction';
    const bytes = requestBytes(request, MAX_QUEUED_BYTES);
    if (
      bytes > MAX_QUEUED_BYTES ||
      pending.size >= MAX_PENDING_REQUESTS + (cleanup ? 8 : 0) ||
      pendingBytes + bytes > MAX_QUEUED_BYTES + (cleanup ? 8192 : 0)
    ) {
      fail(
        request.id,
        coordinationError(
          'RESOURCE_LIMIT',
          'The TinyJoin client request queue is full',
        ),
      );
      return;
    }
    const entry = {request, bytes};
    pending.set(request.id, entry);
    pendingBytes += bytes;
    dispatch(entry);
  };
  scope.addEventListener('message', receive);
  group.onmessage = (event: MessageEvent<unknown>) => onGroup(event.data);
  inbox.onmessage = (event: MessageEvent<unknown>) => {
    if (
      isRecord(event.data) &&
      event.data.epoch === leader?.epoch &&
      isWorkerResponse(event.data.response)
    )
      response(event.data.response);
  };
  receive({data: init} as MessageEvent<unknown>);

  // Holding this lock lets an owner distinguish a dead client from a slow or
  // backgrounded client, without timeouts that could replay a live write.
  void navigator.locks
    .request(clientChannel(client), async () => {
      if (closed) return;
      const lifetime = new Promise<void>((resolve) => {
        releaseClient = resolve;
      });
      for (const entry of pending.values()) dispatch(entry);
      group.postMessage({kind: 'discover'});
      void navigator.locks
        .request(
          databaseChannel(storage.name),
          {signal: election.signal},
          async () => {
            if (closed || terminalError) return;
            const tenure = new Promise<void>((resolve) => {
              releaseLeader = resolve;
            });
            const epoch = crypto.randomUUID();
            const ownerId = (await navigator.locks.query()).held?.find(
              (lock) => lock.name === databaseChannel(storage.name),
            )?.clientId;
            if (closed) return;
            if (!ownerId)
              throw new Error('The TinyJoin database election lock was lost');
            const starting: Announcement = {
              kind: 'leader',
              epoch,
              compatibility: COORDINATION_COMPATIBILITY,
              ready: false,
              revision: localRevision,
              ownerId,
            };
            publish(starting);
            try {
              // A terminated Worker may release its browser lock just before its
              // native sync handle. Retry only initialization's definite lock failure.
              const deadline = Date.now() + 3_000;
              while (true) {
                engine = createLocalRpc();
                try {
                  localRevision = (await engine.request('init', {storage}))
                    .revision;
                  break;
                } catch (error) {
                  engine.dispose();
                  if (
                    asCodedError(error)?.code !== 'STORAGE_LOCKED' ||
                    Date.now() >= deadline
                  )
                    throw error;
                  await new Promise((resolve) => setTimeout(resolve, 20));
                }
              }
              if (closed) {
                await tenure;
                return;
              }
              const noteResult = (result: unknown): void => {
                for (const value of Array.isArray(result) ? result : [result]) {
                  if (isRecord(value) && typeof value.revision === 'number')
                    localRevision = Math.max(localRevision, value.revision);
                }
                if (leader) leader.revision = localRevision;
              };
              const retire = (error: SerializedError): void => {
                publish({kind: 'failed', epoch, error});
                owner?.close();
                ownerInbox?.close();
                const failedEngine = engine;
                engine = undefined;
                void failedEngine
                  ?.request('close', undefined)
                  .catch(() => undefined)
                  .finally(() => {
                    failedEngine.dispose();
                    releaseLeader?.();
                  });
              };
              owner = createDatabaseBroker(
                engine,
                epoch,
                client,
                response,
                () => localRevision,
                noteResult,
                retire,
              );
              engine.onEvent((event) => publish({kind: 'event', epoch, event}));
              ownerInbox = new BroadcastChannel(ownerChannel(epoch));
              ownerInbox.onmessage = (event: MessageEvent<unknown>) => {
                const value = event.data;
                if (
                  isRecord(value) &&
                  value.kind === 'request' &&
                  typeof value.client === 'string' &&
                  value.client.length <= 64 &&
                  value.epoch === epoch &&
                  typeof value.compatibility === 'string' &&
                  isWorkerRequest(value.request) &&
                  (value.statementSql === undefined ||
                    typeof value.statementSql === 'string') &&
                  (value.statements === undefined ||
                    (Array.isArray(value.statements) &&
                      value.statements.length <= 256 &&
                      value.statements.every(
                        (entry) =>
                          Array.isArray(entry) &&
                          entry.length === 2 &&
                          Number.isSafeInteger(entry[0]) &&
                          typeof entry[1] === 'string',
                      )))
                )
                  owner?.receive(value as RoutedRequest);
              };
              publish({...starting, ready: true, revision: localRevision});
              await tenure;
            } catch (error) {
              publish({
                kind: 'failed',
                epoch,
                error:
                  asCodedError(error) ??
                  coordinationError(
                    'WORKER_OPERATION_FAILED',
                    error instanceof Error ? error.message : String(error),
                  ),
              });
              engine?.dispose();
            }
          },
        )
        .catch((error: unknown) => {
          if (!closed) {
            terminalError = coordinationError(
              'WORKER_OPERATION_FAILED',
              String(error),
            );
            failPending(terminalError);
          }
        });
      await lifetime;
    })
    .catch((error: unknown) => {
      if (!closed)
        failPending(
          coordinationError('WORKER_OPERATION_FAILED', String(error)),
        );
    });
};
