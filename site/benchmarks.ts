import {readFileSync} from 'node:fs';

// Comparative benchmark results are measured by benchmarks/compare/run.mjs
// --publish into site/data/benchmarks.json, and with --storage memory into
// site/data/benchmarks-memory.json. Reading committed data keeps the
// documentation build hermetic, as for site/data/sizes.json.
//
// Markdown sources use {{benchmarks.<name>}} placeholders, and
// {{benchmarks.memory-<name>}} for the in-memory results. Text placeholders
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

// One group of bars. A null value did not complete, and `missing` says why. A
// range spans an engine's fastest and slowest samples, where it has samples.
type Measure = {
  label: string;
  values: (number | null)[];
  ranges: ([min: number, max: number] | null)[];
  timedOut: boolean[];
  missing: string[];
  titles: string[];
};
type Chart = {unit: 'ms' | 'bytes'; measures: Measure[]};

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
      ranges: results.map((result) =>
        result?.median == null || result.min == null || result.max == null
          ? null
          : [result.min, result.max],
      ),
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
    ranges: ENGINES.map(() => null),
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

// Each engine's bar length for a measure: a timed-out engine's reaches the
// time limit, and one that did not complete has none.
const lengths = (report: Report, {values, timedOut}: Measure) =>
  values.map((value, index) => (timedOut[index] ? report.timeoutMs : value));

// The engines of a measure, fastest first, and those that did not complete
// last, keeping the engines' own order among equals.
const ranked = (report: Report, measure: Measure): number[] => {
  const measured = lengths(report, measure);
  return ENGINES.map((_, index) => index).sort(
    (a, b) => (measured[a] ?? Infinity) - (measured[b] ?? Infinity) || a - b,
  );
};

// The plots, in pixels, that a value can be relied on to fit inside a bar of:
// any chart's, and one at least 40rem wide.
const NARROW_PLOT = 244;
const WIDE_PLOT = 334;

// About how wide a value is inside a bar, in pixels: small tabular figures and
// a unit, with padding on either side.
const valueWidth = (text: string): number => text.length * 6.4 + 12;

const fraction = (value: number): string => value.toFixed(4);

// One grouped horizontal bar chart. Each measure is a term in a description
// list, whose one definition holds a bar per engine, fastest first, so a long
// label never pushes the bars apart and assistive technology reads each measure
// as a label followed by every engine in the order they finished. Each measure
// is drawn to its own scale, linear from zero, on which its slowest run reaches
// the full width, so a quick workload's bars are as legible as a slow one's.
// Each bar is named, carries its value inside it where the value fits, and ends
// in a faint bracket from its engine's fastest run to its slowest.
const chartHtml = (report: Report, chart: Chart): string => {
  const format = chart.unit == 'ms' ? formatMs : formatBytes;
  const measures = chart.measures.map((measure) => {
    const {label, values, ranges, timedOut, missing, titles} = measure;
    const measured = lengths(report, measure);
    const longest = Math.max(
      ...measured.map((length, index) =>
        Math.max(length ?? 0, timedOut[index] ? 0 : (ranges[index]?.[1] ?? 0)),
      ),
    );
    const bars = ranked(report, measure).map((index) => {
      const [engine, name] = ENGINES[index];
      const value = values[index];
      const x = (measured[index] ?? 0) / longest;
      // A timed-out engine's bar is striped, since it is a lower bound, and
      // has no range to show.
      const range = timedOut[index] ? null : ranges[index];
      const bar =
        measured[index] == null
          ? ''
          : `<span class="bar${timedOut[index] ? ' over' : ''}" ` +
            `style="--x:${fraction(x)}"></span>`;
      const [low, high] = range?.map((end) => end / longest) ?? [x, x];
      const bracket =
        range == null
          ? ''
          : `<span class="range" style="--low:${fraction(low)};` +
            `--high:${fraction(high)}"></span>`;
      const shown = value == null ? missing[index] : format(value);
      // A value inside its bar must end before the bar's range begins.
      const room = Math.min(x, low);
      const fit =
        room * NARROW_PLOT >= valueWidth(shown)
          ? 'all'
          : room * WIDE_PLOT >= valueWidth(shown)
            ? 'wide'
            : 'none';
      return (
        `<span class="${engine}" title="${escapeHtml(titles[index])}">` +
        `<span class="engine">${name}</span><span class="plot">${bar}` +
        `${bracket}<span class="value" data-fit="${fit}" ` +
        `style="--end:${fraction(Math.max(x, high))}">${escapeHtml(shown)}` +
        '</span></span></span>'
      );
    });
    return `<dt>${escapeHtml(label)}</dt><dd>${bars.join('')}</dd>`;
  });
  const note =
    chart.unit == 'ms'
      ? `Median of ${report.samples} runs, fastest first. ` +
        'Brackets span the fastest to the slowest run.'
      : 'Bytes fetched to open a database, smallest first.';
  return (
    `<figure class="chart"><figcaption>${note}</figcaption>` +
    `<dl>${measures.join('')}</dl></figure>`
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

// How often each engine was the fastest, the second, and the slowest across a
// report's timed workloads. Engines that tie share a place, and an engine that
// did not complete counts as the slowest.
const placings = (report: Report): string => {
  const counts = ENGINES.map(() => [0, 0, 0]);
  for (const workload of report.results) {
    const measure = timeChart(report, [workload.id]).measures[0];
    const measured = lengths(report, measure).map(
      (length) => length ?? Infinity,
    );
    measured.forEach((length, index) => {
      const faster = measured.filter((other) => other < length).length;
      counts[index][Math.min(faster, 2)] += 1;
    });
  }
  return [
    '| Engine | Fastest | Second | Slowest |',
    '| --- | ---: | ---: | ---: |',
    ...ENGINES.map(
      ([, name], index) => `| ${name} | ${counts[index].join(' | ')} |`,
    ),
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

const MEMORY = 'memory-';

// A placeholder's report, and its name within that report.
const named = (
  reports: {opfs: Report; memory: Report},
  name: string,
): [Report, string] =>
  name.startsWith(MEMORY)
    ? [reports.memory, name.slice(MEMORY.length)]
    : [reports.opfs, name];

const renderText = (report: Report, name: string): string => {
  switch (name) {
    case 'versions':
      return versions(report);
    case 'workloads':
      return workloads(report);
    case 'environment':
      return environment(report);
    case 'placings':
      return placings(report);
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

const createBenchmarks = (reports: {
  opfs: Report;
  memory: Report;
}): Benchmarks => {
  // Each report's charts: its download sizes, and one for each workload group,
  // named by the group, and by memory- and the group for the in-memory results.
  const charts = new Map<string, [Report, Chart]>();
  for (const [prefix, report] of [
    ['', reports.opfs],
    [MEMORY, reports.memory],
  ] as const) {
    charts.set(prefix + 'download', [report, downloadChart(report)]);
    for (const group of new Set(
      report.results.map(({group}) => group.toLowerCase()),
    )) {
      charts.set(prefix + group, [
        report,
        timeChart(report, groupIds(report, group)),
      ]);
    }
  }
  return {
    renderText: (markdown) =>
      markdown.replaceAll(PLACEHOLDER, (placeholder, name) =>
        charts.has(name) ? placeholder : renderText(...named(reports, name)),
      ),
    renderTables: (markdown) =>
      markdown.replaceAll(
        PLACEHOLDER,
        (placeholder, name) => {
          const chart = charts.get(name)?.[1];
          return chart ? chartMarkdown(chart) : placeholder;
        },
      ),
    chartReplacers: [...charts].map(([name, [report, chart]]) => [
      new RegExp(`<p>\\{\\{benchmarks\\.${name}\\}\\}</p>`, 'g'),
      // A replacement string, in which $ would otherwise be special.
      `<div class="charts">${chartHtml(report, chart)}</div>`.replaceAll(
        '$',
        '$$$$',
      ),
    ]),
  };
};

let benchmarks: Benchmarks | undefined;

const readReport = (file: string): Report =>
  JSON.parse(readFileSync(file, 'utf8'));

export const getBenchmarks = (): Benchmarks =>
  (benchmarks ??= createBenchmarks({
    opfs: readReport('site/data/benchmarks.json'),
    memory: readReport('site/data/benchmarks-memory.json'),
  }));
