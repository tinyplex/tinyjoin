import {
  type Client,
  ClientError,
  type JsonPrimitive,
  type JsonValue,
  type PreparedStatement,
  type QueryOptions,
  type ResultField,
  type Results,
  type Row,
  type RowMode,
  type SerializedError,
  type SubscriptionOptions,
  type TablesChangedEvent,
  type Transaction,
  create,
} from 'tinyjoin/node';

type Task = {id: string; title: string};

export async function exerciseNodeDeclarations(): Promise<void> {
  const database: Client = await create();
  const explicit: Client = await create('memory://');
  const omitted: Client = await create(undefined);
  try {
    const primitive: JsonPrimitive = 'first';
    const params: JsonValue[] = [primitive];
    const rowMode: RowMode = 'object';
    const options: QueryOptions = {rowMode};
    const statement: PreparedStatement<Task> = await database.prepare<Task>(
      'SELECT id, title FROM tasks WHERE id = $1',
    );
    const result: Results<Task> = await statement.execute(params, options);
    const field: ResultField | undefined = result.fields[0];
    const row: Row | undefined = result.rows[0];
    const title: string | undefined = result.rows[0]?.title;
    const count: number = await database.transaction(
      async (transaction: Transaction) => {
        const selected = await transaction.execute(statement, ['first']);
        return selected.rows.length;
      },
    );
    const subscription: SubscriptionOptions = {tables: ['tasks']};
    const unsubscribe: () => void = database.subscribe(
      subscription,
      (event: TablesChangedEvent) => {
        const revision: number = event.revision;
        const tables: string[] = event.tables;
        void [revision, tables];
      },
    );
    unsubscribe();
    await statement.close();
    const serialized: SerializedError = {code: 'EXAMPLE', message: 'example'};
    const error: ClientError = new ClientError(serialized);
    void [title, count, field, row, error];
  } finally {
    await Promise.all([database.close(), explicit.close(), omitted.close()]);
  }
}

// @ts-expect-error Node currently supports only ephemeral memory storage.
void create('opfs://tasks');
// @ts-expect-error A filesystem backend is not part of the Node entry point.
void create('file:///tmp/tasks');
// @ts-expect-error Browser worker options are not Node create options.
void create({dataDir: 'memory://'});
// @ts-expect-error Node create has no second options argument.
void create('memory://', {});
