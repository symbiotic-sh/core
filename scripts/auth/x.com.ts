#!/usr/bin/env npx ts-node
/**
 * X (Twitter) login script.
 * Multi-step React SPA login flow.
 *
 * Input (stdin JSON): { domain, username, password, totp_code? }
 * Output (stdout JSON): { success, session?, error? }
 */

import { chromium } from 'playwright';

interface ScriptInput {
  domain: string;
  username: string;
  password: string;
  totp_code?: string;
}

interface ScriptOutput {
  success: boolean;
  session?: string;
  error?: string;
}

async function readStdin(): Promise<string> {
  return new Promise<string>((resolve) => {
    let data = '';
    process.stdin.on('data', (chunk) => (data += chunk));
    process.stdin.on('end', () => resolve(data));
  });
}

async function main() {
  const input: ScriptInput = JSON.parse(await readStdin());

  const browser = await chromium.launch({ headless: true });
  const context = await browser.newContext({
    userAgent: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36',
  });
  const page = await context.newPage();

  try {
    await page.goto('https://x.com/i/flow/login', {
      waitUntil: 'networkidle',
      timeout: 30_000,
    });

    // Step 1: Username entry
    // X uses autocomplete="username" for the first input
    const usernameField = page.locator('input[autocomplete="username"]').first();
    await usernameField.waitFor({ state: 'visible', timeout: 10_000 });
    await usernameField.fill(input.username);

    // Click "Next" button
    const nextButton = page.locator('[role="button"]:has-text("Next")').first();
    await nextButton.click();

    // Step 2: Wait for password field
    const passwordField = page.locator('input[type="password"]').first();
    await passwordField.waitFor({ state: 'visible', timeout: 10_000 });
    await passwordField.fill(input.password);

    // Click "Log in" button
    const loginButton = page.locator('[role="button"]:has-text("Log in")').first();
    await loginButton.click();
    await page.waitForLoadState('networkidle');

    // Check for error alerts
    const errorAlert = page.locator('[role="alert"]').first();
    if (await errorAlert.isVisible({ timeout: 3000 }).catch(() => false)) {
      const errorText = await errorAlert.textContent();
      const output: ScriptOutput = {
        success: false,
        error: errorText?.trim() || 'X login failed',
      };
      process.stdout.write(JSON.stringify(output));
      return;
    }

    // Handle TOTP if prompted
    if (input.totp_code) {
      const totpField = page.locator('input[autocomplete="one-time-code"]').first();
      if (await totpField.isVisible({ timeout: 5000 }).catch(() => false)) {
        await totpField.fill(input.totp_code);
        const confirmBtn = page.locator('[role="button"]:has-text("Confirm"), [role="button"]:has-text("Next")').first();
        await confirmBtn.click();
        await page.waitForLoadState('networkidle');
      }
    }

    // Capture session cookies
    const cookies = await context.cookies();
    const authToken = cookies.find(
      (c) => c.name === 'auth_token' && c.domain.includes('x.com')
    );
    const ct0 = cookies.find(
      (c) => c.name === 'ct0' && c.domain.includes('x.com')
    );

    const success = authToken !== undefined && ct0 !== undefined;
    const sessionStr = cookies
      .filter((c) => c.domain.includes('x.com') || c.domain.includes('twitter.com'))
      .map((c) => `${c.name}=${c.value}`)
      .join('; ');

    const output: ScriptOutput = {
      success,
      session: success ? sessionStr : undefined,
      error: success ? undefined : 'Missing auth_token or ct0 cookies',
    };
    process.stdout.write(JSON.stringify(output));
  } catch (err: any) {
    const output: ScriptOutput = {
      success: false,
      error: err.message,
    };
    process.stdout.write(JSON.stringify(output));
  } finally {
    await browser.close();
  }
}

main();
