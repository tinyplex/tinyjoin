const state = element<HTMLElement>('[data-testid="state"]');
const report = element<HTMLElement>('[data-testid="report"]');
const error = element<HTMLElement>('[data-testid="error"]');
const worker = new Worker(new URL('./paged-memory-worker.mts', import.meta.url), {
  name: 'tinyjoin-page-native-memory-proof',
  type: 'module',
});

worker.addEventListener(
  'message',
  (event: MessageEvent<unknown>) => {
    if (
      typeof event.data === 'object' &&
      event.data !== null &&
      'ok' in event.data &&
      event.data.ok === true &&
      'report' in event.data
    ) {
      report.textContent = JSON.stringify(event.data.report);
      state.textContent = 'Ready';
    } else {
      showError(
        typeof event.data === 'object' &&
          event.data !== null &&
          'message' in event.data
          ? String(event.data.message)
          : 'The page-native Worker returned an invalid result',
      );
    }
    worker.terminate();
  },
  {once: true},
);
worker.addEventListener(
  'error',
  (event) => {
    showError(event.message);
    worker.terminate();
  },
  {once: true},
);
worker.postMessage({type: 'run'});

function element<ElementType extends Element>(selector: string): ElementType {
  const value = document.querySelector<ElementType>(selector);
  if (value === null) {
    throw new Error(`Missing fixture element: ${selector}`);
  }
  return value;
}

function showError(message: string): void {
  state.textContent = 'Failed';
  error.hidden = false;
  error.textContent = message;
}
