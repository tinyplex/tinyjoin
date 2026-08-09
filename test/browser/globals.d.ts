interface Window {
  __tinygresTest?: {
    benchmark(iterations: number): Promise<number[]>;
  };
}
