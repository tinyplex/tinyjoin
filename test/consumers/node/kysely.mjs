import assert from 'node:assert/strict';

import {Kysely} from 'kysely';
import {Migrator} from 'kysely/migration';
import {TinyJoinDialect} from 'tinyjoin/kysely';
import {create} from 'tinyjoin/node';

const client = await create();
try {
  const db = new Kysely({dialect: new TinyJoinDialect({client})});
  const migrator = new Migrator({
    db,
    provider: {
      getMigrations: async () => ({
        '0001_notes': {
          up: (db) =>
            db.schema
              .createTable('notes')
              .addColumn('id', 'integer', (col) => col.primaryKey())
              .addColumn('body', 'varchar(255)', (col) => col.notNull())
              .execute(),
        },
      }),
    },
  });
  assert.equal((await migrator.migrateToLatest()).error, undefined);
  await db.transaction().execute((trx) =>
    trx.insertInto('notes').values({id: 1, body: 'Hello'}).execute(),
  );
  assert.deepEqual(await db.selectFrom('notes').selectAll().execute(), [
    {id: 1, body: 'Hello'},
  ]);
  assert.deepEqual(
    (await db.introspection.getTables()).map(({name}) => name),
    ['notes'],
  );
  await db.destroy();
  console.log('NODE_KYSELY_CONSUMER_OK');
} finally {
  await client.close();
}
