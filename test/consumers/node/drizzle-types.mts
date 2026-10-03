import {eq} from 'drizzle-orm';
import {integer, pgTable, text} from 'drizzle-orm/pg-core';
import {type TinyJoinDatabase, drizzle} from 'tinyjoin/drizzle';
import {type Client, create} from 'tinyjoin/node';

const notes = pgTable('notes', {
  id: integer('id').primaryKey(),
  body: text('body').notNull(),
});

export async function exerciseDrizzleDeclarations(): Promise<void> {
  const client: Client = await create();
  try {
    const db: TinyJoinDatabase<{notes: typeof notes}> = drizzle(client, {
      schema: {notes},
    });
    const $client: Client = db.$client;
    const rows: {id: number; body: string}[] = await db
      .select()
      .from(notes)
      .where(eq(notes.id, 1));
    const found: {body: string} | undefined = await db.query.notes.findFirst({
      columns: {body: true},
    });
    void [$client, rows, found];
  } finally {
    await client.close();
  }
}
