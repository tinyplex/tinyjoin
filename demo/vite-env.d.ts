/// <reference types="vite/client" />

interface Window {
  __tinygresDemo?: {
    benchmark(iterations: number): Promise<number[]>;
  };
}
