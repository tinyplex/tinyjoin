/// vite

import type {Plugin} from 'vite';

/// TinyjoinOfflineOptions
export interface TinyjoinOfflineOptions {
  /// TinyjoinOfflineOptions.mode
  mode?: 'service-worker' | 'manifest';
  /// TinyjoinOfflineOptions.navigationFallback
  navigationFallback?: string | false;
}

/// tinyjoinOffline
export function tinyjoinOffline(options?: TinyjoinOfflineOptions): Plugin;
