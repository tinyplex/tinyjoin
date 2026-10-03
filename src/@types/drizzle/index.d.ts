/// drizzle

import type {DrizzleConfig} from 'drizzle-orm';
import type {PgRemoteDatabase} from 'drizzle-orm/pg-proxy';
import type {Client} from '../index.js';

/// TinyJoinDatabase
export type TinyJoinDatabase<
  TSchema extends Record<string, unknown> = Record<string, never>,
> = PgRemoteDatabase<TSchema> & {$client: Client};

/// drizzle.drizzle
export function drizzle<
  TSchema extends Record<string, unknown> = Record<string, never>,
>(client: Client, config?: DrizzleConfig<TSchema>): TinyJoinDatabase<TSchema>;

/// MigrationJournal
export interface MigrationJournal {
  /// MigrationJournal.entries
  entries: {tag: string; when: number}[];
}

/// MigrationConfig
export interface MigrationConfig {
  /// MigrationConfig.journal
  journal: MigrationJournal;

  /// MigrationConfig.migrations
  migrations: Record<string, string>;

  /// MigrationConfig.migrationsTable
  migrationsTable?: string;
}

/// drizzle.migrate
export function migrate<
  TSchema extends Record<string, unknown> = Record<string, never>,
>(db: TinyJoinDatabase<TSchema>, config: MigrationConfig): Promise<void>;

/// PushOptions
export interface PushOptions {
  /// PushOptions.version
  version?: number;

  /// PushOptions.drop
  drop?: boolean;

  /// PushOptions.renames
  renames?: Record<string, string>;
}

/// drizzle.push
export function push<
  TSchema extends Record<string, unknown> = Record<string, never>,
>(
  db: TinyJoinDatabase<TSchema>,
  schema: Record<string, unknown>,
  options?: PushOptions,
): Promise<boolean>;
