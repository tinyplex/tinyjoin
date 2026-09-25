import {createRemoteAdapter} from './rpc.js';

export default createRemoteAdapter(
  'pglite',
  () => new Worker(new URL('./pglite-worker.js', import.meta.url), {type: 'module'}),
);
