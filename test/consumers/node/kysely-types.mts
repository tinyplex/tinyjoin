import {Kysely} from 'kysely';
import {TinyJoinDialect, type TinyJoinDialectConfig} from 'tinyjoin/kysely';
import {type Client, create} from 'tinyjoin/node';

interface Database {
  notes: {id: number; body: string};
}

export async function exerciseKyselyDeclarations(): Promise<void> {
  const client: Client = await create();
  try {
    const config: TinyJoinDialectConfig = {client};
    const db = new Kysely<Database>({dialect: new TinyJoinDialect(config)});
    const rows: {body: string}[] = await db
      .selectFrom('notes')
      .select('body')
      .where('id', '=', 1)
      .execute();
    void rows;
    await db.destroy();
  } finally {
    await client.close();
  }
}
