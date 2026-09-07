import { chromium, expect } from '@playwright/test';
import { readFile } from 'node:fs/promises';
const url = 'http://127.0.0.1:8765';
const pat = (await readFile('.kvmhelm/secrets/cli.pat', 'utf8')).trim();
const devices = await (await fetch(url + '/api/v1/devices', { headers: { Authorization: `Bearer ${pat}` } })).json();
const ids = devices.filter(d => d.driver_id === 'simulator').slice(0, 2).map(d => d.device_id);
if (ids.length !== 2) throw Error('Two simulators required; no hardware fallback');
const browser = await chromium.launch({ headless: true });
const page = await browser.newPage();
let failure = false;
const pageErrors = [];
page.on('pageerror', e => pageErrors.push(e.message));
try {
  // Change this browser's view only; never overwrite the user's saved layout.
  await page.route('**/api/v1/preferences', async route => {
    const response = await route.fetch();
    if (route.request().method() !== 'GET') throw Error('Unexpected preferences write');
    const prefs = await response.json();
    await route.fulfill({ json: { ...prefs, selected: ids[0], visible: ids } });
  });
  await page.route('**/api/v1/sessions/*/frame', route => failure ? route.abort('failed') : route.continue());
  await page.goto(url + '/overview');
  await page.getByLabel('Personal Access Token').fill(pat);
  await page.getByRole('button', { name: 'Anmelden →' }).click();
  await expect(page.locator('.wall .screen img')).toHaveCount(2);
  await page.waitForTimeout(1000);
  await page.evaluate(() => {
    window.__frameWarnings = 0;
    new MutationObserver(() => { if (document.querySelector('.frame-error')) window.__frameWarnings++; })
      .observe(document.body, { subtree: true, childList: true });
  });
  failure = true;
  await page.waitForTimeout(600);
  failure = false;
  await page.waitForTimeout(1000);
  if (await page.evaluate(() => window.__frameWarnings)) throw Error('Transient fetch failure flashed a warning');
  failure = true;
  await expect(page.locator('.frame-error')).toHaveCount(2, { timeout: 5000 });
  await expect(page.locator('.wall .screen img')).toHaveCount(2);
  failure = false;
  await expect(page.locator('.frame-error')).toHaveCount(0);
  await page.screenshot({ path: '.kvmhelm/qa/frame-recovery.png', fullPage: true });
  await page.route('**/api/v1/tools/computer_control', route => route.fulfill({ json: {
    isError: true, content: [], structuredContent: { ok: false, frames: [], error: { code: 'CONTROL_BUSY', message: 'Control busy (injected test)' } },
  } }));
  await page.goto(url + '/computers');
  await expect(page.locator('.screen img')).toHaveCount(1);
  await page.getByRole('button', { name: 'Steuerung anfordern', exact: true }).click();
  await expect(page.locator('.screen [role="alert"]')).toHaveText('Control busy (injected test)');
  await page.waitForTimeout(1000);
  await expect(page.locator('.screen [role="alert"]')).toHaveText('Control busy (injected test)');
  await page.goto(url + '/settings'); // Unmount and close the observation sessions.
  if (pageErrors.length) throw Error(pageErrors.join('\n'));
  console.log('PASS: two tiles, no transient flash, persistent outage visible, last images retained, recovery clears frame warnings, control errors survive successful polls. No hardware inputs or preferences changed.');
} finally { await browser.close(); }
