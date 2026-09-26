import {readFileSync} from 'node:fs';

// Comparative benchmark results are measured by benchmarks/compare/run.mjs
// --publish into site/data/benchmarks.json. Reading committed data keeps the
// documentation build hermetic, as for site/data/sizes.json.
//
// Markdown sources use {{benchmarks.<name>}} placeholders. Text placeholders
// become Markdown before TinyDocs parses a page. A chart placeholder stands
// alone in its paragraph. On the website, that paragraph is swapped for grouped
// bar charts after rendering, since guides render Markdown without inline
// HTML. In the README and llms-full.txt, where those charts would be sanitized
// or unreadable, it becomes an ordinary Markdown table instead.

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

const DOWNLOADS: [keyof Download, string][] = [
  ['raw', 'Download (uncompressed)'],
  ['gzip', 'Download (gzip)'],
  ['brotli', 'Download (Brotli)'],
];

// One group of bars. A null value did not complete, and `missing` says why.
type Measure = {
  label: string;
  values: (number | null)[];
  timedOut: boolean[];
  missing: string[];
  titles: string[];
};
type Chart = {unit: 'ms' | 'bytes'; measures: Measure[]};
type Scale = {
  x: (value: number) => number;
  ticks: [position: number, label: string][];
  intervals: number;
};

const PLACEHOLDER = /\{\{benchmarks\.([a-z-]+)\}\}/g;
const KIB = 1024;
const MIB = 1024 * KIB;

const formatMs = (ms: number): string =>
  ms >= 1000
    ? `${(ms / 1000).toFixed(ms >= 10_000 ? 1 : 2)} s`
    : `${ms.toFixed(ms >= 100 ? 0 : ms >= 10 ? 1 : 2)} ms`;

const formatBytes = (bytes: number): string =>
  bytes >= MIB
    ? `${(bytes / MIB).toFixed(1)} MiB`
    : `${Math.round(bytes / KIB)} KiB`;

const escapeHtml = (text: string): string =>
  text
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;');

const isNumber = (value: number | null): value is number => value != null;

const timeChart = (report: Report, ids: string[]): Chart => ({
  unit: 'ms',
  measures: ids.map((id) => {
    const workload = report.results.find((result) => result.id == id);
    if (workload == null) {
      throw new Error(`No published benchmark result for ${id}`);
    }
    const results = ENGINES.map(([engine]) => workload.engines[engine]);
    const missing = results.map((result) =>
      result?.timedOut ? `over ${formatMs(report.timeoutMs)}` : 'failed',
    );
    return {
      label: workload.label,
      values: results.map((result) => result?.median ?? null),
      timedOut: results.map((result) => result?.timedOut == true),
      missing,
      titles: results.map(
        (result, index) =>
          `${ENGINES[index][1]}: ` +
          (result?.median == null || result.min == null || result.max == null
            ? missing[index]
            : `median ${formatMs(result.median)}, ${formatMs(result.min)} ` +
              `to ${formatMs(result.max)} across ` +
              `${result.samples.length} samples`),
      ),
    };
  }),
});

const downloadChart = (report: Report): Chart => ({
  unit: 'bytes',
  measures: DOWNLOADS.map(([key, label]) => ({
    label,
    values: ENGINES.map(([engine]) => report.download[engine]?.[key] ?? null),
    timedOut: ENGINES.map(() => false),
    missing: ENGINES.map(() => 'not measured'),
    titles: ENGINES.map(
      ([engine, name]) =>
        `${name}: ` +
        (report.download[engine] == null
          ? 'not measured'
          : `${report.download[engine][key].toLocaleString('en-US')} bytes`),
    ),
  })),
});

const groupIds = (report: Report, group: string): string[] =>
  report.results
    .filter((workload) => workload.group.toLowerCase() == group)
    .map(({id}) => id);

// One chart per workload group. Download size leads the startup group, since
// it is fetched before either startup time is measured.
const getCharts = (report: Report, group: string): Chart[] => [
  ...(group == 'startup' ? [downloadChart(report)] : []),
  timeChart(report, groupIds(report, group)),
];

// Each unit applies from its own size upward; an axis uses the largest that
// its longest bar reaches.
const UNITS: {[unit in Chart['unit']]: [size: number, name: string][]} = {
  ms: [
    [1, 'ms'],
    [1000, 's'],
  ],
  bytes: [
    [KIB, 'KiB'],
    [MIB, 'MiB'],
  ],
};

// Every axis is linear from zero, in at most four round steps, so that bar
// lengths compare directly, however far apart the engines are.
const linearScale = (values: number[], unit: Chart['unit']): Scale => {
  const max = Math.max(...values);
  const [size, name] =
    UNITS[unit].filter(([from]) => max >= from).at(-1) ?? UNITS[unit][0];
  const scaled = max / size;
  const magnitude = 10 ** (Math.floor(Math.log10(scaled)) - 1);
  const step = [1, 2, 5, 10, 20, 50]
    .map((multiple) => multiple * magnitude)
    .find((candidate) => Math.ceil(scaled / candidate) <= 4) as number;
  const intervals = Math.ceil(scaled / step);
  return {
    x: (value) => value / (intervals * step * size),
    ticks: Array.from({length: intervals + 1}, (_, index) => [
      index / intervals,
      index == 0 ? '0' : `${Number((index * step).toPrecision(3))} ${name}`,
    ]),
    intervals,
  };
};

// One grouped horizontal bar chart. Each measure is a term in a description
// list, whose one definition holds a bar per engine, so a long label never
// pushes the bars apart and assistive technology reads each measure as a label
// followed by every engine's value. Every bar carries its value at its tip.
const chartHtml = (report: Report, chart: Chart, legend: boolean): string => {
  const format = chart.unit == 'ms' ? formatMs : formatBytes;
  const scale = linearScale(
    chart.measures.flatMap(({values, timedOut}) =>
      values
        .map((value, index) => (timedOut[index] ? report.timeoutMs : value))
        .filter(isNumber),
    ),
    chart.unit,
  );
  const measures = chart.measures.map(
    ({label, values, timedOut, missing, titles}) => {
      const present = values.filter(isNumber);
      const fastest = present.length > 1 ? Math.min(...present) : null;
      const bars = ENGINES.map(([engine, name], index) => {
        // A timed-out engine's bar is striped and reaches the time limit.
        const measured = values[index];
        const length = timedOut[index] ? report.timeoutMs : measured;
        const bar =
          length == null
            ? ''
            : `<span class="bar${timedOut[index] ? ' over' : ''}" ` +
              `style="--x:${scale.x(length).toFixed(4)}"></span>`;
        const text =
          measured == null
            ? escapeHtml(missing[index])
            : measured == fastest
              ? `<b>${format(measured)}</b>`
              : format(measured);
        return (
          `<span class="${engine}" title="${escapeHtml(titles[index])}">` +
          `<span class="engine">${name} </span>${bar}` +
          `<span class="value">${text}</span></span>`
        );
      });
      return `<dt>${escapeHtml(label)}</dt><dd>${bars.join('')}</dd>`;
    },
  );
  const keys = ENGINES.map(
    ([engine, name]) => `<span class="key ${engine}">${name}</span>`,
  ).join('');
  const note =
    chart.unit == 'ms'
      ? `Median of ${report.samples} runs. Shorter is better.`
      : 'Bytes fetched to open a database. Shorter is better.';
  const ticks = scale.ticks
    .map(
      ([position, label]) =>
        `<span style="--x:${position.toFixed(4)}">${label}</span>`,
    )
    .join('');
  return (
    `<figure class="chart" style="--intervals:${scale.intervals}">` +
    '<figcaption>' +
    (legend ? `<span class="legend">${keys}</span>` : '') +
    `<span class="note">${note}</span></figcaption>` +
    `<dl>${measures.join('')}</dl>` +
    `<div class="axis" aria-hidden="true">${ticks}</div></figure>`
  );
};

const chartMarkdown = (chart: Chart): string => {
  const format = chart.unit == 'ms' ? formatMs : formatBytes;
  return [
    `| ${chart.unit == 'ms' ? 'Median time' : 'Size'} | ` +
      `${ENGINES.map(([, name]) => name).join(' | ')} |`,
    `| --- |${' ---: |'.repeat(ENGINES.length)}`,
    ...chart.measures.map(({label, values, missing}) => {
      const present = values.filter(isNumber);
      const fastest = present.length > 1 ? Math.min(...present) : null;
      const cells = values.map((value, index) =>
        value == null
          ? missing[index]
          : value == fastest
            ? `**${format(value)}**`
            : format(value),
      );
      return `| ${label} | ${cells.join(' | ')} |`;
    }),
  ].join('\n');
};

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

const environment = (report: Report): string => {
  const {cpu, logicalCpus, memoryBytes, os, osRelease, browserVersion} =
    report.environment;
  return (
    `Measured on ${report.measuredAt.slice(0, 10)} with headless Chromium ` +
    `${browserVersion} on ${os == 'darwin' ? 'macOS' : os} (${osRelease}), ` +
    `${cpu}, ${logicalCpus} logical CPUs, ` +
    `${Math.round(memoryBytes / 1024 ** 3)} GiB RAM. ` +
    `Each time is the median of ${report.samples} samples.`
  );
};

const renderText = (report: Report, name: string): string => {
  switch (name) {
    case 'versions':
      return versions(report);
    case 'workloads':
      return workloads(report);
    case 'environment':
      return environment(report);
    default:
      throw new Error(`Unknown benchmark placeholder: ${name}`);
  }
};

export type Benchmarks = {
  // Text placeholders to Markdown, leaving chart placeholders in place.
  renderText: (markdown: string) => string;
  // Chart placeholders to Markdown tables, for plain-text outputs.
  renderTables: (markdown: string) => string;
  // Rendered chart paragraphs to chart HTML, for TinyDocs replacers.
  chartReplacers: [RegExp, string][];
};

const createBenchmarks = (report: Report): Benchmarks => {
  const charts = new Map(
    [...new Set(report.results.map(({group}) => group.toLowerCase()))].map(
      (group) => [group, getCharts(report, group)],
    ),
  );
  return {
    renderText: (markdown) =>
      markdown.replaceAll(PLACEHOLDER, (placeholder, name) =>
        charts.has(name) ? placeholder : renderText(report, name),
      ),
    renderTables: (markdown) =>
      markdown.replaceAll(
        PLACEHOLDER,
        (placeholder, name) =>
          charts.get(name)?.map(chartMarkdown).join('\n\n') ?? placeholder,
      ),
    chartReplacers: [...charts].map(([name, list]) => [
      new RegExp(`<p>\\{\\{benchmarks\\.${name}\\}\\}</p>`, 'g'),
      // A replacement string, in which $ would otherwise be special.
      (
        '<div class="charts">' +
        list
          .map((chart, index) => chartHtml(report, chart, index == 0))
          .join('') +
        '</div>'
      ).replaceAll('$', '$$$$'),
    ]),
  };
};

let benchmarks: Benchmarks | undefined;

export const getBenchmarks = (): Benchmarks =>
  (benchmarks ??= createBenchmarks(
    JSON.parse(readFileSync('site/data/benchmarks.json', 'utf8')),
  ));
