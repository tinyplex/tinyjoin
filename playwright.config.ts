import {defineConfig, devices} from '@playwright/test';
import {tmpdir} from 'node:os';
import {resolve} from 'node:path';

const port = Number(process.env.TINYJOIN_BROWSER_PORT ?? 4173);

export default defineConfig({
  testDir: './test/browser',
  outputDir: resolve(tmpdir(), 'tinyjoin-playwright-results'),
  fullyParallel: true,
  forbidOnly: Boolean(process.env.CI),
  retries: process.env.CI ? 2 : 0,
  reporter: process.env.CI ? 'github' : 'list',
  use: {
    baseURL: `http://127.0.0.1:${port}`,
    trace: 'on-first-retry',
  },
  projects: [
    {
      name: 'chromium',
      use: {...devices['Desktop Chrome']},
    },
  ],
  webServer: {
    command: `npm run test:browser:serve -- --host 127.0.0.1 --port ${port} --strictPort`,
    url: `http://127.0.0.1:${port}`,
    // A docs preview may already occupy this port. Never run runtime tests
    // against a different application just because it answers HTTP requests.
    reuseExistingServer: false,
    timeout: 120_000,
  },
});
