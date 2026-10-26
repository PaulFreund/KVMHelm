import { referencePath, referenceRoot } from "./reference.mjs";
// The Vue bundle against isolated native and reference daemons.
// Only explicitly created simulators receive input; no hardware/capture access.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { chmod, copyFile, mkdtemp, readFile, rm } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve, sep } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { chromium, expect } from "@playwright/test";

const reference = process.env.KVMHELM_TEST_DAEMON === "typescript";
assert.ok(
  reference || process.env.KVMHELM_TEST_BINARY,
  "Set KVMHELM_TEST_BINARY",
);
const dir = await mkdtemp(join(tmpdir(), "kvmhelm-native-ui-"));
const executable = reference
  ? process.execPath
  : join(dir, process.platform === "win32" ? "kvmhelm.exe" : "kvmhelm");
const prefix = reference ? [referencePath("dist/server/cli.js")] : [];
const env = Object.fromEntries(
  Object.entries(process.env).filter(
    ([key]) => !key.toUpperCase().startsWith("KVMHELM_"),
  ),
);
delete env.HOME;
delete env.USERPROFILE;
if (!reference) env.PATH = "";
Object.assign(env, {
  KVMHELM_DATA: join(dir, "data"),
  KVMHELM_SECRETS: join(dir, "secrets"),
  KVMHELM_HOST: "invalid-host-unused",
  KVMHELM_PORT: "invalid-port-unused",
  KVMHELM_MAX_SESSIONS: "2",
  KVMHELM_REPLAY_MB: "0.25",
});
const options = {
  cwd: reference ? referenceRoot : dir,
  env,
  windowsHide: true,
};
const listener = createServer();
await new Promise((r) => listener.listen(0, "127.0.0.1", r));
const port = listener.address().port;
await new Promise((r) => listener.close(r));
const base = `http://127.0.0.1:${port}`;
let daemon,
  browser,
  pat,
  logs = "";
async function until(fn, label, timeout = 15000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    if (await fn()) return;
    await delay(30);
  }
  throw Error(label + " timed out; " + logs);
}
async function start() {
  logs = "";
  daemon = spawn(
    executable,
    [...prefix, "serve", "--host", "127.0.0.1", "--port", String(port)],
    { ...options, stdio: ["ignore", "ignore", "pipe"] },
  );
  daemon.stderr.on("data", (d) => {
    logs = (logs + d).slice(-8000);
  });
  await until(
    async () => {
      if (daemon.exitCode !== null || daemon.signalCode !== null)
        throw Error(logs);
      return fetch(base + "/readyz")
        .then((r) => r.ok)
        .catch(() => false);
    },
    "Daemon startup",
    45000,
  );
}
async function stop() {
  const child = daemon;
  daemon = undefined;
  if (child?.exitCode === null && child.signalCode === null) {
    const ended = new Promise((r) => child.once("exit", r));
    child.kill();
    await ended;
  }
}
async function request(
  path,
  method = "GET",
  body,
  client = "acceptance",
  token = pat,
) {
  const r = await fetch(base + "/api/v1" + path, {
    method,
    headers: {
      authorization: "Bearer " + token,
      "content-type": "application/json",
      "x-kvmhelm-client": client,
    },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(40000),
  });
  const value = await r.json();
  assert.equal(r.status, 200, path + ": " + JSON.stringify(value));
  return value;
}
async function tool(name, body, client) {
  const value = await request("/tools/" + name, "POST", body, client);
  assert.equal(value.isError, false, JSON.stringify(value.structuredContent));
  return value.structuredContent;
}
async function createDevice(page, name) {
  await page
    .getByRole("button", { name: "＋ Computer hinzufügen", exact: true })
    .first()
    .click();
  const form = page.locator(".modal");
  await form.getByLabel("Name", { exact: true }).fill(name);
  await form.getByLabel(/^Treiber/).selectOption("simulator");
  await form
    .getByLabel("Minimum Action Latency (ms)", { exact: true })
    .fill("0");
  await form.getByLabel(/^Medienprofil/).selectOption("warm");
  await form.getByLabel(/^Tastaturlayout/).selectOption("de");
  await form.getByLabel("Tags", { exact: true }).fill("fixture, local");
  await form.getByRole("button", { name: "Speichern", exact: true }).click();
  await expect(form).toBeHidden();
}
try {
  if (!reference) {
    await copyFile(resolve(process.env.KVMHELM_TEST_BINARY), executable);
    await chmod(executable, 0o700);
  }
  const bootstrap = spawnSync(executable, [...prefix, "token", "bootstrap"], {
    ...options,
    encoding: "utf8",
  });
  assert.equal(bootstrap.status, 0, bootstrap.stderr);
  pat = (await readFile(join(dir, "secrets", "cli.pat"), "utf8")).trim();
  await start();
  if (!reference) {
    const health = spawnSync(executable, ["healthcheck"], {
      ...options,
      env: { ...env, KVMHELM_URL: base },
      encoding: "utf8",
    });
    assert.equal(health.status, 0, health.stderr);
    assert.equal(JSON.parse(health.stdout).ok, true);
  }
  assert.deepEqual(await request("/devices"), []);
  browser = await chromium.launch({ headless: true, args: ["--mute-audio"] });
  const context = await browser.newContext({
    viewport: { width: 1440, height: 1100 },
  });
  const page = await context.newPage();
  let sleeping = !reference;
  if (!reference)
    await page.route("**/api/v1/tools/open_computer", async (route) => {
      if (!sleeping) return route.continue();
      if (route.request().postDataJSON().mode === "observe")
        return route.fulfill({
          json: {
            isError: true,
            content: [],
            structuredContent: {
              ok: false,
              frames: [],
              error: { code: "NO_SIGNAL", message: "Sleeping fixture" },
            },
          },
        });
      const response = await route.fetch();
      const body = await response.json();
      body.content = body.content.filter((item) => item.type !== "image");
      body.structuredContent.frames = [];
      body.structuredContent.video_error = {
        code: "NO_SIGNAL",
        message: "Sleeping fixture",
      };
      return route.fulfill({ response, json: body });
    });
  if (!reference)
    await page.route("**/api/v1/sessions/*/frame", (route) =>
      sleeping
        ? route.fulfill({
            status: 503,
            json: { error: { code: "NO_SIGNAL", message: "Sleeping fixture" } },
          })
        : route.continue(),
    );
  const errors = [],
    inputs = [];
  let streams = 0,
    client;
  page.on("pageerror", (e) => errors.push(e.message));
  page.on("request", (r) => {
    if (r.url().endsWith("/tools/computer")) {
      inputs.push(r.postDataJSON());
      client = r.headers()["x-kvmhelm-client"];
    }
    if (r.url().endsWith("/events")) streams++;
  });
  await page.goto(base + "/computers");
  await page.getByLabel("Personal Access Token").fill(pat);
  await page.getByRole("button", { name: "Anmelden →" }).click();
  await createDevice(page, "Desktop fixture");
  await page
    .getByRole("button", { name: "Desktop fixture", exact: true })
    .first()
    .click();
  if (!reference) {
    const acquire = page.getByRole("button", {
      name: "Steuerung übernehmen",
      exact: true,
    });
    await expect(acquire).toBeEnabled();
    await expect(page.locator(".empty-display")).toContainText(
      "Sleeping fixture",
    );
    const opened = page.waitForRequest(
      (r) =>
        r.url().endsWith("/tools/open_computer") &&
        r.postDataJSON().mode === "control",
    );
    await acquire.click();
    const opening = (await opened).postDataJSON();
    assert.equal(opening.wake, undefined);
    assert.ok(opening.request_id);
    const blank = page.getByRole("group", {
      name: "KVM-Bildschirm Desktop fixture",
      exact: true,
    });
    await expect(blank).toBeFocused();
    await expect(page.locator(".screen img")).toHaveCount(0);
    assert.equal(
      inputs.length,
      0,
      "acquiring control must not send a wake key",
    );
    const batchesBefore = (await request("/diagnostics")).metrics.batches;
    await page.route(
      "**/api/v1/tools/computer",
      async (route) => {
        const response = await route.fetch();
        const body = await response.json();
        body.isError = true;
        body.structuredContent.ok = false;
        body.structuredContent.error = {
          code: "EXECUTION_UNKNOWN",
          message: "Uncertain no-video input",
        };
        await route.fulfill({ response, json: body });
      },
      { times: 1 },
    );
    await page.keyboard.press("Enter");
    await expect
      .poll(async () => (await request("/diagnostics")).metrics.batches)
      .toBeGreaterThan(batchesBefore);
    await expect(
      page.getByRole("alert").filter({ hasText: "Uncertain no-video input" }),
    ).toBeVisible();
    await delay(150);
    assert.equal(inputs.length, 1, "uncertain input must not replay");
    await page.keyboard.press("ArrowRight");
    await expect
      .poll(async () => (await request("/diagnostics")).metrics.batches)
      .toBe(batchesBefore + 2);
    await expect(
      page.getByRole("alert").filter({ hasText: "Uncertain no-video input" }),
    ).toHaveCount(0);
    await blank.click({ position: { x: 100, y: 80 }, button: "right" });
    await until(
      () =>
        inputs.some((r) =>
          r.actions.some((a) => a.type === "click" && a.button === "right"),
        ),
      "Mouse without video",
    );
    await expect
      .poll(async () => (await request("/diagnostics")).metrics.batches)
      .toBeGreaterThan(batchesBefore + 2);
    assert.ok(
      inputs.every(
        (r) =>
          r.reference.frame_id === undefined && r.observation.mode === "none",
      ),
    );
    await expect(page.locator(".screen img")).toHaveCount(0);
    await page
      .getByRole("button", { name: "Steuerung freigeben", exact: true })
      .click();
    await expect(
      page.getByRole("button", { name: "Steuerung übernehmen", exact: true }),
    ).toBeEnabled();
    sleeping = false;
    inputs.length = 0;
    await page.unroute("**/api/v1/tools/open_computer");
    await page.unroute("**/api/v1/sessions/*/frame");
  }
  await expect(page.locator(".screen img")).toBeVisible();
  await createDevice(page, "Touch fixture");
  const devices = await request("/devices"),
    desktop = devices.find((d) => d.name === "Desktop fixture"),
    touch = devices.find((d) => d.name === "Touch fixture");
  assert.equal(desktop.keyboard_layout, "de");
  assert.deepEqual(desktop.tags, ["fixture", "local"]);
  const probe = await tool("open_computer", {
    computer_id: touch.device_id,
    mode: "observe",
    profile: "warm",
  });
  const limited = await request("/tools/open_computer", "POST", {
    computer_id: touch.device_id,
    mode: "observe",
    profile: "warm",
  });
  assert.equal(limited.structuredContent.error.code, "RESOURCE_LIMIT");
  await tool("close_computer", { session_id: probe.session.session_id });

  // A stale browser form must not overwrite a concurrent configuration edit.
  await page.getByRole("button", { name: "Bearbeiten", exact: true }).click();
  await request("/devices", "POST", {
    device: {
      device_id: desktop.device_id,
      revision: desktop.revision,
      name: "Concurrent fixture",
      driver_id: "simulator",
      transport: "simulator",
      media_profile: "warm",
      keyboard_layout: "de",
      minimum_action_latency_ms: 0,
    },
  });
  await page
    .locator(".modal")
    .getByLabel("Name", { exact: true })
    .fill("Stale fixture");
  const conflict = page.waitForResponse(
    (r) =>
      r.url().endsWith("/api/v1/devices") && r.request().method() === "POST",
  );
  await page
    .locator(".modal")
    .getByRole("button", { name: "Speichern", exact: true })
    .click();
  assert.equal((await conflict).status(), 409);
  await expect(page.locator(".modal")).toBeVisible();
  assert.equal(
    (await request("/devices")).find((d) => d.device_id === desktop.device_id)
      .name,
    "Concurrent fixture",
  );
  await page.getByRole("button", { name: "Dialog schließen" }).click();
  await expect(
    page
      .getByRole("button", { name: "Concurrent fixture", exact: true })
      .first(),
  ).toBeVisible();
  await page.getByRole("button", { name: "Bearbeiten", exact: true }).click();
  await page
    .locator(".modal")
    .getByLabel("Name", { exact: true })
    .fill("Desktop fixture");
  await page
    .locator(".modal")
    .getByRole("button", { name: "Speichern", exact: true })
    .click();
  await expect(page.locator(".modal")).toBeHidden();
  await expect(
    page.getByRole("group", {
      name: "KVM-Bildschirm Desktop fixture",
      exact: true,
    }),
  ).toBeVisible();
  const target = page.getByRole("group", {
    name: "KVM-Bildschirm Desktop fixture",
    exact: true,
  });
  if (!reference)
    await page.route(
      "**/api/v1/tools/computer_control",
      async (route) => {
        const args = route.request().postDataJSON();
        assert.equal(args.operation, "acquire");
        const closed = await request(
          "/tools/close_computer",
          "POST",
          { session_id: args.session_id },
          route.request().headers()["x-kvmhelm-client"],
        );
        assert.ok(
          closed.structuredContent.ok ||
            closed.structuredContent.error.code === "SESSION_LOST",
        );
        await route.fulfill({
          json: {
            isError: true,
            content: [],
            structuredContent: {
              ok: false,
              frames: [],
              error: {
                code: "SESSION_LOST",
                message: "Session closed by a configuration change",
              },
            },
          },
        });
      },
      { times: 1 },
    );
  await page
    .getByRole("button", { name: "Steuerung übernehmen", exact: true })
    .click();
  try {
    await expect(target).toBeFocused();
  } catch (error) {
    console.error(
      "Control acquisition state",
      JSON.stringify({
        alerts: await page.getByRole("alert").allTextContents(),
        toolbar: await page.locator(".control-toolbar").innerText(),
        sessions: await request("/sessions"),
      }),
    );
    throw error;
  }
  const batchesBeforeKeyboard = (await request("/diagnostics")).metrics.batches;
  await page.keyboard.press("a");
  await until(
    () =>
      inputs.some((r) =>
        r.actions.some((a) => a.type === "type" && a.text === "a"),
      ),
    "Keyboard dispatch",
  );
  await expect
    .poll(async () => (await request("/diagnostics")).metrics.batches)
    .toBe(batchesBeforeKeyboard + 1);
  await page.keyboard.press("Control+Alt+Escape");
  await expect(target).not.toBeFocused();
  const localCount = inputs.length;
  await page.getByRole("button", { name: "Text / Paste", exact: true }).click();
  const field = page.getByLabel("Text auf Ziel eingeben");
  await field.fill("line 1\nline 2");
  await delay(150);
  assert.equal(
    inputs.length,
    localCount,
    "editing the local preview cannot type on the target",
  );
  let fail = true;
  await page.route("**/tools/computer", (route) => {
    if (!fail) return route.continue();
    fail = false;
    return route.fulfill({
      json: {
        isError: true,
        content: [],
        structuredContent: {
          ok: false,
          frames: [],
          error: {
            code: "UNSUPPORTED_ACTION",
            message: "Synthetic input failure",
          },
        },
      },
    });
  });
  await page.getByRole("button", { name: "Senden", exact: true }).click();
  await expect(
    page.getByRole("alert").filter({ hasText: "Synthetic input failure" }),
  ).toBeVisible();
  await expect(field).toHaveValue("line 1\nline 2");
  await page.getByRole("button", { name: "Senden", exact: true }).click();
  await expect(field).toHaveValue("");
  await expect(target).toBeFocused();
  const pastedCount = inputs.length;
  await target.evaluate((el) => {
    const clipboardData = new DataTransfer();
    clipboardData.setData("text/plain", "Review ä€\n");
    el.dispatchEvent(
      new ClipboardEvent("paste", {
        bubbles: true,
        cancelable: true,
        clipboardData,
      }),
    );
  });
  await expect(field).toHaveValue("Review ä€\n");
  assert.equal(inputs.length, pastedCount);
  await field.fill("x".repeat(4097));
  await expect(
    page.getByRole("button", { name: "Senden", exact: true }),
  ).toBeDisabled();
  await field.fill("");
  await page.getByRole("button", { name: "Ctrl Alt Del", exact: true }).click();
  await until(
    () =>
      inputs.some((r) =>
        r.actions.some((a) => a.keys?.join(",") === "CTRL,ALT,DELETE"),
      ),
    "Special key dispatch",
  );
  const image = page.locator(".screen img");
  await image.dblclick({ position: { x: 80, y: 80 }, delay: 60 });
  await until(
    () => inputs.some((r) => r.actions.some((a) => a.type === "double_click")),
    "Double click",
  );
  assert.equal(
    inputs
      .flatMap((r) => r.actions)
      .filter((a) => a.type === "click" && a.button === "left").length,
    0,
  );
  await image.click({ position: { x: 100, y: 90 }, button: "right" });
  await until(
    () =>
      inputs.some((r) =>
        r.actions.some((a) => a.type === "click" && a.button === "right"),
      ),
    "Right click",
  );
  await image.hover({ position: { x: 120, y: 90 } });
  await page.mouse.wheel(0, 120);
  await until(
    () => inputs.some((r) => r.actions.some((a) => a.type === "scroll")),
    "Wheel dispatch",
  );
  await page.getByRole("button", { name: "Originalgröße 1:1" }).click();
  await expect(target).toHaveClass(/native-size/);
  await page.getByRole("button", { name: "An Fenster anpassen" }).click();
  await page.getByRole("button", { name: "Vollbild ↗", exact: true }).click();
  await expect
    .poll(() => page.evaluate(() => !!document.fullscreenElement))
    .toBe(true);
  await page
    .getByRole("button", { name: "Vollbild verlassen", exact: true })
    .click();

  // Fill the real daemon journal using this browser's client, then exercise
  // the existing UI's rotation before exhaustion. No input is replayed.
  const session = (await request("/sessions", "GET", undefined, client))[0];
  for (let n = 0; n < session.journal_remaining - 9; n++)
    await tool(
      "computer_control",
      {
        session_id: session.session_id,
        request_id: "fill-" + n,
        operation: "renew",
      },
      client,
    );
  await target.focus();
  await page.keyboard.press("b");
  await until(
    () => inputs.some((r) => r.actions.some((a) => a.text === "b")),
    "First journal boundary action",
  );
  await expect
    .poll(
      async () =>
        (await request("/sessions", "GET", undefined, client))[0]
          ?.journal_remaining,
    )
    .toBeLessThanOrEqual(8);
  await page.keyboard.press("c");
  await until(
    () =>
      inputs.some(
        (r) =>
          r.session_id !== session.session_id &&
          r.actions.some((a) => a.text === "c"),
      ),
    "Journal rotation",
  );
  const rotated = await request("/sessions", "GET", undefined, client);
  assert.equal(rotated.length, 1);
  assert.notEqual(rotated[0].session_id, session.session_id);
  const streamsBefore = streams;
  await stop();
  await start();
  await expect(
    page.getByRole("button", { name: "Steuerung übernehmen", exact: true }),
  ).toBeVisible({ timeout: 20000 });
  await expect(page.locator(".screen img")).toBeVisible();
  await until(
    async () =>
      (await request("/diagnostics")).health.sessions === 1 &&
      streams > streamsBefore,
    "Observation/SSE recovery after restart",
    20000,
  );
  assert.equal(
    (await request("/devices")).find((d) => d.device_id === desktop.device_id)
      .lease,
    null,
    "restart recovery cannot reacquire control",
  );
  await page.getByRole("button", { name: /Einstellungen/ }).click();
  await until(
    async () => (await request("/diagnostics")).health.sessions === 0,
    "Desktop session cleanup",
  );

  // Token management and confirmed device deletion through the real Vue UI.
  const tokens = page.locator(".detail-card").filter({
    has: page.getByRole("heading", {
      name: "Personal Access Tokens",
      exact: true,
    }),
  });
  await tokens.getByLabel("Name", { exact: true }).fill("UI reader");
  await tokens
    .getByLabel("Geräte-IDs (Komma getrennt, * für alle)", { exact: true })
    .fill(desktop.device_id);
  for (const box of await tokens.locator(".scope-grid input").all())
    await box.uncheck();
  await tokens.getByLabel("devices:read", { exact: true }).check();
  await tokens.getByLabel("video:read", { exact: true }).check();
  await tokens
    .getByRole("button", { name: "Token erstellen", exact: true })
    .click();
  const reader = await tokens.getByLabel("Neuer Token").inputValue();
  assert.deepEqual(
    (await request("/devices", "GET", undefined, "reader", reader)).map(
      (d) => d.device_id,
    ),
    [desktop.device_id],
  );
  await tokens.getByRole("button", { name: "Ausblenden", exact: true }).click();
  await tokens
    .locator(".manage-row")
    .filter({ hasText: "UI reader" })
    .getByRole("button", { name: "Widerrufen", exact: true })
    .click();
  await until(
    async () =>
      (
        await fetch(base + "/api/v1/me", {
          headers: { authorization: "Bearer " + reader },
        })
      ).status === 401,
    "UI revokes token",
  );
  await page
    .getByRole("button", { name: "Diagnose laden", exact: true })
    .click();
  await expect(page.locator(".detail-card pre")).toContainText('"devices"');
  const touchRow = page
    .locator(".manage-row")
    .filter({ hasText: "Touch fixture" });
  page.once("dialog", (d) => d.accept());
  await touchRow
    .getByRole("button", { name: "Entfernen", exact: true })
    .click();
  await until(
    async () => (await request("/devices")).length === 1,
    "Confirmed deletion",
  );

  const mobile = await browser.newContext({
    viewport: { width: 390, height: 844 },
    isMobile: true,
    hasTouch: true,
  });
  const phone = await mobile.newPage();
  const touches = [];
  phone.on("pageerror", (e) => errors.push(e.message));
  phone.on("request", (r) => {
    if (r.url().endsWith("/tools/computer")) touches.push(r.postDataJSON());
  });
  await phone.addInitScript(
    (value) => localStorage.setItem("kvmhelm.access-token", value),
    pat,
  );
  await phone.goto(base + "/computers");
  await expect(phone.locator(".screen img")).toBeVisible();
  await phone
    .getByRole("button", { name: "Steuerung übernehmen", exact: true })
    .click();
  await phone.locator(".screen img").tap();
  await until(
    () => touches.some((r) => r.actions.some((a) => a.type === "click")),
    "Touch tap",
  );
  const box = await phone.locator(".screen img").boundingBox();
  const cdp = await mobile.newCDPSession(phone),
    point = { x: box.x + box.width / 2, y: box.y + box.height / 2 };
  await cdp.send("Input.dispatchTouchEvent", {
    type: "touchStart",
    touchPoints: [point],
  });
  await cdp.send("Input.dispatchTouchEvent", {
    type: "touchMove",
    touchPoints: [{ x: point.x + 30, y: point.y + 20 }],
  });
  await cdp.send("Input.dispatchTouchEvent", {
    type: "touchEnd",
    touchPoints: [],
  });
  await until(
    () => touches.some((r) => r.actions.some((a) => a.type === "drag")),
    "Touch drag",
  );
  await phone.goto(base + "/settings");
  await until(
    async () => (await request("/diagnostics")).health.sessions === 0,
    "Mobile session cleanup",
  );
  await mobile.close();
  assert.deepEqual(errors, []);
  if (!reference)
    for (const invalid of ["-1", "NaN", "1e999"]) {
      const rejected = spawnSync(
        executable,
        ["serve", "--host", "127.0.0.1", "--port", "0"],
        {
          ...options,
          env: { ...env, KVMHELM_REPLAY_MB: invalid },
          encoding: "utf8",
          timeout: 10000,
        },
      );
      assert.equal(
        rejected.status,
        1,
        "invalid replay configuration must fail before starting a Core",
      );
    }
  console.log(
    `${reference ? "Reference" : "Native"} Vue acceptance: environment/flag precedence, fractional replay budget, session limit, create/edit/CAS/delete, keyboard/focus, safe paste preview, special keys, mouse/wheel, sizing/fullscreen, journal rotation, restart/SSE without control reacquisition, scoped token creation/revoke, diagnostics and mobile touch/drag passed${reference ? "" : "; no-video keyboard/mouse, uncertain-input recovery without replay and known session-loss recovery passed"}`,
  );
} catch (error) {
  console.error("Vue acceptance failed:", error);
  throw error;
} finally {
  await browser?.close();
  await stop();
  assert.ok(resolve(dir).startsWith(resolve(tmpdir()) + sep));
  await rm(dir, {
    recursive: true,
    force: true,
    maxRetries: 40,
    retryDelay: 100,
  });
}
