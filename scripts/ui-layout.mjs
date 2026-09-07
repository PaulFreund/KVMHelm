import { chromium, expect } from "@playwright/test";
import { spawn, spawnSync } from "node:child_process";
import { mkdtemp, readFile, rm, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve, sep } from "node:path";
const dir = await mkdtemp(join(tmpdir(), "kvmhelm-ui-"));
const args = ["--data", join(dir, "data"), "--secrets", join(dir, "secrets")];
const patFile = join(dir, "secrets", "cli.pat");
const bootstrap = spawnSync(
  process.execPath,
  ["dist/server/cli.js", "token", "bootstrap", ...args],
  { encoding: "utf8" },
);
if (bootstrap.status !== 0) throw Error(bootstrap.stderr);
const pat = (await readFile(patFile, "utf8")).trim();
const url = "http://127.0.0.1:18877";
let daemon, browser;
async function start() {
  daemon = spawn(
    process.execPath,
    ["dist/server/cli.js", "serve", "--port", "18877", ...args],
    { stdio: "ignore" },
  );
  for (let n = 0; n < 100; n++) {
    if (
      await fetch(url + "/healthz")
        .then((r) => r.ok)
        .catch(() => false)
    )
      return;
    await new Promise((r) => setTimeout(r, 100));
  }
  throw Error("Test daemon did not start");
}
async function stop() {
  if (!daemon || daemon.exitCode !== null) return;
  const stopped = new Promise((r) => daemon.once("exit", r));
  daemon.kill();
  await stopped;
}
const request = async (path, method = "GET", body) => {
  const response = await fetch(url + "/api/v1" + path, {
    method,
    headers: {
      Authorization: `Bearer ${pat}`,
      "Content-Type": "application/json",
    },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!response.ok) throw Error(`${path}: ${response.status}`);
  return response.json();
};
try {
  await start();
  if ((await request("/devices")).length)
    throw Error("New install contains demo devices");
  for (const name of ["Screen A", "Screen B", "Screen C"])
    await request("/devices", "POST", {
      device: { name, driver_id: "simulator", transport: "simulator" },
    });
  const devices = await request("/devices"),
    ids = devices.map((d) => d.device_id);
  const prefs = await request("/preferences");
  await request("/preferences", "PUT", {
    ...prefs,
    selected: ids[0],
    visible: ids,
    order: ids,
  });
  browser = await chromium.launch({ headless: true });
  let context = await browser.newContext({
    viewport: { width: 1440, height: 1000 },
  });
  let page = await context.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.goto(url + "/overview");
  await page.getByLabel("Personal Access Token").fill(pat);
  await page.getByRole("button", { name: "Anmelden →" }).click();
  await expect(page.locator(".wall img")).toHaveCount(3);
  await expect(page.locator(".brand-mark")).toHaveCount(0);
  await page
    .getByRole("button", { name: "Screen B nach vorne", exact: true })
    .click();
  await expect(page.locator(".wall > div").first()).toHaveAttribute(
    "data-device-id",
    ids[1],
  );
  await expect
    .poll(async () => (await request("/preferences")).order[0])
    .toBe(ids[1]);
  await page.reload();
  await expect(page.locator(".wall > div").first()).toHaveAttribute(
    "data-device-id",
    ids[1],
  );
  await expect(page.locator(".wall img")).toHaveCount(3);
  const colors = () =>
    page.evaluate(() =>
      Array.from(document.querySelectorAll("body *"))
        .flatMap((el) => {
          const css = getComputedStyle(el);
          return ["color", "backgroundColor", "borderTopColor"].map(
            (k) => css[k],
          );
        })
        .filter((c) => {
          const m = c.match(/^rgba?\((\d+), (\d+), (\d+)/);
          return m && !(m[1] === m[2] && m[2] === m[3]);
        }),
    );
  if ((await colors()).length) throw Error("Colored dark-mode chrome");
  await mkdir(".kvmhelm/qa", { recursive: true });
  await page.screenshot({
    path: ".kvmhelm/qa/compact-dark.png",
    fullPage: true,
  });
  await page
    .getByRole("button", { name: "Farbschema wechseln", exact: true })
    .click();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  if ((await colors()).length) throw Error("Colored light-mode chrome");
  await page.screenshot({
    path: ".kvmhelm/qa/compact-light.png",
    fullPage: true,
  });
  const top = await page
    .locator(".wall .display")
    .first()
    .evaluate((el) => el.getBoundingClientRect().top);
  if (top > 250) throw Error("Too much space above screens: " + top);
  const state = await context.storageState();
  if (
    !state.origins.some((o) =>
      o.localStorage.some(
        (v) => v.name === "kvmhelm.access-token" && v.value === pat,
      ),
    )
  )
    throw Error("Token not persisted");
  await context.close();
  await stop();
  await start();
  context = await browser.newContext({
    storageState: { cookies: [], origins: state.origins },
    viewport: { width: 1440, height: 1000 },
  });
  page = await context.newPage();
  await page.goto(url + "/overview");
  await expect(page.locator(".wall img")).toHaveCount(3);
  await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  await page.goto(url + "/settings");
  await page.getByRole("button", { name: "Abmelden", exact: true }).click();
  await expect(page.getByRole("button", { name: "Anmelden →" })).toBeVisible();
  if (await page.evaluate(() => localStorage.getItem("kvmhelm.access-token")))
    throw Error("Logout retained token");
  if (errors.length) throw Error(errors.join("\n"));
  console.log(
    "PASS: empty install, persisted ordering, monochrome dark/light, compact screen area, no logo, token survives browser/daemon restart without cookies, logout clears browser token.",
  );
} finally {
  await browser?.close();
  await stop();
  const target = resolve(dir),
    base = resolve(tmpdir()) + sep;
  if (
    !target.startsWith(base) ||
    !target.slice(base.length).startsWith("kvmhelm-ui-")
  )
    throw Error("Unsafe cleanup target");
  await rm(target, { recursive: true, force: true });
}
