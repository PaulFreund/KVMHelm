import { test } from "node:test";
import assert from "node:assert/strict";
import { writeFile, mkdir } from "node:fs/promises";
import { join } from "node:path";
import { fixture } from "./fixture.js";
import { serve } from "../server/server.js";
import { chromium, expect } from "@playwright/test";

test(
  "generic sidepanels are optional, device-scoped, editable and notify only for live events",
  { timeout: 40000 },
  async (t) => {
    const f = await fixture(t),
      path = join(f.dir, "panel-plugin");
    await mkdir(path);
    await writeFile(
      join(path, "kvmhelm.plugin.json"),
      JSON.stringify({
        id: "local.panels",
        version: "1",
        apiVersion: "kvmhelm.plugin/v1",
        kind: "feature",
        runtime: "external-process",
        entry: "index.mjs",
        permissions: ["ui:panel", "notifications:publish"],
        uiSlots: ["kvm.sidepanel"],
      }),
    );
    await writeFile(
      join(path, "index.mjs"),
      `let device;const panel=()=>process.send({type:'panel',panel:{id:'notes',title:'Live notes',device_id:device,slot:'kvm.sidepanel',text:'Safe text <script>bad()</script>',fields:[{id:'terms',label:'Notify strings',type:'string-list',value:['Hello']}],actions:[{id:'save',label:'Save strings'}]}});process.on('message',m=>{if(m.type==='initialize'){device=m.devices[0];process.send({type:'ready'});panel();}if(m.type==='health')process.send({type:'health'});if(m.type==='action'){process.send({type:'panel',panel:{id:'notes',title:'Live notes',device_id:device,slot:'kvm.sidepanel',text:m.values.terms.join(', '),fields:[{id:'terms',label:'Notify strings',type:'string-list',value:m.values.terms}],actions:[{id:'save',label:'Save strings'}]}});process.send({type:'notification',device_id:device,text:'A new match'});process.send({type:'action.result',request_id:m.request_id,ok:true});}if(m.type==='dispose')process.exit(0);});`,
    );
    await f.plugins.install({ path, devices: [f.device.device_id] });
    const daemon = await serve(f.core, f.plugins, {
      host: "127.0.0.1",
      port: 0,
      origins: [],
    });
    const base =
      "http://127.0.0.1:" + (daemon.server.address() as { port: number }).port;
    const browser = await chromium.launch({ headless: true });
    try {
      await expect.poll(() => f.plugins.list()[0].panels.length).toBe(1);
      f.core.event("plugin.notification", f.device.device_id, {
        plugin_id: "local.panels",
        text: "Old notification",
      });
      const page = await browser.newPage({
        viewport: { width: 1500, height: 1000 },
      });
      await page.addInitScript(
        (token) => localStorage.setItem("kvmhelm.access-token", token),
        f.token.token,
      );
      await page.goto(base + "/computers");
      await expect(page.locator(".screen img")).toBeVisible();
      await expect(page.locator(".plugin-notice")).toHaveCount(0);
      await expect(
        page.getByRole("complementary", { name: "Seitenpanel Fixture" }),
      ).toHaveCount(0);
      await page.getByRole("button", { name: /Seitenpanel anzeigen/ }).click();
      const side = page.getByRole("complementary", {
        name: "Seitenpanel Fixture",
      });
      await expect(side).toBeVisible();
      await expect(
        side.getByText("Safe text <script>bad()</script>", { exact: true }),
      ).toBeVisible();
      const screenBox = await page.locator(".screen").boundingBox(),
        sideBox = await side.boundingBox();
      assert.ok(sideBox!.x >= screenBox!.x + screenBox!.width);
      await side.getByText("Einstellungen", { exact: true }).click();
      await side.getByLabel("Notify strings").fill("Alpha\nBeta");
      await side.getByRole("button", { name: "Save strings" }).click();
      await expect(side.locator("pre")).toHaveText("Alpha, Beta");
      await expect(page.locator(".plugin-notice")).toHaveCount(1);
      await page.reload();
      await expect(side).toBeVisible();
      await expect(page.locator(".plugin-notice")).toHaveCount(0);
      await page
        .getByRole("button", { name: /Seitenpanel ausblenden/ })
        .click();
      await expect(side).toHaveCount(0);
      await expect(page.locator(".screen img")).toBeVisible();
      const restricted = await f.auth.create({
        name: "read-only",
        scopes: ["devices:read"],
        devices: [f.device.device_id],
      });
      const response = await fetch(
        base + "/api/v1/plugins/local.panels/panels/notes/actions",
        {
          method: "POST",
          headers: {
            Authorization: "Bearer " + restricted.token,
            "Content-Type": "application/json",
          },
          body: JSON.stringify({
            action: "save",
            values: { terms: ["forbidden"] },
          }),
        },
      );
      assert.equal(response.status, 403);
      await assert.rejects(
        f.plugins.panelAction("local.panels", "notes", "unknown", {}, () => {}),
      );
      await assert.rejects(
        f.plugins.panelAction(
          "local.panels",
          "notes",
          "save",
          { unexpected: ["x"] },
          () => {},
        ),
      );
      await assert.rejects(
        f.plugins.panelAction("local.panels", "notes", "save", {}, () => {
          throw Error("Device denied");
        }),
        /Device denied/,
      );
    } finally {
      await browser.close();
      await daemon.close();
    }
  },
);
