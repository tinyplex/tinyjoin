import {readFileSync} from 'node:fs';

// Comparative benchmark results are measured by benchmarks/compare/run.mjs
// --publish into site/data/benchmarks.json, and rendered here into the
// Markdown that publishes them, before TinyDocs parses it. Reading committed
// data keeps the documentation build hermetic, as for site/data/sizes.json.
//
// Each cell carries its value and a thin bar scaled to the slowest (or
// largest) engine in its row. Guides render Markdown without inline HTML, so a
// cell holds a {{bar:width}}value{{/bar}} token, which BAR_REPLACEMENT turns
// into HTML once TinyDocs has rendered the page. Where HTML is sanitized, as on
// GitHub, the style is dropped and an ordinary table of numbers remains.
export const BAR_PATTERN = /\{\{bar:([\d.]+)\}\}(.*?)\{\{\/bar\}\}/g;
export const BAR_REPLACEMENT = '<span class="bar" style="--bar:$1%">$2</span>';

type Result = {
  samples: number[];
  median?: number;
  min?: number;
  max?: number;
  timedOut?: boolean;
  error?: string;
};
type Workload = {
  id: string;
  group: string;
  label: string;
  derivedFrom?: number;
  engines: {[engine: string]: Result};
};
type Download = {raw: number; gzip: number; brotli: number};
type Report = {
  measuredAt: string;
  samples: number;
  timeoutMs: number;
  environment: {[key: string]: any};
  engines: {
    [engine: string]: {
      package: string;
      version: string;
      storage: string;
      engineVersion?: string;
    };
  };
  download: {[engine: string]: Download};
  results: Workload[];
};

const ENGINES: [string, string][] = [
  ['tinyjoin', 'TinyJoin'],
  ['sqlite', 'SQLite'],
  ['pglite', 'PGlite'],
];

const TABLES: {[name: string]: string[]} = {
  'home-startup': ['download', 'cold-open', 'reopen'],
  'home-crud': [
    'insert-transaction',
    'select-pk',
    'select-all',
    'join',
    'update-pk',
    'delete-pk',
  ],
};

const formatMs = (ms: number): string =>
  ms >= 1000
    ? `${(ms / 1000).toFixed(ms >= 10_000 ? 1 : 2)} s`
    : `${ms.toFixed(ms >= 100 ? 0 : ms >= 10 ? 1 : 2)} ms`;

const formatBytes = (bytes: number): string =>
  bytes >= 1024 * 1024
    ? `${(bytes / 1024 / 1024).toFixed(1)} MiB`
    : `${Math.round(bytes / 1024)} KiB`;

// Values are what each cell shows; null is a sample that did not complete.
const row = (
  label: string,
  values: (number | null)[],
  format: (value: number) => string,
  failures: string[] = [],
): string => {
  const present = values.filter((value): value is number => value != null);
  const largest = Math.max(...present);
  const smallest = Math.min(...present);
  const cells = values.map((value, index) => {
    if (value == null) {
      return failures[index] ?? '';
    }
    const width = Math.max((value / largest) * 100, 0.5).toFixed(1);
    const text = format(value);
    return (
      `{{bar:${width}}}` +
      (present.length > 1 && value == smallest ? `**${text}**` : text) +
      '{{/bar}}'
    );
  });
  return `| ${label} | ${cells.join(' | ')} |`;
};

const header = (first: string): string =>
  `| ${first} | ${ENGINES.map(([, name]) => name).join(' | ')} |\n` +
  `| --- |${' ---: |'.repeat(ENGINES.length)}`;

const downloadRow = (report: Report, key: keyof Download = 'gzip'): string =>
  row(
    `Download (${key})`,
    ENGINES.map(([engine]) => report.download[engine]?.[key] ?? null),
    formatBytes,
  );

const workloadRow = (report: Report, id: string): string => {
  const workload = report.results.find((result) => result.id == id);
  if (workload == null) {
    throw new Error(`No published benchmark result for ${id}`);
  }
  const results = ENGINES.map(([engine]) => workload.engines[engine]);
  return row(
    workload.label,
    results.map((result) => result?.median ?? null),
    formatMs,
    results.map((result) =>
      result?.timedOut
        ? `over ${formatMs(report.timeoutMs)}`
        : result?.error
          ? 'failed'
          : '',
    ),
  );
};

const table = (report: Report, ids: string[]): string =>
  [
    header('Median time'),
    ...ids.map((id) =>
      id == 'download' ? downloadRow(report) : workloadRow(report, id),
    ),
  ].join('\n');

// Download size leads the startup group, since it is fetched before either.
const groupTable = (report: Report, group: string): string =>
  table(report, [
    ...(group == 'startup' ? ['download'] : []),
    ...report.results
      .filter((workload) => workload.group.toLowerCase() == group)
      .map(({id}) => id),
  ]);

const workloads = (report: Report): string =>
  [
    '| Workload | Group | Speed test origin |',
    '| --- | --- | --- |',
    ...report.results.map(
      ({id, group, label, derivedFrom}) =>
        `| \`${id}\`: ${label} | ${group} | ` +
        (derivedFrom ? `Test ${derivedFrom}` : '') +
        ' |',
    ),
  ].join('\n');

const downloads = (report: Report): string =>
  [
    header('Bytes to open a database'),
    downloadRow(report, 'raw'),
    downloadRow(report, 'gzip'),
    downloadRow(report, 'brotli'),
  ].join('\n');

const versions = (report: Report): string =>
  [
    '| Engine | Package | Version | Storage |',
    '| --- | --- | --- | --- |',
    ...ENGINES.map(([engine, name]) => {
      const {package: pkg, version, engineVersion, storage} =
        report.engines[engine];
      return (
        `| ${name} | \`${pkg}\` | ${version}` +
        (engineVersion ? ` (${engineVersion})` : '') +
        ` | \`${storage}\` |`
      );
    }),
  ].join('\n');

const environment = (report: Report): string => {
  const {cpu, logicalCpus, memoryBytes, os, osRelease, browserVersion} =
    report.environment;
  return (
    `Measured on ${report.measuredAt.slice(0, 10)} with headless Chromium ` +
    `${browserVersion} on ${os == 'darwin' ? 'macOS' : os} (${osRelease}), ` +
    `${cpu}, ${logicalCpus} logical CPUs, ` +
    `${Math.round(memoryBytes / 1024 ** 3)} GiB RAM. ` +
    `Each value is the median of ${report.samples} samples.`
  );
};

const render = (report: Report, name: string): string => {
  if (TABLES[name]) {
    return table(report, TABLES[name]);
  }
  switch (name) {
    case 'downloads':
      return downloads(report);
    case 'versions':
      return versions(report);
    case 'workloads':
      return workloads(report);
    case 'environment':
      return environment(report);
    default:
      if (report.results.some(({group}) => group.toLowerCase() == name)) {
        return groupTable(report, name);
      }
      throw new Error(`Unknown benchmark placeholder: ${name}`);
  }
};

export const getBenchmarkRenderer = (): ((markdown: string) => string) => {
  const report: Report = JSON.parse(
    readFileSync('site/data/benchmarks.json', 'utf8'),
  );
  return (markdown) =>
    markdown.replaceAll(/\{\{benchmarks\.([a-z-]+)\}\}/g, (_match, name) =>
      render(report, name),
    );
};
