// Captures each card that the docs build publishes, docs/benchmark-card.html
// and docs/site-card.html, as a 1600x900 PNG, serving it and its fonts and
// logo from the committed docs. Each image is written beside the others in
// site/extras, which the docs build publishes beside the cards. The benchmarks
// guide shares the benchmark card as its Open Graph image, and every other
// page shares the site card.
//
// Each image is stamped with a hash of the card it was captured from, so that
// the docs check can tell when a newly built card needs capturing again. The
// check imports the stamp from here, so Playwright is only loaded to capture.
import {createHash} from 'node:crypto';
import {readFile, writeFile} from 'node:fs/promises';
import {resolve} from 'node:path';
import {fileURLToPath} from 'node:url';
import {crc32} from 'node:zlib';

const root = resolve(fileURLToPath(new URL('..', import.meta.url)));
const docs = resolve(root, 'docs');
const origin = 'http://tinyjoin.test';

export const CARDS = ['benchmark-card', 'site-card'];

export const cardStamp = (card, html) =>
  `${card}.html SHA-256 ` + createHash('sha256').update(html).digest('hex');

// A PNG with a comment in a tEXt chunk, inserted before the chunk that ends it.
const withComment = (png, comment) => {
  const data = Buffer.from(`Comment\0${comment}`, 'latin1');
  const chunk = Buffer.alloc(data.length + 12);
  chunk.writeUInt32BE(data.length);
  chunk.write('tEXt', 4, 'latin1');
  data.copy(chunk, 8);
  chunk.writeUInt32BE(crc32(chunk.subarray(4, -4)), data.length + 8);
  return Buffer.concat([png.subarray(0, -12), chunk, png.subarray(-12)]);
};

const captureCards = async () => {
  const {chromium} = await import('@playwright/test');
  let browser;
  try {
    browser = await chromium.launch({headless: true});
    const page = await browser.newPage({viewport: {width: 1600, height: 900}});
    await page.route(`${origin}/**`, (route) =>
      route.fulfill({
        path: resolve(docs, '.' + new URL(route.request().url()).pathname),
      }),
    );
    for (const card of CARDS) {
      const html = await readFile(resolve(docs, `${card}.html`), 'utf8');
      const output = resolve(root, `site/extras/${card}.png`);
      await page.goto(`${origin}/${card}.html`);
      await page.evaluate(() => document.fonts.ready);
      await writeFile(
        output,
        withComment(await page.screenshot(), cardStamp(card, html)),
      );
      console.log(`Wrote the ${card.replace('-', ' ')} to ${output}`);
    }
  } finally {
    await browser?.close();
  }
};

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  await captureCards();
}
