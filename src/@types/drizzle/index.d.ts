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
