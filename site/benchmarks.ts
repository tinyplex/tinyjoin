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
  flushMs?: (number | null)[];
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

const downloadMeasure = (
  report: Report,
  [key, label]: [keyof Download, string],
): Measure => ({
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
});

const downloadChart = (report: Report): Chart => ({
  unit: 'bytes',
  measures: DOWNLOADS.map((download) => downloadMeasure(report, download)),
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
const placingCounts = (report: Report): number[][] => {
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
  return counts;
};

const placings = (report: Report): string => {
  const counts = placingCounts(report);
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

const count = (n: number, noun: string): string =>
  `${n} ${noun}${n == 1 ? '' : 's'}`;

// What the runner's probes read in the run shown, from whichever fields the
// report holds: one made before the flush probe existed has only the CPU
// probe's first three figures, and one whose flush probe failed before the
// run started has no baseline for it.
const probes = (report: Report): string => {
  const {cpuProbe, flushProbe} = report.environment;
  if (cpuProbe == null) {
    return '';
  }
  const sentences = [
    `In the run shown, the CPU probe took ${formatMs(cpuProbe.baselineMs)} ` +
      `at the start and ${formatMs(cpuProbe.slowestMs)} at its slowest, ` +
      `and the runner waited ${cpuProbe.waitedSeconds} s in all for the CPU` +
      (cpuProbe.unrecoveredRounds
        ? `, and ${count(cpuProbe.unrecoveredRounds, 'round')} began ` +
          'before it had recovered.'
        : '.'),
  ];
  if (flushProbe?.baselineMs != null) {
    sentences.push(
      'A write and flush usually took ' +
        `${formatMs(flushProbe.referenceMs ?? flushProbe.baselineMs)}, and ` +
        `${formatMs(flushProbe.slowestMs)} at its slowest, and the runner ` +
        `waited ${flushProbe.waitedSeconds} s in all for the disk` +
        (flushProbe.unrecoveredSamples
          ? `, and ${count(flushProbe.unrecoveredSamples, 'sample')} began ` +
            'before it had recovered'
          : '') +
        (flushProbe.gaveUp ? ', and the runner gave up waiting on it' : '') +
        (flushProbe.approximate
          ? '; the flush timed was a full flush of the drive rather than ' +
            'the barrier the engines issue.'
          : '.'),
    );
  }
  return sentences.join(' ');
};

// The homepage's claims are written here rather than in its Markdown, so that
// they are only ever as strong as the published run supports: the
// superlatives stand while TinyJoin's download is the smallest of the three
// and it is the fastest engine in every workload, and a later run that shows
// otherwise leaves the page with plainer words and the count it measured.
const OTHERS = `${ENGINES[1][1]} and ${ENGINES[2][1]}`;

const sweeps = (report: Report): boolean =>
  placingCounts(report)[0][0] == report.results.length;

// The words the homepage's headline begins with.
const epithet = (report: Report): string =>
  isSmallest(report) && sweeps(report)
    ? 'The smallest, fastest'
    : 'A tiny, fast';

// The heading of the homepage's first section.
const heading = (report: Report): string =>
  isSmallest(report) && sweeps(report)
    ? `Smaller and faster than ${OTHERS}`
    : `Measured against ${OTHERS}`;

// The claim under the homepage's headline, up to the words that link to the
// guide: how TinyJoin's download and its placings compare with the other two
// engines' in the published run.
const claim = (report: Report): string => {
  const total = report.results.length;
  const speed = sweeps(report)
    ? `faster than both in all ${total} workloads`
    : `the fastest of the three in ${placingCounts(report)[0][0]} of the ` +
      `${total} workloads`;
  return isSmallest(report)
    ? `Smaller than ${OTHERS}, and ${speed}`
    : `Beside ${OTHERS}, ${speed}`;
};

// The count of TinyJoin's wins, for the homepage's first section, whose heading
// has already said how it compares.
const tally = (report: Report): string => {
  const total = report.results.length;
  return sweeps(report)
    ? `TinyJoin is the fastest in all ${total} of the workloads we measure`
    : `TinyJoin is the fastest in ${placingCounts(report)[0][0]} of the ` +
        `${total} workloads we measure`;
};

// An engine's compressed download, as the homepage compares them.
const downloadOf = (report: Report, engine: string): string => {
  const gzip = report.download[engine]?.gzip;
  if (gzip == null) {
    throw new Error(`No published download size for ${engine}`);
  }
  return formatBytes(gzip);
};

const renderText = (report: Report, name: string): string => {
  switch (name) {
    case 'epithet':
      return epithet(report);
    case 'heading':
      return heading(report);
    case 'claim':
      return claim(report);
    case 'tally':
      return tally(report);
    case 'sqlite-download':
      return downloadOf(report, 'sqlite');
    case 'pglite-download':
      return downloadOf(report, 'pglite');
    case 'versions':
      return versions(report);
    case 'workloads':
      return workloads(report);
    case 'environment':
      return environment(report);
    case 'placings':
      return placings(report);
    case 'probes':
      return probes(report);
    default:
      throw new Error(`Unknown benchmark placeholder: ${name}`);
  }
};

// The benchmark card summarizes a report in a 1600x900 page to share as an
// image. Its template, site/benchmark-card.html, holds the layout and fixed
// copy, and its {{card.<name>}} placeholders take the measurements below.

// The measures featured in the card's tiles, each with its title: the gzip
// download, or a timed workload.
const CARD_TILES: [measure: string, title: string][] = [
  ['download', 'Download, gzip'],
  ['cold-open', 'First open'],
  ['select-all', 'Read 10,000 rows'],
];

// Short names for the other workloads that the card lists TinyJoin's leads in.
const CARD_NAMES: {[id: string]: string} = {
  'cold-open': 'First open',
  reopen: 'Reopen',
  'insert-autocommit': 'Autocommit inserts',
  'insert-transaction': 'Bulk inserts',
  'insert-indexed': 'Indexed inserts',
  'insert-batch': 'Batched inserts',
  'select-pk': 'Point reads',
  'select-scan': 'Range scans',
  'select-like': 'LIKE scans',
  'select-indexed': 'Indexed ranges',
  'select-all': 'Full reads',
  'group-by': 'GROUP BY',
  join: 'Joins',
  'update-pk': 'Point updates',
  'update-scan': 'Range updates',
  upsert: 'Upserts',
  'delete-pk': 'Point deletes',
  'delete-like': 'LIKE deletes',
  'delete-range': 'Range deletes',
  'create-index': 'Index builds',
};

const CARD_PLACEHOLDER = /\{\{card\.([a-z]+)\}\}/g;

const formatRatio = (ratio: number): string =>
  `${ratio >= 10 ? Math.round(ratio) : ratio.toFixed(1)}×`;

// How TinyJoin compares with the nearest other engine in a measure: whether it
// leads, being faster than every other, and the ratio of the slower of the two
// to the faster.
const compare = (
  report: Report,
  measure: Measure,
): {leads: boolean; ratio: number; other: string} => {
  const measured = lengths(report, measure);
  const [first, second] = ranked(report, measure);
  const other = first == 0 ? second : first;
  const [tinyjoin, nearest] = [measured[0], measured[other]];
  if (tinyjoin == null || nearest == null) {
    throw new Error(`Cannot compare the engines in ${measure.label}`);
  }
  const leads = first == 0 && nearest > tinyjoin;
  return {
    leads,
    ratio: leads ? nearest / tinyjoin : tinyjoin / nearest,
    other: ENGINES[other][1],
  };
};

// Whether TinyJoin's compressed download is the smallest of the three.
const isSmallest = (report: Report): boolean =>
  compare(report, downloadMeasure(report, ['gzip', 'Download (gzip)'])).leads;

// A tile: its title, how TinyJoin compares with the nearest other engine, and
// a bar per engine, fastest first, on a scale on which the slowest reaches the
// full width.
const cardTile = (
  report: Report,
  measure: Measure,
  unit: Chart['unit'],
  title: string,
): string => {
  const {leads, ratio, other} = compare(report, measure);
  if (!leads) {
    console.warn(
      `The benchmark card features ${title}, ` +
        'in which TinyJoin is not the fastest',
    );
  }
  const format = unit == 'ms' ? formatMs : formatBytes;
  const comparison =
    unit == 'ms' ? (leads ? 'faster' : 'slower') : leads ? 'smaller' : 'larger';
  const measured = lengths(report, measure);
  const longest = Math.max(...measured.map((length) => length ?? 0));
  const bars = ranked(report, measure).map((index) => {
    const [engine, name] = ENGINES[index];
    const value = measure.values[index];
    const x = (measured[index] ?? 0) / longest;
    const shown = value == null ? measure.missing[index] : format(value);
    return (
      `<dt class="${engine}">${name}</dt><dd class="${engine}">` +
      `<span class="bar" style="--x:${fraction(x)}"></span>` +
      `${escapeHtml(shown)}</dd>`
    );
  });
  return (
    `<figure class="tile"><figcaption>${escapeHtml(title)}</figcaption>` +
    `<p><b>${formatRatio(ratio)}</b> ${comparison} than ${other}</p>` +
    `<dl>${bars.join('')}</dl></figure>`
  );
};

const cardTiles = (report: Report): string =>
  CARD_TILES.map(([measure, title]) =>
    measure == 'download'
      ? cardTile(
          report,
          downloadMeasure(report, ['gzip', 'Download (gzip)']),
          'bytes',
          title,
        )
      : cardTile(report, timeChart(report, [measure]).measures[0], 'ms', title),
  ).join('');

// About how wide a lead is on the card, in pixels: its 20px text, its padding
// and border, and the gap after it. The leads share the 1,424px row with their
// label.
const leadWidth = (text: string): number => text.length * 10.5 + 42;
const LEADS_WIDTH = 1424 - 170;

// Every other workload in which TinyJoin is the fastest, by how far ahead it
// is, as many as fit on one row. Ties, and leads that round to 1.0×, count as
// fastest in the headline, but are not leads to list.
const cardWins = (report: Report): string => {
  let width = 0;
  return report.results
    .filter(({id}) => !CARD_TILES.some(([measure]) => measure == id))
    .map(({id, label}) => ({
      name: CARD_NAMES[id] ?? label,
      ...compare(report, timeChart(report, [id]).measures[0]),
    }))
    .filter(({leads, ratio}) => leads && ratio >= 1.05)
    .sort((a, b) => b.ratio - a.ratio)
    .filter(({name, ratio}) => {
      width += leadWidth(`${name} ${formatRatio(ratio)}`);
      return width <= LEADS_WIDTH;
    })
    .map(
      ({name, ratio}) =>
        `<li>${escapeHtml(name)} <b>${formatRatio(ratio)}</b></li>`,
    )
    .join('');
};

// The card's headline: how many workloads TinyJoin is the fastest in, and under
// it the next thing worth saying. Where it is the fastest in all of them, a
// line about its other placings would only repeat the first, so the second is
// about its size instead, when its download is also the smallest of the three.
const cardHeadline = (report: Report): string => {
  const [fastest, second, slowest] = placingCounts(report)[0];
  const total = report.results.length;
  return (
    (fastest == total
      ? `Fastest in all ${total} workloads.`
      : `Fastest in ${fastest} of ${total} workloads.`) +
    '<br /><em>' +
    (fastest == total
      ? isSmallest(report)
        ? 'And of course the tiniest.'
        : 'Second to none.'
      : slowest == 0
        ? 'Never the slowest.'
        : `Second in ${second}, and the slowest in ${slowest}.`) +
    '</em>'
  );
};

const renderCard = (
  report: Report,
  template: string,
  release: string,
): string => {
  const {sqlite, pglite} = report.engines;
  const {browserVersion, cpu} = report.environment;
  const values: {[name: string]: string} = {
    release: escapeHtml(release),
    headline: cardHeadline(report),
    engines: escapeHtml(
      `SQLite ${sqlite.engineVersion ?? sqlite.version} ` +
        `and PGlite ${pglite.version}`,
    ),
    tiles: cardTiles(report),
    wins: cardWins(report),
    method: escapeHtml(
      [
        'Same SQL on every engine',
        `Median of ${report.samples} runs`,
        `Chromium ${browserVersion.split('.')[0]}`,
        cpu,
        report.measuredAt.slice(0, 10),
      ].join(' · '),
    ),
  };
  return template.replaceAll(CARD_PLACEHOLDER, (_placeholder, name) => {
    if (values[name] == null) {
      throw new Error(`Unknown benchmark card placeholder: ${name}`);
    }
    return values[name];
  });
};

export type Benchmarks = {
  // Text placeholders to Markdown, leaving chart placeholders in place.
  renderText: (markdown: string) => string;
  // Chart placeholders to Markdown tables, for plain-text outputs.
  renderTables: (markdown: string) => string;
  // Rendered chart paragraphs to chart HTML, for TinyDocs replacers.
  chartReplacers: [RegExp, string][];
  // The benchmark card's template to HTML, labeled with a release.
  renderCard: (template: string, release: string) => string;
};

// The workloads the homepage draws side by side, one of each kind of work, each
// under a label short enough to sit beside its bars in half the page's width.
const HIGHLIGHTS: [id: string, label: string][] = [
  ['cold-open', 'Open a new database'],
  ['select-all', 'Read 10,000 rows'],
  ['select-pk', '1,000 reads by key'],
  ['update-pk', '1,000 updates by key'],
  ['insert-transaction', '10,000 inserts'],
  ['insert-autocommit', '1,000 commits'],
  ['join', '100 joins'],
];

const highlightsChart = (report: Report): Chart => ({
  unit: 'ms',
  measures: HIGHLIGHTS.map(([id, label]) => ({
    ...timeChart(report, [id]).measures[0],
    label,
  })),
});

const createBenchmarks = (report: Report): Benchmarks => {
  // The report's charts: its download sizes, the homepage's highlights, and
  // one for each workload group, named by the group.
  const charts = new Map<string, Chart>([
    ['download', downloadChart(report)],
    ['highlights', highlightsChart(report)],
  ]);
  for (const group of new Set(
    report.results.map(({group}) => group.toLowerCase()),
  )) {
    charts.set(group, timeChart(report, groupIds(report, group)));
  }
  return {
    renderText: (markdown) =>
      markdown.replaceAll(PLACEHOLDER, (placeholder, name) =>
        charts.has(name) ? placeholder : renderText(report, name),
      ),
    renderTables: (markdown) =>
      markdown.replaceAll(PLACEHOLDER, (placeholder, name) => {
        const chart = charts.get(name);
        return chart ? chartMarkdown(chart) : placeholder;
      }),
    chartReplacers: [...charts].map(([name, chart]) => [
      new RegExp(`<p>\\{\\{benchmarks\\.${name}\\}\\}</p>`, 'g'),
      // A replacement string, in which $ would otherwise be special.
      `<div class="charts">${chartHtml(report, chart)}</div>`.replaceAll(
        '$',
        '$$$$',
      ),
    ]),
    renderCard: (template, release) => renderCard(report, template, release),
  };
};

let benchmarks: Benchmarks | undefined;

export const getBenchmarks = (): Benchmarks =>
  (benchmarks ??= createBenchmarks(
    JSON.parse(readFileSync('site/data/benchmarks.json', 'utf8')),
  ));
