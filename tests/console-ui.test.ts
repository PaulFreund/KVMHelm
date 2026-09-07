import { test } from "node:test";
import assert from "node:assert/strict";
import { chromium, expect } from "@playwright/test";
import { setTimeout as delay } from "node:timers/promises";
import { fixture } from "./fixture.js";
import { serve } from "../server/server.js";
import { mkdir } from "node:fs/promises";

test(
  "console unifies focus, isolates paste, supports mouse and independent microphone/audio",
  { timeout: 60000 },
  async (t) => {
    const f = await fixture(t);
    await f.core.connect(f.core.get(f.device.device_id));
    const runtime = f.core.get(f.device.device_id);
    Object.assign(runtime.caps!.audio, {
      from_target: true,
      to_target: true,
      reason: "",
    });
    const micChanges: boolean[] = [];
    let packets = 0;
    runtime.driver.setMicrophone = async (enabled) => {
      micChanges.push(enabled);
    };
    runtime.driver.sendAudio = async (_, ctx) => {
      ctx.assertControl();
      packets++;
    };
    runtime.driver.subscribeAudio = async function* (signal) {
      while (!signal.aborted) {
        await delay(30);
        if (!signal.aborted)
          yield {
            device_id: f.device.device_id,
            format: "pcm_s16le" as const,
            sample_rate: 48000,
            channels: 2,
            sequence: 0,
            timestamp_ms: performance.now(),
            connection_generation: 1,
            discontinuity: false,
            data: Buffer.alloc(5760),
          };
      }
    };
    const daemon = await serve(f.core, f.plugins, {
      host: "127.0.0.1",
      port: 0,
      origins: [],
    });
    const base =
      "http://127.0.0.1:" + (daemon.server.address() as { port: number }).port;
    const browser = await chromium.launch({
      headless: true,
      args: [
        "--use-fake-device-for-media-stream",
        "--use-fake-ui-for-media-stream",
      ],
    });
    try {
      const page = await browser.newPage({
        viewport: { width: 1440, height: 1100 },
      });
      const errors: string[] = [],
        actions: any[] = [];
      page.on("pageerror", (e) => errors.push(e.message));
      page.on("request", (q) => {
        if (q.url().endsWith("/tools/computer"))
          actions.push(...q.postDataJSON().actions);
      });
      await page.addInitScript(
        (token) => localStorage.setItem("kvmhelm.access-token", token),
        f.token.token,
      );
      await page.goto(base + "/computers");
      await expect(page.locator(".screen img")).toBeVisible();
      const target = page.getByRole("group", {
        name: "KVM-Bildschirm Fixture",
      });
      await page
        .getByRole("button", { name: "Steuerung übernehmen", exact: true })
        .click();
      await expect(target).toBeFocused();
      await page.keyboard.press("a");
      await expect
        .poll(() => actions.filter((a) => a.type === "type").length)
        .toBe(1);
      assert.equal(actions.find((a) => a.type === "type").text, "a");
      await expect.poll(() => f.core.metrics.batches).toBe(1);
      await page.keyboard.press("Control+Alt+Escape");
      await expect(target).not.toBeFocused();
      assert.equal(
        actions.some((a) => a.keys?.includes("Escape")),
        false,
      );
      await page
        .getByRole("button", { name: "Text / Paste", exact: true })
        .click();
      const field = page.getByLabel("Text auf Ziel eingeben");
      await field.fill("line 1\nline 2");
      const before = actions.length;
      await page.waitForTimeout(100);
      assert.equal(actions.length, before);
      let fail = true;
      await page.route("**/tools/computer", (route) => {
        if (fail) {
          fail = false;
          return route.fulfill({
            json: {
              isError: true,
              structuredContent: {
                ok: false,
                frames: [],
                error: {
                  code: "UNSUPPORTED_ACTION",
                  message: "Synthetic input failure",
                },
              },
              content: [],
            },
          });
        }
        return route.continue();
      });
      await page.getByRole("button", { name: "Senden", exact: true }).click();
      await expect(
        page.getByRole("alert").filter({ hasText: "Synthetic input failure" }),
      ).toBeVisible();
      await expect(field).toHaveValue("line 1\nline 2");
      await page.getByRole("button", { name: "Senden", exact: true }).click();
      await expect(field).toHaveValue("");
      await expect(target).toBeFocused();
      // Pasting into the remote viewport opens a local preview, never types immediately.
      const pastedBefore = actions.length;
      await target.evaluate((el) => {
        const data = new DataTransfer();
        data.setData("text/plain", "review me\n");
        el.dispatchEvent(
          new ClipboardEvent("paste", {
            bubbles: true,
            cancelable: true,
            clipboardData: data,
          }),
        );
      });
      await expect(field).toHaveValue("review me\n");
      assert.equal(actions.length, pastedBefore);
      await field.fill("x".repeat(4097));
      await expect(
        page.getByRole("button", { name: "Senden", exact: true }),
      ).toBeDisabled();
      await field.fill("");
      await page
        .getByRole("button", { name: "Ctrl Alt Del", exact: true })
        .click();
      await expect
        .poll(() =>
          actions.some((a) => a.keys?.join(",") === "CTRL,ALT,DELETE"),
        )
        .toBe(true);
      await expect.poll(() => f.core.metrics.batches).toBe(3);
      const img = page.locator(".screen img");
      await img.dblclick({ position: { x: 80, y: 80 }, delay: 60 });
      await expect
        .poll(() => actions.filter((a) => a.type === "double_click").length)
        .toBe(1);
      assert.equal(
        actions.filter((a) => a.type === "click").length,
        0,
        "double click must not add an extra single click",
      );
      await expect.poll(() => f.core.metrics.batches).toBeGreaterThanOrEqual(4);
      await img.click({ position: { x: 100, y: 90 }, button: "right" });
      await expect
        .poll(() =>
          actions.some((a) => a.type === "click" && a.button === "right"),
        )
        .toBe(true);
      await page.getByRole("button", { name: "Originalgröße 1:1" }).click();
      await expect(target).toHaveClass(/native-size/);
      await page.getByRole("button", { name: "An Fenster anpassen" }).click();
      await page
        .getByRole("button", { name: "Ziel hören", exact: true })
        .click();
      await expect(
        page.getByRole("button", { name: "Ton ausschalten", exact: true }),
      ).toBeVisible();
      await page
        .getByRole("button", { name: "Mikrofon einschalten", exact: true })
        .click();
      await expect(
        page.getByRole("button", {
          name: "● Mikrofon ausschalten",
          exact: true,
        }),
      ).toBeVisible();
      await expect.poll(() => packets).toBeGreaterThan(0);
      assert.equal(micChanges.at(-1), true);
      // Long control sessions renew their bounded input journal without requiring another mic click.
      const oldSession = [...f.core.sessions.values()][0];
      const owner = {
        token: f.auth.tokens.get(oldSession.token_id)!,
        client_id: oldSession.client_id,
      };
      for (let n = oldSession.journal.size; n < 249; n++) {
        const result = await f.core.tool(owner, "computer_control", {
          session_id: oldSession.session_id,
          operation: "renew",
          request_id: "audio-rotation-" + n,
        });
        assert.equal(result.ok, true);
      }
      const oldBatches = f.core.metrics.batches;
      await page.getByRole("button", { name: "Alt Tab", exact: true }).click();
      await expect.poll(() => f.core.metrics.batches).toBe(oldBatches + 1);
      await page.getByRole("button", { name: "Esc", exact: true }).click();
      await expect
        .poll(() => f.core.sessions.has(oldSession.session_id))
        .toBe(false);
      await expect.poll(() => micChanges.filter(Boolean).length).toBe(2);
      await expect(
        page.getByRole("button", {
          name: "● Mikrofon ausschalten",
          exact: true,
        }),
      ).toBeVisible();
      await expect(
        page.getByRole("button", { name: "Ton ausschalten", exact: true }),
      ).toBeVisible();

      await page
        .getByRole("button", { name: "● Mikrofon ausschalten", exact: true })
        .click();
      await expect.poll(() => micChanges.at(-1)).toBe(false);
      await expect(
        page.getByRole("button", { name: "Ton ausschalten", exact: true }),
      ).toBeVisible();
      await page.getByLabel("Mikrofonmodus").selectOption("hold");
      const ptt = page.getByRole("button", {
        name: "Zum Sprechen halten",
        exact: true,
      });
      await ptt.focus();
      await page.keyboard.down("Space");
      await expect.poll(() => micChanges.at(-1)).toBe(true);
      await page.keyboard.up("Space");
      await expect.poll(() => micChanges.at(-1)).toBe(false);
      await expect(
        page.getByRole("button", { name: "Ton ausschalten", exact: true }),
      ).toBeVisible();
      await page.getByLabel("Mikrofonmodus").selectOption("toggle");
      // Cancel while browser permission acquisition is unresolved; late tracks must be stopped.
      await page.evaluate(() => {
        const original = navigator.mediaDevices.getUserMedia.bind(
          navigator.mediaDevices,
        );
        (window as any).originalMic = original;
        navigator.mediaDevices.getUserMedia = async (constraints) => {
          const stream = await original(constraints);
          (window as any).lateStream = stream;
          await new Promise<void>((resolve) => {
            (window as any).finishMic = resolve;
          });
          return stream;
        };
      });
      const activations = micChanges.filter(Boolean).length;
      await page
        .getByRole("button", { name: "Mikrofon einschalten", exact: true })
        .click();
      await expect
        .poll(() => page.evaluate(() => !!(window as any).finishMic))
        .toBe(true);
      await page
        .getByRole("button", { name: "Aktivierung abbrechen", exact: true })
        .click();
      await page.evaluate(() => {
        (window as any).finishMic();
        navigator.mediaDevices.getUserMedia = (window as any).originalMic;
      });
      await expect
        .poll(() =>
          page.evaluate(
            () => (window as any).lateStream.getTracks()[0].readyState,
          ),
        )
        .toBe("ended");
      assert.equal(micChanges.filter(Boolean).length, activations);
      await expect(
        page.getByRole("button", { name: "Ton ausschalten", exact: true }),
      ).toBeVisible();

      await page
        .getByRole("button", { name: "Mikrofon einschalten", exact: true })
        .click();
      await expect.poll(() => micChanges.at(-1)).toBe(true);
      await page
        .getByRole("button", { name: "Steuerung freigeben", exact: true })
        .click();
      await expect.poll(() => micChanges.at(-1)).toBe(false);
      await expect(
        page.getByRole("button", { name: "Ton ausschalten", exact: true }),
      ).toBeVisible();
      await expect(
        page.getByRole("button", { name: "Mikrofon einschalten", exact: true }),
      ).toBeDisabled();
      await mkdir(".kvmhelm/review", { recursive: true });
      await page.screenshot({
        path: ".kvmhelm/review/console-desktop.png",
        fullPage: true,
      });
      await page.setViewportSize({ width: 390, height: 844 });
      await page.screenshot({
        path: ".kvmhelm/review/console-mobile.png",
        fullPage: true,
      });
      assert.ok(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth + 1,
        ),
        "mobile page must not overflow horizontally",
      );
      assert.deepEqual(errors, []);
    } finally {
      await browser.close();
      await daemon.close();
    }
  },
);
