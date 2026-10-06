export type Probe = {
  name: string;
  primitive?: string;
  approximate?: boolean;
  probe: () => number | Promise<number>;
};

export type FlushProbe = Probe & {
  primitive: string;
  probe: () => Promise<number>;
  close: () => Promise<void>;
};

export type SettlerState = {
  baselineMs: number | null;
  referenceMs: number | null;
  quietestMs: number | null;
  slowestMs: number;
  waitedSeconds: number;
  unrecoveredRounds?: number;
  unrecoveredSamples?: number;
  primitive?: string;
  approximate?: boolean;
  gaveUp?: boolean;
  fault?: string;
};

export type SettleResult = {
  ms: number | null;
  waitedSeconds: number;
  recovered: boolean;
};

export type SettlerOptions = {
  tolerance: (referenceMs: number) => number;
  stepSeconds: number;
  maxSeconds: number;
  reference?: 'start' | 'repeated';
  floor?: (baselineMs: number) => number;
  counter?: 'unrecoveredRounds' | 'unrecoveredSamples';
  giveUpAfter?: number;
  baselineWindows?: number;
  baselineGapSeconds?: number;
  sleep?: (seconds: number) => Promise<void>;
  log?: (message: string) => void;
  warn?: (message: string) => void;
};

export type Settler = {
  settle: (label: string) => Promise<SettleResult>;
  report: () => SettlerState;
};

export const FLUSH_BYTES: number;
export function createCpuProbe(): Probe & {probe: () => number};
export function createFlushProbe(
  baseDir: string,
  options?: {window?: number; warn?: (message: string) => void},
): Promise<FlushProbe>;
export function createNodeFlushProbe(
  baseDir: string,
  options?: {window?: number},
): Promise<FlushProbe>;
export function createSettler(
  probe: Probe,
  options: SettlerOptions,
): Promise<Settler>;
