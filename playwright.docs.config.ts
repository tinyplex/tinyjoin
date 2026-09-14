import {defineConfig, devices} from '@playwright/test';
import {tmpdir} from 'node:os';
import {resolve} from 'node:path';

const port = 4178;

export default defineConfig({
  testDir: './test/site',
  outputDir: resolve(tmpdir(), 'tinyjoin-docs-playwright-results'),
  fullyParallel: true,
  forbidOnly: Boolean(process.env.CI),
  retries: process.env.CI ? 2 : 0,
  reporter: process.env.CI ? 'github' : 'list',
  use: {
    baseURL: `http://127.0.0.1:${port}`,
    trace: 'on-first-retry',
  },
  projects: [{name: 'chromium', use: {...devices['Desktop Chrome']}}],
  webServer: {
    command: `npm run test:docs:serve -- --host 127.0.0.1 --port ${port} --strictPort`,
    url: `http://127.0.0.1:${port}`,
    reuseExistingServer: !process.env.CI,
    timeout: 30_000,
  },
});
