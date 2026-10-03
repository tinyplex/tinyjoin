import {existsSync, readFileSync, readdirSync} from 'node:fs';
import {resolve} from 'node:path';
import {pathToFileURL} from 'node:url';

import {
  DrizzleQueryError,
  TransactionRollbackError,
  and,
  count,
  desc,
  eq,
  gt,
  inArray,
  like,
  or,
  relations,
  sql,
} from 'drizzle-orm';
import {alias, boolean, integer, jsonb, pgTable, text} from 'drizzle-orm/pg-core';
import {afterEach, beforeEach, describe, expect, it} from 'vitest';

import {
  drizzle,
  migrate,
  type MigrationJournal,
  type TinyJoinDatabase,
} from '../../src/drizzle/index.js';
import type {Client} from '../../src/index.js';

// The driver runs against the real engine, through the Node entry point the
// build produces, so it waits for a build like the other artifact tests.
const nodeEntry = resolve(import.meta.dirname, '../../dist/node/index.js');
const runIfBuilt = existsSync(nodeEntry) ? describe : describe.skip;

const users = pgTable('users', {
  id: integer('id').primaryKey(),
  name: text('name').notNull(),
  email: text('email'),
  active: boolean('active').notNull().default(true),
  visits: integer('visits').notNull().default(0),
  meta: jsonb('meta'),
});
const posts = pgTable('posts', {
  id: integer('id').primaryKey(),
  userId: integer('user_id').notNull(),
  title: text('title').notNull(),
  score: integer('score').notNull().default(0),
});
const usersRelations = relations(users, ({many}) => ({posts: many(posts)}));
const postsRelations = relations(posts, ({one}) => ({
  user: one(users, {fields: [posts.userId], references: [users.id]}),
}));
const schema = {users, posts, usersRelations, postsRelations};

runIfBuilt('the Drizzle driver', () => {
  let client: Client;
  let db: TinyJoinDatabase<typeof schema>;

  beforeEach(async () => {
    const {create} = (await import(pathToFileURL(nodeEntry).href)) as {
      create: () => Promise<Client>;
    };
    client = await create();
    await client.exec(`
      CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT,
        active BOOLEAN NOT NULL DEFAULT true, visits INTEGER NOT NULL DEFAULT 0,
        meta JSONB);
      CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL,
        title TEXT NOT NULL, score INTEGER NOT NULL DEFAULT 0);
    `);
    db = drizzle(client, {schema});
    await db.insert(users).values([
      {id: 1, name: 'Ann', email: 'ann@example.com', meta: {tags: ['a']}},
      {id: 2, name: 'Bob', visits: 3},
      {id: 3, name: 'Cy', active: false},
    ]);
    await db.insert(posts).values([
      {id: 10, userId: 1, title: 'First', score: 7},
      {id: 11, userId: 2, title: 'Second', score: 3},
      {id: 12, userId: 1, title: 'Third', score: 9},
    ]);
  });

  afterEach(async () => {
    await client.close();
  });

  it('builds selects, filters, ordering, and aggregates', async () => {
    expect(db.$client).toBe(client);
    expect(
      await db
        .select({id: users.id, name: users.name})
        .from(users)
        .where(and(eq(users.active, true), or(like(users.name, 'A%'), gt(users.visits, 1))))
        .orderBy(desc(users.id)),
    ).toEqual([
      {id: 2, name: 'Bob'},
      {id: 1, name: 'Ann'},
    ]);
    expect(
      await db
        .select({id: users.id})
        .from(users)
        .where(inArray(users.id, [1, 3]))
        .limit(1)
        .offset(1),
    ).toEqual([{id: 3}]);
    expect(await db.select({n: count()}).from(users)).toEqual([{n: 3}]);
    expect(await db.$count(users, eq(users.active, true))).toBe(2);
    expect(
      await db
        .select({active: users.active, n: count()})
        .from(users)
        .groupBy(users.active)
        .orderBy(users.active),
    ).toEqual([
      {active: false, n: 1},
      {active: true, n: 2},
    ]);
    expect(
      await db.selectDistinct({userId: posts.userId}).from(posts).orderBy(posts.userId),
    ).toEqual([{userId: 1}, {userId: 2}]);
    // An `sql` field is an expression without an alias, which Drizzle reads by
    // position.
    expect(
      await db
        .select({id: posts.id, doubled: sql<number>`${posts.score} * 2`})
        .from(posts)
        .where(gt(sql`${posts.score} - 5`, 0))
        .orderBy(posts.id),
    ).toEqual([
      {id: 10, doubled: 14},
      {id: 12, doubled: 18},
    ]);
    expect(
      await db
        .select({id: users.id})
        .from(users)
        .where(inArray(users.id, db.select({id: posts.userId}).from(posts)))
        .orderBy(users.id),
    ).toEqual([{id: 1}, {id: 2}]);
  });

  it('joins, including columns that share a name', async () => {
    expect(
      await db
        .select()
        .from(posts)
        .innerJoin(users, eq(users.id, posts.userId))
        .where(eq(posts.id, 11)),
    ).toEqual([
      {
        posts: {id: 11, userId: 2, title: 'Second', score: 3},
        users: {
          id: 2,
          name: 'Bob',
          email: null,
          active: true,
          visits: 3,
          meta: null,
        },
      },
    ]);
    const author = alias(users, 'author');
    expect(
      await db
        .select({postId: posts.id, authorId: author.id})
        .from(posts)
        .leftJoin(author, eq(author.id, posts.userId))
        .orderBy(posts.id),
    ).toEqual([
      {postId: 10, authorId: 1},
      {postId: 11, authorId: 2},
      {postId: 12, authorId: 1},
    ]);
    expect(
      await db.query.users.findMany({
        columns: {id: true, name: true},
        where: eq(users.active, true),
        orderBy: [users.id],
      }),
    ).toEqual([
      {id: 1, name: 'Ann'},
      {id: 2, name: 'Bob'},
    ]);
    expect(
      await db.query.users.findFirst({where: eq(users.id, 3), columns: {name: true}}),
    ).toEqual({name: 'Cy'});
  });

  it('writes, returns, increments, and upserts', async () => {
    expect(
      await db
        .update(users)
        .set({visits: sql`${users.visits} + 1`})
        .where(eq(users.id, 2))
        .returning({visits: users.visits}),
    ).toEqual([{visits: 4}]);
    expect(
      await db
        .insert(users)
        .values({id: 2, name: 'Bobby', visits: 10})
        .onConflictDoUpdate({
          target: users.id,
          set: {
            name: sql`excluded.name`,
            visits: sql`${users.visits} + excluded.visits`,
          },
        })
        .returning({name: users.name, visits: users.visits}),
    ).toEqual([{name: 'Bobby', visits: 14}]);
    await db.insert(users).values({id: 1, name: 'Dup'}).onConflictDoNothing();
    expect(
      await db.delete(posts).where(eq(posts.userId, 1)).returning({id: posts.id}),
    ).toEqual([{id: 10}, {id: 12}]);
    expect(await db.$count(posts)).toBe(1);
  });

  it('stores JSON values as JSON, however they are bound', async () => {
    await db
      .update(users)
      .set({meta: {nested: {deep: [1, 'two']}}})
      .where(eq(users.id, 2));
    const insert = db
      .insert(users)
      .values({id: 4, name: 'Di', meta: sql.placeholder('meta')})
      .prepare('insert_with_meta');
    await insert.execute({meta: {from: 'placeholder'}});
    expect((await client.query('SELECT id, meta FROM users ORDER BY id')).rows).toEqual([
      {id: 1, meta: {tags: ['a']}},
      {id: 2, meta: {nested: {deep: [1, 'two']}}},
      {id: 3, meta: null},
      {id: 4, meta: {from: 'placeholder'}},
    ]);
    expect(
      await db.select({meta: users.meta}).from(users).where(eq(users.id, 4)),
    ).toEqual([{meta: {from: 'placeholder'}}]);
  });

  it('commits and rolls back transactions, which do not nest', async () => {
    const renamed = await db.transaction(async (tx) => {
      await tx.update(users).set({name: 'Ann 2'}).where(eq(users.id, 1));
      return tx.select({name: users.name}).from(users).where(eq(users.id, 1));
    });
    expect(renamed).toEqual([{name: 'Ann 2'}]);

    await expect(
      db.transaction(async (tx) => {
        await tx.update(users).set({name: 'Gone'}).where(eq(users.id, 1));
        tx.rollback();
      }),
    ).rejects.toBeInstanceOf(TransactionRollbackError);
    await expect(
      db.transaction(async (tx) => {
        await tx.delete(posts);
        throw new Error('boom');
      }),
    ).rejects.toThrow('boom');
    expect(await db.$count(posts)).toBe(3);
    expect(
      await db.select({name: users.name}).from(users).where(eq(users.id, 1)),
    ).toEqual([{name: 'Ann 2'}]);

    await expect(
      db.transaction((tx) => tx.transaction(async () => undefined)),
    ).rejects.toThrow('do not nest');
    await expect(
      db.transaction(async () => undefined, {isolationLevel: 'serializable'}),
    ).rejects.toThrow('isolation level');
  });

  it('reports the engine error as the cause of a failed query', async () => {
    const failure = await db
      .select({id: users.id})
      .from(users)
      .where(sql`${users.name} > 1`)
      .catch((error: unknown) => error);
    expect(failure).toBeInstanceOf(DrizzleQueryError);
    expect((failure as DrizzleQueryError).cause).toMatchObject({code: 'TYPE_MISMATCH'});
  });
});

// Three migrations that `drizzle-kit generate` wrote: the first creates two
// tables, the second drops one and adds a column and an index to the other, and
// the third changes that table's columns and drops one.
const migrationsFolder = resolve(import.meta.dirname, 'migrations');
const journal = JSON.parse(
  readFileSync(resolve(migrationsFolder, 'meta/_journal.json'), 'utf8'),
) as MigrationJournal;
// Keyed as a bundler's glob import keys them.
const migrations = Object.fromEntries(
  readdirSync(migrationsFolder)
    .filter((file) => file.endsWith('.sql'))
    .map((file) => [
      `./drizzle/${file}`,
      readFileSync(resolve(migrationsFolder, file), 'utf8'),
    ]),
);

runIfBuilt('the Drizzle migrator', () => {
  let client: Client;
  let db: TinyJoinDatabase;

  beforeEach(async () => {
    const {create} = (await import(pathToFileURL(nodeEntry).href)) as {
      create: () => Promise<Client>;
    };
    client = await create();
    db = drizzle(client);
  });

  afterEach(async () => {
    await client.close();
  });

  const applied = async (table = '__drizzle_migrations') =>
    (await client.query(`SELECT tag FROM "${table}" ORDER BY created_at`)).rows;

  it('applies each migration once, in journal order', async () => {
    await migrate(db, {journal: {entries: journal.entries.slice(0, 1)}, migrations});
    expect((await client.getSchema()).tables.map(({name}) => name)).toEqual([
      '__drizzle_migrations',
      'post_tags',
      'users',
    ]);
    await client.query("INSERT INTO users (id, name) VALUES ('u1', 'Ann')");

    await migrate(db, {journal, migrations});
    const {tables} = await client.getSchema();
    expect(tables.map(({name}) => name)).toEqual(['__drizzle_migrations', 'users']);
    expect(tables[1]).toEqual({
      name: 'users',
      columns: [
        {name: 'id', type: 'text', nullable: false},
        {name: 'name', type: 'text', nullable: true},
        {name: 'email', type: 'text', nullable: true},
        {name: 'active', type: 'boolean', nullable: false, default: false},
        {name: 'visits', type: 'integer', nullable: false, default: 0},
      ],
      primaryKey: ['id'],
      indexes: [
        {name: 'users_active_idx', columns: ['active'], unique: false},
        {name: 'users_email_unique', columns: ['email'], unique: true},
        {name: 'users_name_idx', columns: ['name'], unique: false},
      ],
    });
    expect(await applied()).toEqual([
      {tag: '0000_create_users'},
      {tag: '0001_add_visits'},
      {tag: '0002_loosen_users'},
    ]);
    // The row written before the migrations keeps the default it was written with.
    expect((await client.query('SELECT * FROM users')).rows).toEqual([
      {id: 'u1', name: 'Ann', email: null, active: true, visits: 0},
    ]);

    const revision = client.getRevision();
    await migrate(db, {journal, migrations});
    expect(client.getRevision()).toBe(revision);
  });

  it('records migrations in the table it is given', async () => {
    const migrationsTable = 'app "migrations"';
    await migrate(db, {journal, migrations, migrationsTable});
    expect(await applied('app ""migrations""')).toHaveLength(3);
    expect(
      (await client.getSchema()).tables.some(
        ({name}) => name === '__drizzle_migrations',
      ),
    ).toBe(false);
  });

  it('leaves a failed migration unapplied and unrecorded', async () => {
    const failing = {
      entries: [...journal.entries, {tag: '0002_bad', when: 2e12}],
    };
    const failure = await migrate(db, {
      journal: failing,
      migrations: {
        ...migrations,
        '0002_bad.sql': `CREATE TABLE notes (id INTEGER PRIMARY KEY);
          CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id TEXT REFERENCES users (id));`,
      },
    }).catch((error: unknown) => error);
    expect(failure).toMatchObject({code: 'UNSUPPORTED_SQL'});
    expect(await applied()).toHaveLength(3);
    expect(
      (await client.getSchema()).tables.some(({name}) => name === 'notes'),
    ).toBe(false);

    await expect(
      migrate(db, {journal: failing, migrations}),
    ).rejects.toThrow('Migration 0002_bad matches 0 of the migrations given');
  });

  it('skips a migration another Client applied first', async () => {
    let raced = false;
    // The other Client applies the migration just before this one tries to.
    const racing = {
      $client: {
        query: client.query.bind(client),
        exec: async (sql: string) => {
          if (sql.startsWith('INSERT') && !raced) {
            raced = true;
            await client.exec(sql);
          }
          return client.exec(sql);
        },
      },
    } as unknown as TinyJoinDatabase;
    await migrate(racing, {journal, migrations});
    expect(raced).toBe(true);
    expect(await applied()).toHaveLength(3);
  });
});

