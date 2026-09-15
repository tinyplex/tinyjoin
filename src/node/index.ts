import {create as createClient, type Client} from '../index.js';
import {createNodeWorker} from './worker.js';

/** Opens an independent in-memory database in a Node.js worker thread. */
export const create = async (dataDir?: 'memory://'): Promise<Client> => {
  if (dataDir !== undefined && dataDir !== 'memory://') {
    throw new TypeError('TinyJoin in Node.js supports only memory:// databases');
  }
  return createClient({workerFactory: createNodeWorker});
};
