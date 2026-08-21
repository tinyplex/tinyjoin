/**
 * Compile-time contract for the deliberately shared part of PGlite's API.
 *
 * The reference shapes below are pinned to @electric-sql/pglite 0.5.5:
 * https://github.com/electric-sql/pglite/blob/0ad290109d01edbcbcf11e2207a5ff2afc29c14c/packages/pglite/src/interface.ts
 *
 * This is intentionally not the whole PGlite interface. TinyGres accepts JSON
 * parameters, supports only `rowMode` query options, and has no Postgres
 * protocol, extension, notification, or dump APIs. Keeping the compatible
 * subset here makes additions to that promise explicit without adding PGlite's
 * large runtime package as a development dependency.
 */

import {
  Client,
  create,
  type ClientOptions,
  type JsonValue,
  type QueryOptions,
  type ResultField,
  type Results,
  type Transaction,
} from '../../src/index.ts';

type PGlite055RowMode = 'array' | 'object';

interface PGlite055QueryOptions {
  rowMode?: PGlite055RowMode;
}

interface PGlite055ResultField {
  name: string;
  dataTypeID: number;
}

interface PGlite055Results<RowType = Record<string, unknown>> {
  rows: RowType[];
  affectedRows?: number;
  command?: string;
  rowCount?: number;
  fields: PGlite055ResultField[];
}

interface PGlite055CoreTransaction {
  query<RowType>(
    query: string,
    params?: JsonValue[],
    options?: PGlite055QueryOptions,
  ): Promise<PGlite055Results<RowType>>;
  sql<RowType>(
    sqlStrings: TemplateStringsArray,
    ...params: JsonValue[]
  ): Promise<PGlite055Results<RowType>>;
  exec(
    query: string,
    options?: PGlite055QueryOptions,
  ): Promise<Array<PGlite055Results>>;
  rollback(): Promise<void>;
  readonly closed: boolean;
}

interface PGlite055Core {
  readonly waitReady: Promise<void>;
  readonly ready: boolean;
  readonly closed: boolean;
  query<RowType>(
    query: string,
    params?: JsonValue[],
    options?: PGlite055QueryOptions,
  ): Promise<PGlite055Results<RowType>>;
  sql<RowType>(
    sqlStrings: TemplateStringsArray,
    ...params: JsonValue[]
  ): Promise<PGlite055Results<RowType>>;
  exec(
    query: string,
    options?: PGlite055QueryOptions,
  ): Promise<Array<PGlite055Results>>;
  transaction<Result>(
    callback: (transaction: PGlite055CoreTransaction) => Promise<Result>,
  ): Promise<Result>;
  close(): Promise<void>;
}

interface PGliteShapedCreate {
  (): Promise<PGlite055Core>;
  (options: ClientOptions): Promise<PGlite055Core>;
  (dataDir: string, options?: ClientOptions): Promise<PGlite055Core>;
}

function assertStructuralCompatibility(
  client: Client,
  transaction: Transaction,
  results: Results<{id: number}>,
): void {
  const compatibleClient: PGlite055Core = client;
  const compatibleTransaction: PGlite055CoreTransaction = transaction;
  const compatibleCreate: PGliteShapedCreate = create;

  // TinyGres supports immediate construction with the same lifecycle
  // properties; its positional dataDir form deliberately lives on async
  // `create`, not `new`.
  const constructed = new Client();
  const constructedWithOptions = new Client({dataDir: 'memory://'});

  // Pin public result and option spellings independently of method variance.
  const queryOptions: QueryOptions = {rowMode: 'array'};
  const resultField: ResultField = {name: 'id', dataTypeID: 20};
  const compatibleResults: PGlite055Results<{id: number}> = results;

  void [
    compatibleClient,
    compatibleTransaction,
    compatibleCreate,
    constructed,
    constructedWithOptions,
    queryOptions,
    resultField,
    compatibleResults,
  ];
}

async function representativeUsage(): Promise<void> {
  const transient = await create();
  const explicitMemory = await create('memory://');
  const persistent = await create('opfs://pglite-compatibility');

  const objectResult = await transient.query<{id: number}>(
    'SELECT id FROM tasks WHERE id = $1',
    [1],
    {rowMode: 'object'},
  );
  const arrayResult = await transient.query<[number]>(
    'SELECT id FROM tasks',
    [],
    {rowMode: 'array'},
  );
  const taggedResult = await transient.sql<{id: number}>`
    SELECT id FROM tasks WHERE id = ${1}
  `;
  const scriptResults = await transient.exec(
    'CREATE TABLE tasks (id INTEGER PRIMARY KEY); SELECT id FROM tasks;',
    {rowMode: 'object'},
  );

  const callbackResult = await transient.transaction(async (tx) => {
    const result = await tx.query<{id: number}>('SELECT id FROM tasks');
    const tagged = await tx.sql<{id: number}>`SELECT id FROM tasks`;
    const script = await tx.exec('SELECT id FROM tasks');
    await tx.rollback();
    const transactionClosed: boolean = tx.closed;
    return {result, tagged, script, transactionClosed};
  });

  await transient.waitReady;
  const ready: boolean = transient.ready;
  const closed: boolean = transient.closed;
  await transient.close();

  void [
    explicitMemory,
    persistent,
    objectResult.rows,
    objectResult.fields,
    objectResult.fields[0]?.dataTypeID,
    arrayResult.rows,
    taggedResult.rows,
    scriptResults,
    callbackResult,
    ready,
    closed,
  ];
}

void assertStructuralCompatibility;
void representativeUsage;
