export class ClientError extends Error {
    code;
    details;
    retryable;
    constructor(error) {
        super(error.message);
        this.name = 'ClientError';
        this.code = error.code;
        this.details =
            error.details === undefined ? undefined : structuredClone(error.details);
        this.retryable = error.retryable ?? false;
    }
}
