import assert from 'node:assert/strict';

import {eq, sql} from 'drizzle-orm';
import {integer, jsonb, pgTable, text} from 'drizzle-orm/pg-core';
import {drizzle, migrate} from 'tinyjoin/drizzle';
import {create} from 'tinyjoin/node';

const notes = pgTable('notes', {
  id: integer('id').primaryKey(),
  body: text('body').notNull(),
  meta: jsonb('meta'),
});

const client = await create();
try {
  const db = drizzle(client, {schema: {notes}});
  const migration = {
    journal: {entries: [{tag: '0000_notes', when: 1}]},
    migrations: {
      './drizzle/0000_notes.sql':
        'CREATE TABLE "notes" ("id" integer PRIMARY KEY NOT NULL, "body" text NOT NULL, "meta" jsonb);',
    },
  };
  await migrate(db, migration);
  await migrate(db, migration);
  assert.equal(db.$client, client);
  await db.insert(notes).values({id: 1, body: 'Hello', meta: {from: 'drizzle'}});
  await db.transaction(async (tx) => {
    await tx
      .update(notes)
      .set({body: sql`${notes.body} || '!'`})
      .where(eq(notes.id, 1));
  });
  assert.deepEqual(await db.select().from(notes), [
    {id: 1, body: 'Hello!', meta: {from: 'drizzle'}},
  ]);
  assert.deepEqual((await client.query('SELECT meta FROM notes')).rows, [
    {meta: {from: 'drizzle'}},
  ]);
  assert.deepEqual(await db.query.notes.findFirst({columns: {body: true}}), {
    body: 'Hello!',
  });
  console.log('NODE_DRIZZLE_CONSUMER_OK');
} finally {
  await client.close();
}
