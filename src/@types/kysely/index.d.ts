/// kysely

import type {
  DatabaseIntrospector,
  Dialect,
  DialectAdapter,
  Driver,
  QueryCompiler,
} from 'kysely';
import type {Client} from '../index.js';

/// TinyJoinDialectConfig
export interface TinyJoinDialectConfig {
  /// TinyJoinDialectConfig.client
  client: Client;
}

/// TinyJoinDialect
export class TinyJoinDialect implements Dialect {
  /// TinyJoinDialect.constructor
  constructor(config: TinyJoinDialectConfig);

  /// TinyJoinDialect.createDriver
  createDriver(): Driver;

  /// TinyJoinDialect.createQueryCompiler
  createQueryCompiler(): QueryCompiler;

  /// TinyJoinDialect.createAdapter
  createAdapter(): DialectAdapter;

  /// TinyJoinDialect.createIntrospector
  createIntrospector(): DatabaseIntrospector;
}
