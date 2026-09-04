import { PROTOCOL_VERSION, isRpcResult, isRpcResultHeader, isWorkerEvent, isWorkerResponse, } from '../protocol.js';
import { ClientError } from './error.js';
export class WorkerRpc {
    #worker;
    #pending = new Map();
    #eventListeners = new Set();
    #resultValidation;
    #nextId = 1;
    #disposed = false;
    constructor(worker, resultValidation = 'full') {
        this.#worker = worker;
        this.#resultValidation = resultValidation;
        worker.addEventListener('message', this.#onMessage);
        worker.addEventListener('messageerror', this.#onMessageError);
        worker.addEventListener('error', this.#onError);
    }
    request(method, params) {
        if (this.#disposed) {
            return Promise.reject(new ClientError({
                code: 'WORKER_TERMINATED',
                message: 'The TinyJoin worker has been closed',
            }));
        }
        const id = this.#nextId++;
        const request = {
            v: PROTOCOL_VERSION,
            id,
            method,
            params,
        };
        return new Promise((resolve, reject) => {
            this.#pending.set(id, { method, resolve, reject });
            try {
                this.#worker.postMessage(request);
            }
            catch (error) {
                this.#pending.delete(id);
                reject(clientErrorFromUnknown(error, 'WORKER_POST_FAILED'));
            }
        });
    }
    onEvent(listener) {
        this.#eventListeners.add(listener);
    }
    dispose(error) {
        if (this.#disposed) {
            return;
        }
        this.#disposed = true;
        this.#worker.removeEventListener('message', this.#onMessage);
        this.#worker.removeEventListener('messageerror', this.#onMessageError);
        this.#worker.removeEventListener('error', this.#onError);
        this.#worker.terminate?.();
        const reason = error ??
            new ClientError({
                code: 'WORKER_TERMINATED',
                message: 'The TinyJoin worker has been closed',
            });
        for (const pending of this.#pending.values()) {
            pending.reject(reason);
        }
        this.#pending.clear();
        this.#eventListeners.clear();
    }
    #onMessage = (event) => {
        if (isWorkerEvent(event.data)) {
            for (const listener of this.#eventListeners) {
                listener(event.data);
            }
            return;
        }
        if (!isWorkerResponse(event.data)) {
            this.dispose(new ClientError({
                code: 'PROTOCOL_MISMATCH',
                message: 'The TinyJoin worker sent an invalid protocol message',
            }));
            return;
        }
        const pending = this.#pending.get(event.data.id);
        if (!pending) {
            return;
        }
        if (event.data.ok) {
            const validResult = this.#resultValidation === 'full'
                ? isRpcResult(pending.method, event.data.result)
                : isRpcResultHeader(pending.method, event.data.result);
            if (!validResult) {
                this.dispose(new ClientError({
                    code: 'PROTOCOL_MISMATCH',
                    message: 'The TinyJoin worker returned an invalid result for the requested operation',
                }));
                return;
            }
            this.#pending.delete(event.data.id);
            pending.resolve(event.data.result);
        }
        else {
            this.#pending.delete(event.data.id);
            pending.reject(new ClientError(event.data.error));
        }
    };
    #onMessageError = () => {
        this.dispose(new ClientError({
            code: 'WORKER_MESSAGE_ERROR',
            message: 'The browser could not deserialize a TinyJoin worker message',
        }));
    };
    #onError = (event) => {
        this.dispose(new ClientError({
            code: 'WORKER_ERROR',
            message: event.message || 'The TinyJoin worker crashed',
        }));
    };
}
function clientErrorFromUnknown(error, code) {
    if (error instanceof ClientError) {
        return error;
    }
    return new ClientError({
        code,
        message: error instanceof Error ? error.message : String(error),
    });
}
