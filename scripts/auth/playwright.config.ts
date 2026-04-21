import { defineConfig } from 'playwright/test';

export default defineConfig({
  timeout: 60_000,
  use: {
    headless: true,
    browserName: 'chromium',
    viewport: { width: 1280, height: 720 },
    ignoreHTTPSErrors: false,
  },
});
