import {
  create,
  type Client,
  type JsonValue,
  type PreparedStatement,
  type Row,
} from 'tinyjoin';

export interface OperationState {
  status: 'pending' | 'resolved' | 'rejected';
  entered: boolean;
  code?: string;
  rows?: Row[];
}

export interface RequeryState {
  calls: number;
  resets: number;
  errors: string[];
  rows: Row[][];
}

export interface MultiTabFixture {
  open(client: string, name: string): Promise<void>;
  setup(client: string): Promise<void>;
  query(client: string, sql: string, params?: JsonValue[]): Promise<Row[]>;
  close(client: string): Promise<void>;
  subscribe(client: string): void;
  events(client: string): Array<{revision: number; tables: string[]}>;
  revision(client: string): number;
  startQuery(
    client: string,
    operation: string,
    sql: string,
    params?: JsonValue[],
  ): void;
  startTransaction(
    client: string,
    operation: string,
    id: number,
    value: string,
  ): void;
  release(operation: string): void;
  operation(operation: string): OperationState;
  prepare(client: string, statement: string, sql: string): Promise<void>;
  execute(statement: string, params?: JsonValue[]): Promise<Row[]>;
  executeInTransaction(
    client: string,
    statement: string,
    params?: JsonValue[],
  ): Promise<Row[]>;
  closePrepared(statement: string): Promise<void>;
  executeForeign(client: string, statement: string): Promise<string>;
  advertiseIncompatible(name: string): Promise<void>;
  observeLeader(name: string): void;
  leaderObserved(name: string): boolean;
  replayLeader(name: string): void;
  captureResyncs(): void;
  receivedResyncs(): number;
  subscribeAndRequery(client: string): void;
  requeries(client: string): RequeryState;
}

declare global {
  interface Window {
    __tinyjoinMultiTab: MultiTabFixture;
  }
}

const clients = new Map<string, Client>();
const statements = new Map<string, PreparedStatement>();
const operations = new Map<string, OperationState>();
const releases = new Map<string, () => void>();
const invalidations = new Map<
  string,
  Array<{revision: number; tables: string[]}>
>();
const observations = new Map<
  string,
  {channel: BroadcastChannel; first?: unknown}
>();
const requeries = new Map<string, RequeryState>();
let receivedResyncs = 0;

const client = (name: string): Client => {
  const value = clients.get(name);
  if (!value) throw new Error(`Unknown fixture client: ${name}`);
  return value;
};

const statement = (name: string): PreparedStatement => {
  const value = statements.get(name);
  if (!value) throw new Error(`Unknown fixture statement: ${name}`);
  return value;
};

const errorCode = (error: unknown): string =>
  typeof error === 'object' && error !== null && 'code' in error
    ? String(error.code)
    : String(error);

const track = (
  name: string,
  run: (state: OperationState) => Promise<Row[]>,
): void => {
  if (operations.has(name))
    throw new Error(`Repeated fixture operation: ${name}`);
  const state: OperationState = {status: 'pending', entered: false};
  operations.set(name, state);
  void run(state).then(
    (rows) => {
      state.rows = rows;
      state.status = 'resolved';
    },
    (error) => {
      state.code = errorCode(error);
      state.status = 'rejected';
    },
  );
};

window.__tinyjoinMultiTab = {
  async open(alias, name) {
    clients.set(alias, await create(`opfs://${name}`));
  },
  async setup(alias) {
    await client(alias).exec(
      'CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY, value TEXT NOT NULL)',
    );
  },
  async query(alias, sql, params = []) {
    return (await client(alias).query(sql, params)).rows;
  },
  async close(alias) {
    await client(alias).close();
  },
  subscribe(alias) {
    const events: Array<{revision: number; tables: string[]}> = [];
    invalidations.set(alias, events);
    client(alias).subscribe({tables: ['items']}, (event) => events.push(event));
  },
  events(alias) {
    return invalidations.get(alias) ?? [];
  },
  revision(alias) {
    return client(alias).getRevision();
  },
  startQuery(alias, operation, sql, params = []) {
    track(operation, async () => (await client(alias).query(sql, params)).rows);
  },
  startTransaction(alias, operation, id, value) {
    const gate = new Promise<void>((resolve) =>
      releases.set(operation, resolve),
    );
    track(operation, async (state) =>
      client(alias).transaction(async (transaction) => {
        await transaction.query('INSERT INTO items VALUES ($1, $2)', [
          id,
          value,
        ]);
        const rows = (
          await transaction.query('SELECT * FROM items ORDER BY id')
        ).rows;
        state.entered = true;
        await gate;
        return rows;
      }),
    );
  },
  release(operation) {
    const release = releases.get(operation);
    if (!release) throw new Error(`Unknown fixture gate: ${operation}`);
    release();
    releases.delete(operation);
  },
  operation(name) {
    const state = operations.get(name);
    if (!state) throw new Error(`Unknown fixture operation: ${name}`);
    return state;
  },
  async prepare(alias, name, sql) {
    statements.set(name, await client(alias).prepare(sql));
  },
  async execute(name, params = []) {
    return (await statement(name).execute(params)).rows;
  },
  async executeInTransaction(alias, name, params = []) {
    return client(alias).transaction(
      async (transaction) =>
        (await transaction.execute(statement(name), params)).rows,
    );
  },
  async closePrepared(name) {
    await statement(name).close();
  },
  async executeForeign(alias, name) {
    try {
      await client(alias).transaction((transaction) =>
        transaction.execute(statement(name)),
      );
      return '';
    } catch (error) {
      return errorCode(error);
    }
  },
  async advertiseIncompatible(name) {
    const channel = new BroadcastChannel(`tinyjoin:database:${name}`);
    let ownerId: string | undefined;
    const announce = (): void =>
      channel.postMessage({
        kind: 'leader',
        epoch: 'fixture-incompatible-owner',
        compatibility: '1:999:2:0.1.0',
        ready: true,
        revision: 0,
        ownerId,
      });
    await new Promise<void>((resolve, reject) => {
      void navigator.locks
        .request(`tinyjoin:database:${name}`, async () => {
          ownerId = (await navigator.locks.query()).held?.find(
            (lock) => lock.name === `tinyjoin:database:${name}`,
          )?.clientId;
          if (!ownerId)
            throw new Error(
              'The incompatible test owner did not acquire its election lock',
            );
          channel.onmessage = announce;
          announce();
          const timer = setInterval(announce, 50);
          resolve();
          await new Promise<void>(() => undefined);
          clearInterval(timer);
        })
        .catch(reject);
    });
  },
  observeLeader(name) {
    const observation: {channel: BroadcastChannel; first?: unknown} = {
      channel: new BroadcastChannel(`tinyjoin:database:${name}`),
    };
    observations.set(name, observation);
    observation.channel.onmessage = (event) => {
      if (
        observation.first === undefined &&
        event.data?.kind === 'leader' &&
        event.data.ready === true
      ) {
        observation.first = event.data;
      }
    };
  },
  leaderObserved(name) {
    return observations.get(name)?.first !== undefined;
  },
  replayLeader(name) {
    const observation = observations.get(name);
    if (!observation || observation.first === undefined)
      throw new Error('No owner announcement was recorded');
    observation.channel.postMessage(observation.first);
  },
  captureResyncs() {
    const NativeWorker = window.Worker;
    window.Worker = class extends NativeWorker {
      constructor(url: string | URL, options?: WorkerOptions) {
        super(url, options);
        this.addEventListener('message', event => {
          if (event.data?.event === 'resync') receivedResyncs++;
        });
      }
    };
  },
  receivedResyncs() {
    return receivedResyncs;
  },
  subscribeAndRequery(alias) {
    const state: RequeryState = {calls: 0, resets: 0, errors: [], rows: []};
    requeries.set(alias, state);
    client(alias).subscribe({tables: ['items']}, async event => {
      state.calls++;
      if (event.reset) state.resets++;
      try {
        state.rows.push((await client(alias).query('SELECT * FROM items ORDER BY id')).rows);
      } catch (error) {
        state.errors.push(errorCode(error));
      }
    });
  },
  requeries(alias) {
    const state = requeries.get(alias);
    if (!state) throw new Error(`No requery subscription for client: ${alias}`);
    return state;
  },
};

// Intentionally no pagehide cleanup: page.close() tests browser-enforced release
// of the dedicated Worker and lifetime locks, rather than graceful db.close().
document.querySelector('[data-testid="state"]')!.textContent = 'Ready';
