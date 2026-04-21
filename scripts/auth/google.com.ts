#!/usr/bin/env npx ts-node
/**
 * Google login script.
 * Multi-step login flow: email -> password -> optional 2FA.
 *
 * Google's login uses separate pages for email and password entry.
 * Bot detection is aggressive — this script uses realistic timing
 * and viewport settings but does NOT attempt stealth or evasion.
 * If Google blocks the login, the script reports it clearly.
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

async function readStdin(): Promise<string> {
  return new Promise<string>((resolve) => {
    let data = '';
    process.stdin.on('data', (chunk) => (data += chunk));
    process.stdin.on('end', () => resolve(data));
  });
}

/** Random delay between min and max milliseconds (inclusive). */
function randomDelay(min: number, max: number): number {
  return Math.floor(Math.random() * (max - min + 1)) + min;
}

/** Wait a random amount of time to simulate human pacing. */
async function humanPause(page: Page, min = 500, max = 2000): Promise<void> {
  await page.waitForTimeout(randomDelay(min, max));
}

/** Detect Google-specific error messages on the current page. */
async function detectGoogleError(page: Page): Promise<string | null> {
  // Google shows errors in various containers depending on the step
  const errorSelectors = [
    // Wrong password / invalid credentials
    'div[jsname="B34EJ"]',
    // General error alerts
    '[role="alert"]',
    // "Couldn't find your Google Account" / "Wrong password"
    'div.o6cuMc',
    // Error message container used in newer flows
    'div[class*="LXRPh"]',
    // Fallback: any visible error-like text
    'span[jsname="h9d3hd"]',
  ];

  for (const selector of errorSelectors) {
    try {
      const el = page.locator(selector).first();
      if (await el.isVisible({ timeout: 1000 }).catch(() => false)) {
        const text = await el.textContent();
        if (text && text.trim().length > 0) {
          return text.trim();
        }
      }
    } catch {
      // Element not found, try next
    }
  }

  return null;
}

/** Detect if Google is showing a CAPTCHA or bot challenge. */
async function detectCaptcha(page: Page): Promise<boolean> {
  const captchaIndicators = [
    // reCAPTCHA iframe
    'iframe[src*="recaptcha"]',
    'iframe[src*="captcha"]',
    // "Verify you're not a robot"
    'text="Verify you\'re not a robot"',
    'text="verify that you\'re not a robot"',
  ];

  for (const selector of captchaIndicators) {
    try {
      const el = page.locator(selector).first();
      if (await el.isVisible({ timeout: 1000 }).catch(() => false)) {
        return true;
      }
    } catch {
      // Not found
    }
  }

  return false;
}

/** Detect if Google is showing a "verify it's you" or phone verification prompt. */
async function detectVerificationChallenge(page: Page): Promise<string | null> {
  const challengePatterns = [
    { selector: 'text="Verify it\'s you"', message: 'Google is requesting identity verification (Verify it\'s you). Cannot automate this step.' },
    { selector: 'text="Confirm your recovery email"', message: 'Google is requesting recovery email confirmation. Cannot automate this step.' },
    { selector: 'text="Confirm your recovery phone"', message: 'Google is requesting recovery phone confirmation. Cannot automate this step.' },
    { selector: 'text="Get a verification code"', message: 'Google is requesting SMS/phone verification. Cannot automate this step.' },
    { selector: 'text="This device isn\'t recognized"', message: 'Google flagged this device as unrecognized. Cannot automate this step.' },
    { selector: 'text="Suspicious activity detected"', message: 'Google detected suspicious activity on this account. Login blocked.' },
    { selector: 'text="Your account has been disabled"', message: 'Google account is disabled.' },
    { selector: 'text="This account has been disabled"', message: 'Google account is disabled.' },
    { selector: 'text="Choose how you want to sign in"', message: 'Google is requesting an alternative sign-in method. Cannot automate this step.' },
  ];

  for (const { selector, message } of challengePatterns) {
    try {
      const el = page.locator(selector).first();
      if (await el.isVisible({ timeout: 1000 }).catch(() => false)) {
        return message;
      }
    } catch {
      // Not found
    }
  }

  return null;
}

/** Check if we successfully landed on a Google-authenticated page. */
async function isLoggedIn(page: Page): Promise<boolean> {
  const url = page.url();
  const successUrls = [
    'myaccount.google.com',
    'accounts.google.com/Default',
    'accounts.google.com/SignOutOptions',
    'mail.google.com',
    'drive.google.com',
    'google.com/?',
  ];
  return successUrls.some((s) => url.includes(s));
}

/** Capture Google session cookies and localStorage tokens. */
async function captureSession(page: Page): Promise<string | undefined> {
  const context = page.context();
  const cookies = await context.cookies();

  // Key Google session cookies
  const importantCookies = ['SID', 'HSID', 'SSID', 'APISID', 'SAPISID', 'NID', 'LSID', '__Secure-1PSID', '__Secure-3PSID'];
  const googleCookies = cookies.filter(
    (c) => c.domain.includes('google.com') || c.domain.includes('.google.com')
  );

  // Check that at least one critical session cookie is present
  const hasCriticalCookie = googleCookies.some((c) => importantCookies.includes(c.name));

  const cookieStr = googleCookies
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
    // localStorage may not be accessible on accounts.google.com
  }

  const parts: string[] = [];
  if (cookieStr) parts.push(cookieStr);
  if (localStorageTokens && localStorageTokens !== '{}') parts.push(`localStorage:${localStorageTokens}`);

  if (!hasCriticalCookie && parts.length === 0) {
    return undefined;
  }

  return parts.length > 0 ? parts.join(' | ') : undefined;
}

async function main() {
  const input: ScriptInput = JSON.parse(await readStdin());

  const browser = await chromium.launch({ headless: true });
  const context = await browser.newContext({
    // Realistic viewport and user agent to reduce bot detection triggers
    viewport: { width: 1280, height: 800 },
    userAgent:
      'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36',
    locale: 'en-US',
    timezoneId: 'America/New_York',
  });
  const page = await context.newPage();

  try {
    // ---------------------------------------------------------------
    // Step 1: Navigate to Google sign-in page
    // ---------------------------------------------------------------
    await page.goto('https://accounts.google.com/signin', {
      waitUntil: 'networkidle',
      timeout: 30_000,
    });

    // Check for CAPTCHA immediately (Google sometimes shows it before any input)
    if (await detectCaptcha(page)) {
      const output: ScriptOutput = {
        success: false,
        error: 'Google is showing a CAPTCHA challenge. Cannot automate CAPTCHA solving.',
      };
      process.stdout.write(JSON.stringify(output));
      return;
    }

    // ---------------------------------------------------------------
    // Step 2: Enter email/identifier
    // ---------------------------------------------------------------
    // Google uses input[type="email"] or #identifierId for the email field
    const emailField = page.locator('input[type="email"], #identifierId').first();
    await emailField.waitFor({ state: 'visible', timeout: 10_000 });
    await humanPause(page, 300, 800);

    // Type with a delay to simulate human typing speed
    await emailField.click();
    await page.type('input[type="email"], #identifierId', input.username, {
      delay: randomDelay(30, 80),
    });

    await humanPause(page, 300, 600);

    // ---------------------------------------------------------------
    // Step 3: Click "Next" to proceed to the password page
    // ---------------------------------------------------------------
    const nextButton = page.locator('#identifierNext button, #identifierNext').first();
    await nextButton.click();

    // Wait for either the password page or an error
    await humanPause(page, 1000, 2000);

    // Check for errors after email entry (e.g., "Couldn't find your Google Account")
    const emailError = await detectGoogleError(page);
    if (emailError) {
      const output: ScriptOutput = {
        success: false,
        error: `Email step failed: ${emailError}`,
      };
      process.stdout.write(JSON.stringify(output));
      return;
    }

    // Check for CAPTCHA after email entry
    if (await detectCaptcha(page)) {
      const output: ScriptOutput = {
        success: false,
        error: 'Google is showing a CAPTCHA challenge after email entry. Cannot automate CAPTCHA solving.',
      };
      process.stdout.write(JSON.stringify(output));
      return;
    }

    // ---------------------------------------------------------------
    // Step 4: Wait for the password page to load
    // ---------------------------------------------------------------
    // Google loads password in a separate view/page (not just showing a hidden field)
    const passwordField = page.locator('input[type="password"], input[name="Passwd"]').first();
    await passwordField.waitFor({ state: 'visible', timeout: 15_000 });

    await humanPause(page, 400, 1000);

    // ---------------------------------------------------------------
    // Step 5: Enter password
    // ---------------------------------------------------------------
    await passwordField.click();
    await page.type('input[type="password"], input[name="Passwd"]', input.password, {
      delay: randomDelay(30, 80),
    });

    await humanPause(page, 300, 600);

    // ---------------------------------------------------------------
    // Step 6: Click "Next" to submit the password
    // ---------------------------------------------------------------
    const passwordNext = page.locator('#passwordNext button, #passwordNext').first();
    await passwordNext.click();

    // Wait for navigation or error
    await page.waitForLoadState('networkidle', { timeout: 15_000 }).catch(() => {
      // networkidle may not fire if Google uses long-polling; continue anyway
    });
    await humanPause(page, 1500, 3000);

    // Check for wrong password error
    const passwordError = await detectGoogleError(page);
    if (passwordError) {
      const output: ScriptOutput = {
        success: false,
        error: `Password step failed: ${passwordError}`,
      };
      process.stdout.write(JSON.stringify(output));
      return;
    }

    // ---------------------------------------------------------------
    // Step 7: Handle potential challenges
    // ---------------------------------------------------------------

    // 7a: Check for verification challenges (phone, recovery email, etc.)
    const challenge = await detectVerificationChallenge(page);
    if (challenge) {
      const output: ScriptOutput = {
        success: false,
        error: challenge,
      };
      process.stdout.write(JSON.stringify(output));
      return;
    }

    // 7b: Check for CAPTCHA
    if (await detectCaptcha(page)) {
      const output: ScriptOutput = {
        success: false,
        error: 'Google is showing a CAPTCHA challenge after password entry. Cannot automate CAPTCHA solving.',
      };
      process.stdout.write(JSON.stringify(output));
      return;
    }

    // 7c: Handle TOTP 2FA if prompted and code is available
    if (input.totp_code) {
      // Google TOTP field: input[type="tel"] in the 2-step verification page,
      // or input[name="totpPin"], or input[id="totpPin"]
      const totpSelectors = [
        'input[name="totpPin"]',
        'input[id="totpPin"]',
        'input[type="tel"]',
        'input[autocomplete="one-time-code"]',
      ];

      let totpFilled = false;
      for (const selector of totpSelectors) {
        const totpField = page.locator(selector).first();
        if (await totpField.isVisible({ timeout: 5000 }).catch(() => false)) {
          await humanPause(page, 300, 600);
          await totpField.click();
          await page.type(selector, input.totp_code, {
            delay: randomDelay(40, 90),
          });

          await humanPause(page, 300, 500);

          // Click "Next" or "Verify" button on the TOTP page
          const totpNext = page.locator('#totpNext button, #totpNext, button:has-text("Next"), button:has-text("Verify")').first();
          if (await totpNext.isVisible({ timeout: 2000 }).catch(() => false)) {
            await totpNext.click();
          }

          await page.waitForLoadState('networkidle', { timeout: 10_000 }).catch(() => {});
          await humanPause(page, 1000, 2000);

          // Check for TOTP errors
          const totpError = await detectGoogleError(page);
          if (totpError) {
            const output: ScriptOutput = {
              success: false,
              error: `TOTP verification failed: ${totpError}`,
            };
            process.stdout.write(JSON.stringify(output));
            return;
          }

          totpFilled = true;
          break;
        }
      }

      if (!totpFilled) {
        // TOTP code was provided but no TOTP prompt appeared.
        // This is not necessarily an error — Google may not have prompted for 2FA.
      }
    } else {
      // No TOTP code provided. Check if Google is asking for 2FA.
      const totpPrompt = page.locator('input[name="totpPin"], input[id="totpPin"]').first();
      if (await totpPrompt.isVisible({ timeout: 3000 }).catch(() => false)) {
        const output: ScriptOutput = {
          success: false,
          error: 'Google is requesting a TOTP 2FA code, but no totp_code was provided. Configure TOTP secret in the credential vault.',
        };
        process.stdout.write(JSON.stringify(output));
        return;
      }
    }

    // ---------------------------------------------------------------
    // Step 8: Verify successful login
    // ---------------------------------------------------------------
    // Give Google a moment to redirect after 2FA
    await humanPause(page, 1000, 2000);

    const loggedIn = await isLoggedIn(page);
    const session = await captureSession(page);

    // Determine success: we're on a Google authenticated page OR we have session cookies
    const success = loggedIn || (session !== undefined && session.length > 0);

    if (!success) {
      // One final check for any challenges we may have missed
      const finalChallenge = await detectVerificationChallenge(page);
      if (finalChallenge) {
        const output: ScriptOutput = {
          success: false,
          error: finalChallenge,
        };
        process.stdout.write(JSON.stringify(output));
        return;
      }

      const output: ScriptOutput = {
        success: false,
        error: `Login did not complete. Final URL: ${page.url()}`,
      };
      process.stdout.write(JSON.stringify(output));
      return;
    }

    const output: ScriptOutput = {
      success: true,
      session,
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
