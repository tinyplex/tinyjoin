import {
  type Client as ClientImplementation,
  type ClientError as ClientErrorImplementation,
  type ClientOptions as ClientOptionsImplementation,
  type ColumnSchema as ColumnSchemaImplementation,
  type ColumnType as ColumnTypeImplementation,
  create as createImplementation,
  type DataDir as DataDirImplementation,
  type IndexSchema as IndexSchemaImplementation,
  type JsonPrimitive as JsonPrimitiveImplementation,
  type JsonValue as JsonValueImplementation,
  type PreparedStatement as PreparedStatementImplementation,
  type QueryOptions as QueryOptionsImplementation,
  type ResultField as ResultFieldImplementation,
  type Results as ResultsImplementation,
  type Row as RowImplementation,
  type RowMode as RowModeImplementation,
  type Schema as SchemaImplementation,
  type SerializedError as SerializedErrorImplementation,
  type SetSchemaOptions as SetSchemaOptionsImplementation,
  type SubscriptionOptions as SubscriptionOptionsImplementation,
  type TableSchema as TableSchemaImplementation,
  type TablesChangedEvent as TablesChangedEventImplementation,
  type Transaction as TransactionImplementation,
  type WorkerLike as WorkerLikeImplementation,
} from '../../src/index.ts';
import type {
  Client as ClientDeclaration,
  ClientError as ClientErrorDeclaration,
  ClientOptions as ClientOptionsDeclaration,
  ColumnSchema as ColumnSchemaDeclaration,
  ColumnType as ColumnTypeDeclaration,
  create as createDeclaration,
  DataDir as DataDirDeclaration,
  IndexSchema as IndexSchemaDeclaration,
  JsonPrimitive as JsonPrimitiveDeclaration,
  JsonValue as JsonValueDeclaration,
  PreparedStatement as PreparedStatementDeclaration,
  QueryOptions as QueryOptionsDeclaration,
  ResultField as ResultFieldDeclaration,
  Results as ResultsDeclaration,
  Row as RowDeclaration,
  RowMode as RowModeDeclaration,
  Schema as SchemaDeclaration,
  SerializedError as SerializedErrorDeclaration,
  SetSchemaOptions as SetSchemaOptionsDeclaration,
  SubscriptionOptions as SubscriptionOptionsDeclaration,
  TableSchema as TableSchemaDeclaration,
  TablesChangedEvent as TablesChangedEventDeclaration,
  Transaction as TransactionDeclaration,
  WorkerLike as WorkerLikeDeclaration,
} from '../../src/@types/index.d.ts';
import {startWorker as startWorkerImplementation} from '../../src/worker/index.ts';
import type {startWorker as startWorkerDeclaration} from '../../src/@types/worker/index.d.ts';

type Equivalent<Left, Right> = [Left] extends [Right]
  ? [Right] extends [Left]
    ? true
    : false
  : false;
type Assert<Condition extends true> = Condition;
type PublicShape<Value> = {[Key in keyof Value]: Value[Key]};
type ImplementationModule = typeof import('../../src/index.ts');
type DeclarationModule = typeof import('../../src/@types/index.d.ts');
type ImplementationWorkerModule = typeof import('../../src/worker/index.ts');
type DeclarationWorkerModule =
  typeof import('../../src/@types/worker/index.d.ts');

type ClientParity = Assert<
  Equivalent<
    PublicShape<ClientImplementation>,
    PublicShape<ClientDeclaration>
  >
>;
type ClientErrorParity = Assert<
  Equivalent<
    PublicShape<ClientErrorImplementation>,
    PublicShape<ClientErrorDeclaration>
  >
>;
type PublicValueParity = [
  Assert<
    Equivalent<keyof ImplementationModule, keyof DeclarationModule>
  >,
  Assert<
    typeof createImplementation extends typeof createDeclaration ? true : false
  >,
  Assert<
    Equivalent<
      ConstructorParameters<ImplementationModule['Client']>,
      ConstructorParameters<DeclarationModule['Client']>
    >
  >,
  Assert<
    Equivalent<
      ConstructorParameters<ImplementationModule['ClientError']>,
      ConstructorParameters<DeclarationModule['ClientError']>
    >
  >,
  Assert<
    Equivalent<
      keyof ImplementationWorkerModule,
      keyof DeclarationWorkerModule
    >
  >,
  Assert<
    Equivalent<
      typeof startWorkerImplementation,
      typeof startWorkerDeclaration
    >
  >,
];
type PublicTypeParity = [
  Assert<Equivalent<ClientOptionsImplementation, ClientOptionsDeclaration>>,
  Assert<Equivalent<ColumnSchemaImplementation, ColumnSchemaDeclaration>>,
  Assert<Equivalent<ColumnTypeImplementation, ColumnTypeDeclaration>>,
  Assert<Equivalent<DataDirImplementation, DataDirDeclaration>>,
  Assert<Equivalent<IndexSchemaImplementation, IndexSchemaDeclaration>>,
  Assert<Equivalent<JsonPrimitiveImplementation, JsonPrimitiveDeclaration>>,
  Assert<Equivalent<JsonValueImplementation, JsonValueDeclaration>>,
  Assert<
    Equivalent<
      PreparedStatementImplementation,
      PreparedStatementDeclaration
    >
  >,
  Assert<Equivalent<QueryOptionsImplementation, QueryOptionsDeclaration>>,
  Assert<Equivalent<ResultFieldImplementation, ResultFieldDeclaration>>,
  Assert<Equivalent<ResultsImplementation, ResultsDeclaration>>,
  Assert<Equivalent<RowImplementation, RowDeclaration>>,
  Assert<Equivalent<RowModeImplementation, RowModeDeclaration>>,
  Assert<Equivalent<SchemaImplementation, SchemaDeclaration>>,
  Assert<
    Equivalent<SetSchemaOptionsImplementation, SetSchemaOptionsDeclaration>
  >,
  Assert<Equivalent<SerializedErrorImplementation, SerializedErrorDeclaration>>,
  Assert<
    Equivalent<
      SubscriptionOptionsImplementation,
      SubscriptionOptionsDeclaration
    >
  >,
  Assert<
    Equivalent<TablesChangedEventImplementation, TablesChangedEventDeclaration>
  >,
  Assert<Equivalent<TableSchemaImplementation, TableSchemaDeclaration>>,
  Assert<Equivalent<TransactionImplementation, TransactionDeclaration>>,
  Assert<Equivalent<WorkerLikeImplementation, WorkerLikeDeclaration>>,
];

void [
  null as
    | ClientParity
    | ClientErrorParity
    | PublicValueParity
    | PublicTypeParity
    | null,
];
