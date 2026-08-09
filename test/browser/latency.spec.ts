import {expect, test} from '@playwright/test';

test('measures warmed worker round-trip latency', async ({page}) => {
  await page.goto('/');
  await expect(page.getByTestId('state')).toHaveText('Ready');

  await page.evaluate(() => window.__tinygresTest!.benchmark(25));
  const samples = await page.evaluate(() =>
    window.__tinygresTest!.benchmark(200),
  );
  const sorted = [...samples].sort((left, right) => left - right);
  const report = {
    iterations: samples.length,
    p50: percentile(sorted, 0.5),
    p95: percentile(sorted, 0.95),
    p99: percentile(sorted, 0.99),
  };

  await test.info().attach('worker-latency.json', {
    body: JSON.stringify(report, null, 2),
    contentType: 'application/json',
  });
  console.log(`Tinygres worker latency: ${JSON.stringify(report)} ms`);

  expect(samples).toHaveLength(200);
  expect(samples.every((sample) => Number.isFinite(sample) && sample >= 0)).toBe(
    true,
  );
});

function percentile(sorted: number[], fraction: number): number {
  const index = Math.min(
    sorted.length - 1,
    Math.ceil(sorted.length * fraction) - 1,
  );
  return Number(sorted[index]!.toFixed(3));
}
