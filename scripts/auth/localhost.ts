#!/usr/bin/env npx ts-node
/**
 * Localhost mock login script.
 * Authenticates against the test-server mock login server.
 *
 * Input (stdin JSON): { domain, username, password, totp_code? }
 * Output (stdout JSON): { success, session?, error? }
 *
 * The domain field should be "localhost" or "localhost:PORT".
 * If no port is specified, defaults to 3847.
 * The server must be running on http://localhost:PORT/login.
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

/** Extract port from domain string (e.g., "localhost:3848" -> "3848"). */
function parsePort(domain: string): string {
  const colonIndex = domain.indexOf(':');
  if (colonIndex !== -1) {
    return domain.substring(colonIndex + 1);
  }
  return '3847';
}

async function main() {
  const input: ScriptInput = JSON.parse(await readStdin());
  const port = parsePort(input.domain);
  const baseUrl = `http://localhost:${port}`;

  const browser = await chromium.launch({ headless: true });
  const context = await browser.newContext();
  const page = await context.newPage();

  try {
    await page.goto(`${baseUrl}/login`, {
      waitUntil: 'networkidle',
      timeout: 15_000,
    });

    // Fill the login form
    await page.fill('#username', input.username);
    await page.fill('#password', input.password);

    // Submit the form
    await page.click('button[type="submit"]');
    await page.waitForLoadState('networkidle');

    // Check for error messages
    const errorEl = page.locator('.error, [role="alert"]').first();
    if (await errorEl.isVisible({ timeout: 2000 }).catch(() => false)) {
      const errorText = await errorEl.textContent();
      const output: ScriptOutput = {
        success: false,
        error: errorText?.trim() || 'Login failed',
      };
      process.stdout.write(JSON.stringify(output));
      return;
    }

    // Wait for redirect to dashboard
    await page.waitForURL('**/dashboard', { timeout: 10_000 }).catch(() => {
      // If no redirect happened, we might already be there
    });

    // Verify we landed on the dashboard
    const currentUrl = page.url();
    const onDashboard = currentUrl.includes('/dashboard');

    // Capture session_token cookie
    const cookies = await context.cookies();
    const sessionCookie = cookies.find(
      (c) => c.name === 'session_token' && c.domain.includes('localhost')
    );

    const success = onDashboard && sessionCookie !== undefined;
    const sessionStr = sessionCookie
      ? `${sessionCookie.name}=${sessionCookie.value}`
      : undefined;

    const output: ScriptOutput = {
      success,
      session: success ? sessionStr : undefined,
      error: success ? undefined : 'No session_token cookie captured or not on dashboard',
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
