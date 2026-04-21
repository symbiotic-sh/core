#!/usr/bin/env npx ts-node
/**
 * Generic Playwright login script.
 * Handles standard username/password forms including multi-step flows
 * (e.g. Google/X pattern where password field appears after username submit).
 *
 * Input (stdin JSON): { domain, username, password, totp_code? }
 * Output (stdout JSON): { success, session?, error? }
 */

import { chromium, Page } from 'playwright';

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

/** Common username/email field selectors. */
const USERNAME_SELECTORS = [
  'input[type="email"]',
  'input[name="username"]',
  'input[name="login"]',
  'input[name="email"]',
  'input[id="username"]',
  'input[id="login_field"]',
  'input[autocomplete="username"]',
].join(', ');

/** Common submit button selectors. */
const SUBMIT_SELECTORS = [
  'button[type="submit"]',
  'input[type="submit"]',
  'button:has-text("Sign in")',
  'button:has-text("Log in")',
  'button:has-text("Next")',
  'button:has-text("Continue")',
].join(', ');

/** Common TOTP input selectors. */
const TOTP_SELECTORS = [
  'input[name="totp"]',
  'input[name="otp"]',
  'input[name="code"]',
  'input[autocomplete="one-time-code"]',
  'input[id="app_totp"]',
].join(', ');

/** Common error indicators. */
const ERROR_SELECTORS = [
  '[role="alert"]',
  '.error',
  '.flash-error',
  '.alert-danger',
  '.error-message',
  '#error-message',
].join(', ');

/** Read JSON from stdin. */
async function readStdin(): Promise<string> {
  return new Promise<string>((resolve) => {
    let data = '';
    process.stdin.on('data', (chunk) => (data += chunk));
    process.stdin.on('end', () => resolve(data));
  });
}

/** Check for visible error messages on the page. */
async function detectError(page: Page): Promise<string | null> {
  try {
    const errorEl = page.locator(ERROR_SELECTORS).first();
    if (await errorEl.isVisible({ timeout: 1000 }).catch(() => false)) {
      return await errorEl.textContent() || 'Login failed (error element detected)';
    }
  } catch {
    // No error elements found
  }
  return null;
}

/** Detect if the password field is currently visible. */
async function isPasswordVisible(page: Page): Promise<boolean> {
  try {
    return await page.locator('input[type="password"]').first()
      .isVisible({ timeout: 1000 });
  } catch {
    return false;
  }
}

/** Extract session data: cookies + localStorage tokens. */
async function captureSession(page: Page, domain: string): Promise<string | undefined> {
  const context = page.context();
  const cookies = await context.cookies();
  const domainCookies = cookies
    .filter((c) => c.domain.includes(domain))
    .map((c) => `${c.name}=${c.value}`)
    .join('; ');

  // Try to capture localStorage tokens
  let localStorageTokens = '';
  try {
    localStorageTokens = await page.evaluate(() => {
      const tokens: Record<string, string> = {};
      for (let i = 0; i < localStorage.length; i++) {
        const key = localStorage.key(i);
        if (key && (key.includes('token') || key.includes('session') || key.includes('auth'))) {
          tokens[key] = localStorage.getItem(key) || '';
        }
      }
      return JSON.stringify(tokens);
    });
  } catch {
    // localStorage not accessible
  }

  const parts: string[] = [];
  if (domainCookies) parts.push(domainCookies);
  if (localStorageTokens && localStorageTokens !== '{}') parts.push(`localStorage:${localStorageTokens}`);

  return parts.length > 0 ? parts.join(' | ') : undefined;
}

async function main() {
  const input: ScriptInput = JSON.parse(await readStdin());

  const browser = await chromium.launch({ headless: true });
  const context = await browser.newContext();
  const page = await context.newPage();

  try {
    // Navigate to login page
    await page.goto(`https://${input.domain}/login`, {
      waitUntil: 'networkidle',
      timeout: 30_000,
    });

    // Fill username/email field
    const usernameField = page.locator(USERNAME_SELECTORS).first();
    await usernameField.fill(input.username);

    // Check if password field is visible (single-page) or hidden (multi-step)
    const pwVisible = await isPasswordVisible(page);

    if (pwVisible) {
      // Single-page login: fill password and submit
      await page.locator('input[type="password"]').first().fill(input.password);
      await page.locator(SUBMIT_SELECTORS).first().click();
    } else {
      // Multi-step login: submit username first, wait for password field
      await page.locator(SUBMIT_SELECTORS).first().click();
      await page.waitForSelector('input[type="password"]', { timeout: 10_000 });
      await page.locator('input[type="password"]').first().fill(input.password);
      await page.locator(SUBMIT_SELECTORS).first().click();
    }

    await page.waitForLoadState('networkidle');

    // Check for error after login attempt
    const loginError = await detectError(page);
    if (loginError) {
      const output: ScriptOutput = { success: false, error: loginError };
      process.stdout.write(JSON.stringify(output));
      return;
    }

    // Handle TOTP if required
    if (input.totp_code) {
      const totpField = page.locator(TOTP_SELECTORS).first();
      if (await totpField.isVisible({ timeout: 5000 }).catch(() => false)) {
        await totpField.fill(input.totp_code);
        const verifyButton = page.locator(
          'button[type="submit"], button:has-text("Verify"), button:has-text("Confirm")'
        ).first();
        await verifyButton.click();
        await page.waitForLoadState('networkidle');

        const totpError = await detectError(page);
        if (totpError) {
          const output: ScriptOutput = { success: false, error: totpError };
          process.stdout.write(JSON.stringify(output));
          return;
        }
      }
    }

    // Check for successful redirect away from /login
    const currentUrl = page.url();
    const redirectedAway = !currentUrl.includes('/login') && !currentUrl.includes('/signin');

    // Capture session
    const session = await captureSession(page, input.domain);

    const success = redirectedAway || (session !== undefined && session.length > 0);
    const output: ScriptOutput = {
      success,
      session: session || undefined,
      error: success ? undefined : 'No session captured and still on login page',
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
