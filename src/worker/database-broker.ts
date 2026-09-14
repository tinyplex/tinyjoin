import {asCodedError} from '../common.js';
import type {WorkerRpc} from '../client/rpc.js';
import {
  PROTOCOL_VERSION,
  type SerializedError,
  type WorkerRequest,
  type WorkerResponse,
} from '../protocol.js';
import {
  clientChannel,
  COORDINATION_COMPATIBILITY,
  coordinationError,
  MAX_PENDING_REQUESTS,
  MAX_QUEUED_BYTES,
  type RoutedRequest,
} from './coordination-protocol.js';
import {requestBytes} from './request-size.js';

type Connection = {
  channel: BroadcastChannel;
  lifetime: AbortController;
  prepared: Map<number, number>;
};
type Queued = {message: RoutedRequest; bytes: number};

/** Schedules complete transaction callbacks, not just individual RPC messages. */
export const createDatabaseBroker = (
  rpc: WorkerRpc,
  epoch: string,
  localClient: string,
  localResponse: (response: WorkerResponse) => void,
  revision: () => number,
  noteResult: (result: unknown) => void,
  onFailure: (error: SerializedError) => void,
) => {
  const connections = new Map<string, Connection>();
  const queue: Queued[] = [];
  const abandoned = new Set<string>();
  let queuedBytes = 0;
  let busy = false;
  let closed = false;
  let transaction: {client: string; token: string} | undefined;

  const reply = (client: string, response: WorkerResponse): void => {
    if (client === localClient) localResponse(response);
    else connections.get(client)?.channel.postMessage({epoch, response});
  };
  const fail = (message: RoutedRequest, error: SerializedError): void =>
    reply(message.client, {
      v: PROTOCOL_VERSION,
      id: message.request.id,
      ok: false,
      error,
    });

  const forget = async (client: string): Promise<void> => {
    const connection = connections.get(client);
    if (!connection) return;
    if (transaction?.client === client) {
      try {
        await rpc.request('rollbackTransaction', {
          transactionId: transaction.token,
        });
      } finally {
        transaction = undefined;
      }
    }
    // Host prepared cleanup is only legal outside a transaction. An unrelated
    // client's detach must never interrupt the current callback.
    if (transaction) return;
    for (const statementId of connection.prepared.values()) {
      await rpc.request('closePrepared', {statementId}).catch(() => undefined);
    }
    connection.lifetime.abort();
    connection.channel.close();
    connections.delete(client);
    abandoned.delete(client);
  };

  const handle = async (message: RoutedRequest): Promise<unknown> => {
    const {client, request} = message;
    const connection = connections.get(client)!;
    if (request.method === 'init') return {revision: revision()};
    if (request.method === 'close') {
      abandoned.add(client);
      if (transaction?.client === client) {
        await rpc.request('rollbackTransaction', {
          transactionId: transaction.token,
        });
        transaction = undefined;
      }
      return undefined;
    }
    if (request.method === 'prepareSql') {
      const {statementId} = await rpc.request('prepareSql', request.params);
      // Request ids remain unique for the lifetime of this client, even after
      // election. Engine ids do not, so never expose them across this boundary.
      connection.prepared.set(request.id, statementId);
      return {statementId: request.id};
    }
    if (request.method === 'closePrepared') {
      const statementId = connection.prepared.get(request.params.statementId);
      connection.prepared.delete(request.params.statementId);
      if (statementId !== undefined)
        await rpc.request('closePrepared', {statementId});
      return undefined;
    }
    if (request.method === 'beginTransaction') {
      for (const [logicalId, sql] of message.statements ?? []) {
        if (!connection.prepared.has(logicalId)) {
          connection.prepared.set(
            logicalId,
            (await rpc.request('prepareSql', {sql})).statementId,
          );
        }
      }
      const {transactionId} = await rpc.request('beginTransaction', undefined);
      transaction = {client, token: transactionId};
      return {transactionId: `${epoch}/${transactionId}`};
    }
    let forwarded: WorkerRequest = request;
    if (
      request.params &&
      'transactionId' in request.params &&
      request.params.transactionId !== undefined
    ) {
      if (
        transaction?.client !== client ||
        request.params.transactionId !== `${epoch}/${transaction.token}`
      ) {
        throw coordinationError(
          'TRANSACTION_LOST',
          'The TinyJoin transaction ended when its database owner disconnected',
        );
      }
      forwarded = {
        ...request,
        params: {...request.params, transactionId: transaction.token},
      } as WorkerRequest;
    }
    if (forwarded.method === 'executePrepared') {
      const logicalId = forwarded.params.statementId;
      let statementId = connection.prepared.get(logicalId);
      if (statementId === undefined) {
        if (message.statementSql === undefined)
          throw coordinationError(
            'PREPARED_STATEMENT_CLOSED',
            'The prepared statement is no longer available',
          );
        // Reprepare after handover. The host forbids prepare during a callback,
        // so transaction preparation is restored before beginTransaction below.
        statementId = (
          await rpc.request('prepareSql', {sql: message.statementSql})
        ).statementId;
        connection.prepared.set(logicalId, statementId);
      }
      forwarded = {...forwarded, params: {...forwarded.params, statementId}};
    }
    try {
      const result = await rpc.request(
        forwarded.method,
        forwarded.params as never,
      );
      if (
        request.method === 'commitTransaction' ||
        request.method === 'rollbackTransaction'
      )
        transaction = undefined;
      return result;
    } catch (error) {
      if (request.method === 'commitTransaction') {
        // The host may already have discarded the transaction on commit error.
        await rpc
          .request('rollbackTransaction', {transactionId: transaction!.token})
          .catch(() => undefined);
        transaction = undefined;
      }
      throw error;
    }
  };

  const pump = async (): Promise<void> => {
    if (busy || closed) return;
    busy = true;
    try {
      while (!closed) {
        for (const client of abandoned) {
          if (!transaction || transaction.client === client)
            await forget(client);
        }
        const index = queue.findIndex(
          ({message}) =>
            !transaction ||
            message.client === transaction.client ||
            message.request.method === 'close' ||
            message.request.method === 'init',
        );
        if (index < 0) break;
        const {message, bytes} = queue.splice(index, 1)[0]!;
        queuedBytes -= bytes;
        if (abandoned.has(message.client) || !connections.has(message.client)) {
          fail(
            message,
            coordinationError('CLIENT_CLOSED', 'The TinyJoin client is closed'),
          );
          continue;
        }
        try {
          const result = await handle(message);
          noteResult(result);
          reply(message.client, {
            v: PROTOCOL_VERSION,
            id: message.request.id,
            ok: true,
            result,
          });
        } catch (error) {
          const serialized =
            asCodedError(error) ??
            coordinationError(
              'WORKER_OPERATION_FAILED',
              error instanceof Error ? error.message : String(error),
            );
          fail(message, serialized);
          if (
            [
              'RECOVERY_REQUIRED',
              'STORAGE_COMMIT_OUTCOME_UNKNOWN',
              'STORAGE_ENGINE_POISONED',
            ].includes(serialized.code)
          )
            onFailure(serialized);
        }
      }
    } catch {
      onFailure(
        coordinationError(
          'STORAGE_ENGINE_POISONED',
          'The database owner could not discard an abandoned transaction. Close and reopen the database before continuing.',
        ),
      );
    } finally {
      busy = false;
    }
  };

  return {
    receive: (message: RoutedRequest): void => {
      if (closed) return;
      let connection = connections.get(message.client);
      if (!connection) {
        if (connections.size >= 128) {
          const response: WorkerResponse = {
            v: PROTOCOL_VERSION,
            id: message.request.id,
            ok: false,
            error: coordinationError(
              'RESOURCE_LIMIT',
              'The TinyJoin database has too many connected clients',
            ),
          };
          if (message.client === localClient) localResponse(response);
          else {
            const channel = new BroadcastChannel(clientChannel(message.client));
            channel.postMessage({epoch, response});
            channel.close();
          }
          return;
        }
        connection = {
          channel: new BroadcastChannel(clientChannel(message.client)),
          lifetime: new AbortController(),
          prepared: new Map(),
        };
        connections.set(message.client, connection);
        void navigator.locks
          .request(
            clientChannel(message.client),
            {signal: connection.lifetime.signal},
            () => {
              abandoned.add(message.client);
              void pump();
            },
          )
          .catch(() => undefined);
      }
      if (message.compatibility !== COORDINATION_COMPATIBILITY) {
        fail(
          message,
          coordinationError(
            'DATABASE_VERSION_MISMATCH',
            'Another tab is using an incompatible TinyJoin release. Close the old tabs and reload.',
          ),
        );
        abandoned.add(message.client);
        void pump();
        return;
      }
      if (message.epoch !== epoch) return;
      // Reserve space for the active callback's commit/rollback, even when
      // other tabs have filled the queue while waiting for it.
      const priority =
        message.request.method === 'close' ||
        (transaction?.client === message.client &&
          (message.request.method === 'commitTransaction' ||
            message.request.method === 'rollbackTransaction'));
      const bytes = requestBytes(message, MAX_QUEUED_BYTES);
      if (
        bytes > MAX_QUEUED_BYTES ||
        queue.length >= MAX_PENDING_REQUESTS + (priority ? 8 : 0) ||
        queuedBytes + bytes > MAX_QUEUED_BYTES + (priority ? 8192 : 0)
      ) {
        fail(
          message,
          coordinationError(
            'RESOURCE_LIMIT',
            'The TinyJoin multi-tab request queue is full',
          ),
        );
        return;
      }
      queue.push({message, bytes});
      queuedBytes += bytes;
      void pump();
    },
    close: (): void => {
      closed = true;
      for (const connection of connections.values()) {
        connection.lifetime.abort();
        connection.channel.close();
      }
      connections.clear();
      queue.length = 0;
    },
  };
};
