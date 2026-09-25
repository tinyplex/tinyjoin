import {createRemoteAdapter} from './rpc.js';

export default createRemoteAdapter(
  'sqlite',
  () => new Worker(new URL('./sqlite-worker.js', import.meta.url), {type: 'module'}),
);
