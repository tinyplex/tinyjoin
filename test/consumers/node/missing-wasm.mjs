import assert from 'node:assert/strict';
import {ClientError} from 'tinyjoin';
import {create} from 'tinyjoin/node';

await assert.rejects(
  create(),
  (error) =>
    error instanceof ClientError &&
    /ENOENT|no such file/i.test(error.message),
);
// Startup failure must release the Worker too; the parent verifies natural exit.
console.log('NODE_MISSING_WASM_REJECTED');
