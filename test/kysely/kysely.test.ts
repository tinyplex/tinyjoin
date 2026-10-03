import {existsSync} from 'node:fs';
import {resolve} from 'node:path';
import {pathToFileURL} from 'node:url';

import {type ColumnType, type Generated, Kysely, sql} from 'kysely';
import {type Migration, Migrator} from 'kysely/migration';
import {afterEach, beforeEach, describe, expect, it} from 'vitest';

import type {Client} from '../../src/index.js';
import {TinyJoinDialect} from '../../src/kysely/index.js';

// The dialect runs against the real engine, through the Node entry point the
// build produces, so it waits for a build like the other artifact tests.
const nodeEntry = resolve(import.meta.dirname, '../../dist/node/index.js');
const runIfBuilt = existsSync(nodeEntry) ? describe : describe.skip;

interface Database {
  users: {
    id: string;
    name: string;
    email: string | null;
    visits: ColumnType<number, number | undefined, number>;
    meta: ColumnType<object | null, object | null | undefined, object | null>;
  };
  posts: {
    id: Generated<number>;
    user_id: string;
    title: string;
  };
}

const createClient = async (): Promise<Client> => {
  const {create} = (await import(pathToFileURL(nodeEntry).href)) as {
    create: () => Promise<Client>;
  };
  return create();
};

runIfBuilt('the Kysely dialect', () => {
  let client: Client;
  let db: Kysely<Database>;

  beforeEach(async () => {
    client = await createClient();
    await client.exec(`
      CREATE TABLE users (id TEXT PRIMARY KEY, name VARCHAR(40) NOT NULL,
        email TEXT UNIQUE, visits INTEGER NOT NULL DEFAULT 0, meta JSON);
      CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id TEXT NOT NULL,
        title TEXT NOT NULL);
    `);
    db = new Kysely<Database>({dialect: new TinyJoinDialect({client})});
    await db
      .insertInto('users')
      .values([
        {id: 'a', name: 'Ann', email: 'ann@example.com', meta: {tags: ['x']}},
        {id: 'b', name: 'Bob', email: null, visits: 3},
      ])
      .execute();
    await db
      .insertInto('posts')
      .values([
        {id: 1, user_id: 'a', title: 'First'},
        {id: 2, user_id: 'b', title: 'Second'},
        {id: 3, user_id: 'a', title: 'Third'},
      ])
      .execute();
  });

  afterEach(async () => {
    await db.destroy();
    await client.close();
  });

  it('selects, filters, joins, and aggregates', async () => {
    expect(
      await db
        .selectFrom('users')
        .selectAll()
        .where('visits', '>', 0)
        .execute(),
    ).toEqual([{id: 'b', name: 'Bob', email: null, visits: 3, meta: null}]);
    expect(
      await db
        .selectFrom('posts')
        .innerJoin('users', 'users.id', 'posts.user_id')
        .select(['posts.id', 'users.name', 'posts.title'])
        .where('users.name', 'like', 'A%')
        .orderBy('posts.id', 'desc')
        .execute(),
    ).toEqual([
      {id: 3, name: 'Ann', title: 'Third'},
      {id: 1, name: 'Ann', title: 'First'},
    ]);
    expect(
      await db
        .selectFrom('posts')
        .select(['user_id', (eb) => eb.fn.countAll<number>().as('posts')])
        .groupBy('user_id')
        .orderBy('user_id')
        .execute(),
    ).toEqual([
      {user_id: 'a', posts: 2},
      {user_id: 'b', posts: 1},
    ]);
    expect(
      await db
        .selectFrom('users')
        .select('id')
        .where('id', 'in', db.selectFrom('posts').select('user_id'))
        .where((eb) => eb.not(eb('visits', '<', 0)))
        .orderBy('id')
        .execute(),
    ).toEqual([{id: 'a'}, {id: 'b'}]);
    expect(
      await db.selectFrom('users').select('meta').where('id', '=', 'a').execute(),
    ).toEqual([{meta: {tags: ['x']}}]);
    expect(
      await db
        .selectFrom('posts')
        .innerJoin('users', 'users.id', 'posts.user_id')
        .selectAll('posts')
        .select('users.name')
        .where('posts.id', '=', 2)
        .execute(),
    ).toEqual([{id: 2, user_id: 'b', title: 'Second', name: 'Bob'}]);
  });

  it('writes, returns, counts, and upserts', async () => {
    const updated = await db
      .updateTable('users')
      .set((eb) => ({visits: eb('visits', '+', 1)}))
      .where('id', '=', 'b')
      .executeTakeFirstOrThrow();
    expect(updated.numUpdatedRows).toBe(1n);
    expect(
      await db
        .insertInto('users')
        .values({id: 'b', name: 'Bobby', email: null, visits: 10})
        .onConflict((oc) =>
          oc.column('id').doUpdateSet((eb) => ({
            name: eb.ref('excluded.name'),
            visits: sql<number>`users.visits + excluded.visits`,
          })),
        )
        .returning(['name', 'visits'])
        .execute(),
    ).toEqual([{name: 'Bobby', visits: 14}]);
    const deleted = await db
      .deleteFrom('posts')
      .where('user_id', '=', 'a')
      .returning('id')
      .execute();
    expect(deleted).toEqual([{id: 1}, {id: 3}]);
    expect(
      (await db.deleteFrom('posts').executeTakeFirstOrThrow()).numDeletedRows,
    ).toBe(1n);
    await expect(
      db.insertInto('users').values({id: 'c', name: 'C'.repeat(41)}).execute(),
    ).rejects.toMatchObject({code: 'CONSTRAINT_VIOLATION'});
  });

  it('commits and rolls back transactions, and queues other queries', async () => {
    await db.transaction().execute(async (trx) => {
      await trx.updateTable('users').set({name: 'Ann 2'}).where('id', '=', 'a').execute();
    });
    await expect(
      db.transaction().execute(async (trx) => {
        await trx.deleteFrom('posts').execute();
        throw new Error('boom');
      }),
    ).rejects.toThrow('boom');
    expect(
      await db.selectFrom('posts').select((eb) => eb.fn.countAll<number>().as('n')).execute(),
    ).toEqual([{n: 3}]);

    // A query outside a transaction waits for it rather than failing.
    let committed = false;
    const [, names] = await Promise.all([
      db.transaction().execute(async (trx) => {
        await new Promise((resolve) => setTimeout(resolve, 20));
        await trx.updateTable('users').set({name: 'Ann 3'}).where('id', '=', 'a').execute();
        committed = true;
      }),
      db.selectFrom('users').select('name').where('id', '=', 'a').execute(),
    ]);
    expect(committed).toBe(true);
    expect(names).toEqual([{name: 'Ann 3'}]);

    await expect(
      db
        .transaction()
        .setIsolationLevel('serializable')
        .execute(async () => undefined),
    ).rejects.toThrow('isolation level');
  });

  it('introspects the tables, leaving out its own', async () => {
    const tables = await db.introspection.getTables();
    expect(tables.map(({name}) => name)).toEqual(['posts', 'users']);
    expect(tables[1]).toEqual({
      name: 'users',
      isView: false,
      isForeign: false,
      columns: [
        {name: 'id', dataType: 'text', isNullable: false, isAutoIncrementing: false, hasDefaultValue: false},
        {name: 'name', dataType: 'varchar', isNullable: false, isAutoIncrementing: false, hasDefaultValue: false},
        {name: 'email', dataType: 'text', isNullable: true, isAutoIncrementing: false, hasDefaultValue: false},
        {name: 'visits', dataType: 'int8', isNullable: false, isAutoIncrementing: false, hasDefaultValue: true},
        {name: 'meta', dataType: 'json', isNullable: true, isAutoIncrementing: false, hasDefaultValue: false},
      ],
    });
    expect(await db.introspection.getSchemas()).toEqual([]);
  });
});

runIfBuilt('the Kysely Migrator', () => {
  let client: Client;
  let db: Kysely<Record<string, never>>;

  beforeEach(async () => {
    client = await createClient();
    db = new Kysely({dialect: new TinyJoinDialect({client})});
  });

  afterEach(async () => {
    await db.destroy();
    await client.close();
  });

  const migrations: Record<string, Migration> = {
    '0001_tasks': {
      async up(db) {
        await db.schema
          .createTable('tasks')
          .addColumn('id', 'text', (col) => col.primaryKey())
          .addColumn('title', 'varchar(255)', (col) => col.notNull())
          .addColumn('done', 'boolean', (col) => col.notNull().defaultTo(false))
          .execute();
        await db.schema.createIndex('tasks_done').on('tasks').column('done').execute();
      },
      async down(db) {
        await db.schema.dropTable('tasks').execute();
      },
    },
    '0002_priority': {
      async up(db) {
        await db.schema
          .alterTable('tasks')
          .addColumn('priority', 'integer', (col) => col.notNull().defaultTo(0))
          .execute();
        await db.schema
          .alterTable('tasks')
          .renameColumn('title', 'name')
          .execute();
      },
      async down(db) {
        await db.schema.alterTable('tasks').renameColumn('name', 'title').execute();
        await db.schema.alterTable('tasks').dropColumn('priority').execute();
      },
    },
  };

  it('migrates up and down', async () => {
    const migrator = new Migrator({
      db,
      provider: {getMigrations: async () => migrations},
    });
    const up = await migrator.migrateToLatest();
    expect(up.error).toBeUndefined();
    expect(up.results?.map(({migrationName, status}) => [migrationName, status])).toEqual([
      ['0001_tasks', 'Success'],
      ['0002_priority', 'Success'],
    ]);
    expect((await migrator.migrateToLatest()).results).toEqual([]);
    const {tables} = await client.getSchema();
    expect(tables.find(({name}) => name === 'tasks')?.columns.map(({name}) => name)).toEqual([
      'id',
      'name',
      'done',
      'priority',
    ]);
    expect(
      (await db.introspection.getTables()).map(({name}) => name),
    ).toEqual(['tasks']);

    const down = await migrator.migrateDown();
    expect(down.error).toBeUndefined();
    expect(
      (await client.getSchema()).tables
        .find(({name}) => name === 'tasks')
        ?.columns.map(({name}) => name),
    ).toEqual(['id', 'title', 'done']);
  });
});
