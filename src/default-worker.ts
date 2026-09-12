/**
 * Resolves the default Worker entry that create() constructs when the
 * application does not supply a Worker of its own.
 *
 * This lives beside the package entry point rather than in the client, because
 * the published client is one bundle at this depth: keeping the URL here makes
 * the relative path correct in the source tree and in the bundle alike.
 */
export const defaultWorkerUrl = (): URL =>
  new URL('./worker/default-entry.js', import.meta.url);
