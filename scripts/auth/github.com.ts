#!/usr/bin/env npx ts-node
/**
 * GitHub login script.
 * Known selectors for github.com authentication flow.
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
  const context = await browser.newContext();
  const page = await context.newPage();

  try {
    await page.goto('https://github.com/login', {
      waitUntil: 'networkidle',
      timeout: 30_000,
    });

    // GitHub uses known field IDs
    await page.fill('#login_field', input.username);
    await page.fill('#password', input.password);
    await page.click('input[value="Sign in"]');
    await page.waitForLoadState('networkidle');

    // Check for login errors
    const flashError = page.locator('.flash-error');
    if (await flashError.isVisible({ timeout: 2000 }).catch(() => false)) {
      const errorText = await flashError.textContent();
      const output: ScriptOutput = {
        success: false,
        error: errorText?.trim() || 'GitHub login failed',
      };
      process.stdout.write(JSON.stringify(output));
      return;
    }

    // Handle TOTP if the 2FA page appears
    if (input.totp_code) {
      const totpField = page.locator('#app_totp');
      if (await totpField.isVisible({ timeout: 5000 }).catch(() => false)) {
        await totpField.fill(input.totp_code);
        // GitHub auto-submits or has a verify button
        const verifyBtn = page.locator('button:has-text("Verify")');
        if (await verifyBtn.isVisible({ timeout: 1000 }).catch(() => false)) {
          await verifyBtn.click();
        }
        await page.waitForLoadState('networkidle');
      }
    }

    // Capture session cookies
    const cookies = await context.cookies();
    const sessionCookie = cookies.find(
      (c) => c.name === 'user_session' && c.domain.includes('github.com')
    );

    const success = sessionCookie !== undefined;
    const sessionStr = cookies
      .filter((c) => c.domain.includes('github.com'))
      .map((c) => `${c.name}=${c.value}`)
      .join('; ');

    const output: ScriptOutput = {
      success,
      session: success ? sessionStr : undefined,
      error: success ? undefined : 'No user_session cookie captured',
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
