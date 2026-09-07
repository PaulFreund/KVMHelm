import { test } from "node:test";
import assert from "node:assert/strict";
import { chromium, expect } from "@playwright/test";
import { fixture } from "./fixture.js";
import { serve } from "../server/server.js";
import { Core } from "../server/core.js";
import { PluginHost } from "../server/plugins.js";
import { join } from "node:path";

test(
  "browser isolates local typing, rotates journals and recovers sessions/events after restart",
  { timeout: 60000 },
  async (t) => {
    const f = await fixture(t);
    let core = f.core,
      plugins = f.plugins;
    let daemon = await serve(core, plugins, {
      host: "127.0.0.1",
      port: 0,
      origins: [],
    });
    const port = (daemon.server.address() as { port: number }).port,
      base = `http://127.0.0.1:${port}`;
    const browser = await chromium.launch({ headless: true });
    try {
      const page = await browser.newPage();
      const errors: string[] = [],
        actions: unknown[] = [];
      let streams = 0,
        opens = 0;
      page.on("pageerror", (e) => errors.push(e.message));
      page.on("request", (q) => {
        if (q.url().endsWith("/tools/computer")) actions.push(q.postDataJSON());
        if (q.url().endsWith("/events")) streams++;
        if (q.url().endsWith("/tools/open_computer")) opens++;
      });
      await page.addInitScript(
        (token) => localStorage.setItem("kvmhelm.access-token", token),
        f.token.token,
      );
      let firstOpen = true;
      await page.route("**/tools/open_computer", (route) => {
        if (firstOpen) {
          firstOpen = false;
          return route.fulfill({
            json: {
              isError: true,
              structuredContent: {
                ok: false,
                frames: [],
                error: {
                  code: "DEVICE_OFFLINE",
                  message: "Synthetic startup outage",
                },
              },
              content: [],
            },
          });
        }
        return route.continue();
      });
      await page.goto(base + "/computers");
      await expect(page.locator(".screen img")).toHaveCount(1);
      assert.ok(opens >= 2, "initial open must retry");
      await page
        .getByRole("button", { name: "Steuerung anfordern", exact: true })
        .click();
      await expect(
        page.getByRole("button", { name: "Steuerung freigeben", exact: true }),
      ).toBeVisible();
      await page
        .getByRole("button", { name: "Tastatur aktivieren", exact: true })
        .click();
      const field = page.getByLabel("Text auf Ziel eingeben");
      await field.click();
      await field.pressSequentially("abc");
      await expect(field).toHaveValue("abc");
      assert.equal(actions.length, 0);

      const session = [...core.sessions.values()][0];
      const identity = {
        token: f.auth.tokens.get(session.token_id)!,
        client_id: session.client_id,
      };
      for (let n = 0; n < 249; n++) {
        const r = await core.tool(identity, "computer_control", {
          session_id: session.session_id,
          request_id: `fill-${n}`,
          operation: "renew",
        });
        assert.equal(r.ok, true);
      }
      await page.getByRole("button", { name: "Senden", exact: true }).click();
      await expect.poll(() => core.metrics.batches).toBe(1);
      await field.fill("def");
      await page.getByRole("button", { name: "Senden", exact: true }).click();
      await expect.poll(() => core.metrics.batches).toBe(2);
      assert.equal(
        core.sessions.has(session.session_id),
        false,
        "journal must rotate before exhaustion",
      );
      assert.equal([...core.sessions.values()].length, 1);

      await daemon.close();
      await plugins.shutdown();
      await core.shutdown();
      core = new Core(f.store, f.secrets, f.auth);
      await core.load();
      plugins = new PluginHost(core, join(f.dir, "plugins"));
      daemon = await serve(core, plugins, {
        host: "127.0.0.1",
        port,
        origins: [],
      });
      await expect.poll(() => core.sessions.size, { timeout: 15000 }).toBe(1);
      await expect(page.locator(".frame-error")).toHaveCount(0);
      await expect(
        page.getByRole("button", { name: "Steuerung anfordern", exact: true }),
      ).toBeVisible();
      assert.equal(
        core.get(f.device.device_id).lease,
        undefined,
        "recovery must not reacquire control",
      );
      await expect.poll(() => streams).toBeGreaterThan(1);
      core.event("review.recovered", f.device.device_id, {});
      await page.getByRole("button", { name: /Ereignisse/ }).click();
      await expect(
        page.getByText("review.recovered", { exact: true }),
      ).toBeVisible();
      assert.deepEqual(errors, []);
    } finally {
      await browser.close();
      await daemon.close();
      await plugins.shutdown();
      await core.shutdown();
    }
  },
);
