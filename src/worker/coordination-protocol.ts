import {
  PROTOCOL_VERSION,
  type SerializedError,
  type WorkerEvent,
  type WorkerRequest,
  type WorkerResponse,
} from '../protocol.js';

// One physical database always uses the same election lock, including across
// incompatible releases. Change compatibility whenever engine semantics change.
declare const __TINYJOIN_VERSION__: string;
export const COORDINATION_COMPATIBILITY = `1:${PROTOCOL_VERSION}:2:${typeof __TINYJOIN_VERSION__ === 'string' ? __TINYJOIN_VERSION__ : 'source'}`;
export const databaseChannel = (name: string): string =>
  `tinyjoin:database:${name}`;
export const clientChannel = (client: string): string =>
  `tinyjoin:client:${client}`;
export const ownerChannel = (epoch: string): string =>
  `tinyjoin:owner:${epoch}`;
export const MAX_PENDING_REQUESTS = 256;
export const MAX_QUEUED_BYTES = 8 * 1024 * 1024;

export type RoutedRequest = {
  kind: 'request';
  client: string;
  epoch: string;
  compatibility: string;
  request: WorkerRequest;
  statementSql?: string;
  statements?: [number, string][];
};

export type Announcement = {
  kind: 'leader';
  epoch: string;
  compatibility: string;
  ready: boolean;
  revision: number;
  ownerId: string;
};

export type GroupMessage =
  | Announcement
  | {kind: 'discover'}
  | {kind: 'event'; epoch: string; event: WorkerEvent}
  | {kind: 'failed'; epoch: string; error: SerializedError};

export type RoutedResponse = {epoch: string; response: WorkerResponse};

export const coordinationError = (
  code: string,
  message: string,
): SerializedError => ({code, message});
export const leaderChanged = (): SerializedError =>
  coordinationError(
    'LEADER_CHANGED',
    'The database owner changed. The pending operation may have committed; reconcile its outcome before retrying.',
  );
