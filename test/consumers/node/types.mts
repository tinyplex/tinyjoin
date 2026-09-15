import type {Client, PreparedStatement, Results, Transaction} from 'tinyjoin';
import {create} from 'tinyjoin/node';

type Task = {id: string; title: string};

export async function exerciseNodeDeclarations(): Promise<void> {
  const database: Client = await create();
  const explicit: Client = await create('memory://');
  const omitted: Client = await create(undefined);
  try {
    const statement: PreparedStatement<Task> = await database.prepare<Task>(
      'SELECT id, title FROM tasks WHERE id = $1',
    );
    const result: Results<Task> = await statement.execute(['first']);
    const title: string | undefined = result.rows[0]?.title;
    const count: number = await database.transaction(
      async (transaction: Transaction) => {
        const selected = await transaction.execute(statement, ['first']);
        return selected.rows.length;
      },
    );
    const unsubscribe: () => void = database.subscribe(
      {tables: ['tasks']},
      (event) => {
        const revision: number = event.revision;
        const tables: string[] = event.tables;
        void [revision, tables];
      },
    );
    unsubscribe();
    await statement.close();
    void [title, count];
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
