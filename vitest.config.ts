import {defineConfig} from 'vitest/config';

export default defineConfig({
  test: {
    coverage: {enabled: false},
    include: ['test/**/*.test.ts'],
  },
});
