// Native policy/lifecycle + actual Node advanced-serialization fork. All input,
// plugins, media and storage are isolated synthetic fixtures.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import {
  copyFile,
  chmod,
  mkdtemp,
  mkdir,
  readFile,
  writeFile,
  rm,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve, sep } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { chromium, expect } from "@playwright/test";

assert.ok(process.env.KVMHELM_TEST_BINARY, "Set KVMHELM_TEST_BINARY");
const dir = await mkdtemp(join(tmpdir(), "kvmhelm-native-plugins-"));
const executable = join(
  dir,
  process.platform === "win32" ? "kvmhelm.exe" : "kvmhelm",
);
const env = Object.fromEntries(
  Object.entries(process.env).filter(
    ([k]) => !k.toUpperCase().startsWith("KVMHELM_"),
  ),
);
env.PATH = "";
env.KVMHELM_SYNTHETIC_SECRET = "must-not-reach-plugin";
const args = ["--data", join(dir, "data"), "--secrets", join(dir, "secrets")];
const options = { cwd: dir, env, windowsHide: true };
let daemon,
  browser,
  gateway = "",
  pat = "",
  logs = "";
const pluginPath = join(dir, "panel-plugin");
const panelId = "notes /Ä+%25";
const manifest = {
  id: "local.panels",
  version: "1",
  apiVersion: "kvmhelm.plugin/v1",
  kind: "feature",
  runtime: "external-process",
  entry: "index.mjs",
  permissions: [
    "ui:panel",
    "notifications:publish",
    "video:read",
    "audio:read:from_target",
  ],
  uiSlots: ["kvm.sidepanel"],
  secretRefs: ["fixture.reference"],
};
const pluginSource = `
let device,own,permissions,devices,secretRefs,received=0,rejected=0,requests=0,healthy=true;
const fields=(terms=['Hello'],languages=[],threshold=3)=>[{id:'terms',label:'Notify strings',type:'string-list',value:terms},{id:'languages',label:'Expected languages',type:'multi-select',value:languages,options:[{value:'de',label:'German'},{value:'en',label:'English'}]},{id:'threshold',label:'Threshold',type:'number',value:threshold}];
const publish=(text='Safe text <script>bad()</script>',f=fields())=>process.send({type:'panel',panel:{id:${JSON.stringify(panelId)},title:'Live notes',device_id:device,slot:'kvm.sidepanel',text,highlights:[{start:5,end:9}],fields:f,actions:['save','silent','crash','flood','silence-health','block'].map(id=>({id,label:id==='save'?'Save strings':id}))}});
const summary=()=>process.send({type:'panel',panel:{id:'runtime',title:'Runtime',device_id:device,slot:'kvm.sidepanel',text:JSON.stringify({pid:process.pid,broker:process.ppid,own,permissions,devices,secretRefs,received,rejected,requests,leaked:!!process.env.KVMHELM_SYNTHETIC_SECRET,connected:process.connected}),actions:[]}});
process.on('message',m=>{
 if(m.type==='initialize'){device=m.devices[0];devices=m.devices;own=m.data_path;permissions=m.permissions;secretRefs=m.secret_refs;process.send({type:'ready'});publish();summary();}
 if(m.type==='start'){process.send({type:'devices'});process.send({type:'subscribe',id:'same',source:'video',device_id:device});process.send({type:'subscribe',id:'same',source:'video',device_id:device});process.send({type:'subscribe',id:'audio',source:'audio',device_id:device});process.send({type:'panel',panel:{id:'forbidden',title:'No',text:'No',slot:'settings.plugins'}});process.send({type:'notification',device_id:'ungranted',text:'forbidden'});}
 if(m.type==='devices'){devices=m.devices.map(d=>d.device_id);summary();}
 if(m.type==='health'&&healthy)process.send({type:'health'});
 if(m.type==='media'){if(!Buffer.isBuffer(m.chunk.data)||m.chunk.data[0]!==137||!m.chunk.frame_id)process.exit(31);if(++received===1)summary();}
 if(m.type==='error'){rejected++;if(rejected===3)setTimeout(summary,1200);}
 if(m.type==='action'){
   requests++;
   if(m.action==='crash')process.exit(42);
   if(m.action==='save'){publish(m.values.terms.join(', ')+' / '+m.values.languages.join(','),fields(m.values.terms,m.values.languages,m.values.threshold));process.send({type:'notification',device_id:device,text:'A new match',reference:{panel_id:${JSON.stringify(panelId)},item_id:'match',revision:requests}});}
   if(m.action==='flood'){for(let n=0;n<40;n++)publish('flood '+n);setTimeout(summary,1200);}
   if(m.action==='silence-health')healthy=false;
   if(m.action==='block'){process.send({type:'action.result',request_id:m.request_id,ok:true});const end=Date.now()+9000;while(Date.now()<end){};return;}
   if(m.action==='silent'){summary();return;}
   process.send({type:'action.result',request_id:m.request_id,ok:true});
 }
 if(m.type==='dispose')process.exit(0);
});
`;
async function until(check, label, timeout = 12000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    if (await check()) return;
    await delay(30);
  }
  throw Error(label + " timed out; " + logs);
}
async function start(node = false) {
  gateway = "";
  logs = "";
  const childEnv = { ...env };
  if (node) childEnv.KVMHELM_NODE = process.execPath;
  daemon = spawn(
    executable,
    ["serve", "--host", "127.0.0.1", "--port", "0", ...args],
    { ...options, env: childEnv, stdio: ["ignore", "ignore", "pipe"] },
  );
  daemon.stderr.on("data", (d) => {
    logs = (logs + d.toString()).slice(-8000);
    const m = logs.match(/KVMHelm listening on (http:\/\/[^\s]+)/);
    if (m) gateway = m[1];
  });
  await until(() => {
    if (daemon.exitCode !== null) throw Error(logs);
    return gateway;
  }, "Startup");
}
async function stopDaemon() {
  const child = daemon;
  daemon = undefined;
  if (child?.exitCode === null && child.signalCode === null) {
    const end = new Promise((r) => child.once("exit", r));
    child.kill();
    await end;
  }
}
async function request(path, method = "GET", body, token = pat) {
  const response = await fetch(gateway + "/api/v1" + path, {
    method,
    headers: {
      authorization: "Bearer " + token,
      "content-type": "application/json",
    },
    body: body ? JSON.stringify(body) : undefined,
    signal: AbortSignal.timeout(15000),
  });
  return { status: response.status, body: await response.json() };
}
async function api(path, method = "GET", body, token = pat) {
  const value = await request(path, method, body, token);
  assert.equal(value.status, 200, JSON.stringify(value.body));
  return value.body;
}
async function plugins() {
  return api("/plugins");
}
async function consumers(id, plugin) {
  const device = (await api("/devices")).find((d) => d.device_id === id);
  return device.consumers.filter((c) =>
    c.id.startsWith("plugin:" + plugin + ":"),
  );
}
async function runtime(id = "local.panels") {
  const p = (await plugins()).find((p) => p.id === id);
  const panel = p?.panels.find((p) => p.id === "runtime");
  return panel ? JSON.parse(panel.text) : undefined;
}
async function action(name, values = {}, id = "local.panels", token = pat) {
  return request(
    "/plugins/" + id + "/panels/" + encodeURIComponent(panelId) + "/actions",
    "POST",
    { action: name, values },
    token,
  );
}
async function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    if (e.code === "ESRCH") return false;
    throw e;
  }
}
try {
  await copyFile(resolve(process.env.KVMHELM_TEST_BINARY), executable);
  await chmod(executable, 0o700);
  const boot = spawnSync(executable, ["token", "bootstrap", ...args], {
    ...options,
    encoding: "utf8",
  });
  assert.equal(boot.status, 0, boot.stderr);
  pat = (await readFile(join(dir, "secrets", "cli.pat"), "utf8")).trim();
  await mkdir(pluginPath);
  await writeFile(
    join(pluginPath, "kvmhelm.plugin.json"),
    JSON.stringify(manifest),
  );
  await writeFile(join(pluginPath, "index.mjs"), pluginSource);
  await start();
  assert.deepEqual(await plugins(), []);
  const devices = [];
  for (const name of ["Fixture", "Other"])
    devices.push(
      await api("/devices", "POST", {
        device: {
          name,
          driver_id: "simulator",
          transport: "simulator",
          media_profile: "warm",
          minimum_action_latency_ms: 0,
        },
      }),
    );
  const first = await api("/plugins", "POST", {
    path: pluginPath,
    devices: [devices[0].device_id],
  });
  assert.equal(
    first.status,
    "error",
    "empty PATH must not silently promise a Node runtime",
  );
  assert.equal(first.runtime_error.code, "DEPENDENCY_MISSING");
  assert.equal((await api("/diagnostics")).plugins[0].status, "error");
  await stopDaemon();
  await start(true);
  await until(async () => {
    const r = await runtime();
    return r?.received > 0 && r?.rejected >= 3;
  }, "Advanced IPC Buffer, grants and rejection results");
  let r = await runtime();
  assert.equal(r.leaked, false);
  assert.equal(r.connected, true);
  assert.deepEqual(r.devices, [devices[0].device_id]);
  assert.deepEqual(r.secretRefs, ["fixture.reference"]);
  assert.ok(
    resolve(r.own).startsWith(resolve(join(dir, "data", "plugins")) + sep),
  );
  const broker = r.broker;
  assert.ok(await alive(broker));
  assert.equal((await plugins())[0].status, "running");
  assert.ok(!(await plugins())[0].panels.some((p) => p.id === "forbidden"));
  const owner = await api("/tokens", "POST", {
    name: "reader",
    scopes: ["devices:read"],
    devices: [devices[0].device_id],
  });
  assert.equal(
    (
      await action(
        "save",
        { terms: ["no"], languages: [], threshold: 3 },
        undefined,
        owner.token,
      )
    ).status,
    403,
  );
  const other = await api("/tokens", "POST", {
    name: "other device",
    scopes: ["devices:read", "plugins:manage"],
    devices: [devices[1].device_id],
  });
  assert.equal(
    (await api("/ui-extensions", "GET", undefined, other.token))[0].panels
      .length,
    0,
  );
  assert.equal(
    (
      await action(
        "save",
        { terms: ["no"], languages: [], threshold: 3 },
        undefined,
        other.token,
      )
    ).status,
    403,
  );
  for (const [name, values] of [
    ["unknown", {}],
    ["save", { unexpected: [] }],
    ["save", { languages: ["xx"] }],
    ["save", { threshold: "3" }],
  ])
    assert.notEqual((await action(name, values)).status, 200);
  assert.equal(
    (
      await action("save", {
        terms: ["Alpha", "Beta"],
        languages: ["de", "en"],
        threshold: 7,
      })
    ).status,
    200,
  );
  const silentAt = Date.now();
  assert.equal((await action("silent")).body.error.code, "PLUGIN_ERROR");
  assert.ok(Date.now() - silentAt >= 9500);
  const count = (await runtime()).requests;
  await delay(200);
  assert.equal(
    (await runtime()).requests,
    count,
    "no automatic retry after an unknown plugin action",
  );
  assert.equal((await action("flood")).status, 200);
  await until(
    async () => (await runtime()).rejected > 3,
    "Plugin event budget",
  );
  await api("/plugins/local.panels/restart", "POST", {});
  await until(async () => {
    const p = (await plugins())[0];
    return (
      p.status === "running" &&
      p.panels.find((p) => p.id === panelId)?.text.startsWith("Safe text")
    );
  }, "Restart");
  assert.equal(
    (await runtime()).broker,
    broker,
    "plugins share the optional IPC broker",
  );
  browser = await chromium.launch({ headless: true, args: ["--mute-audio"] });
  const page = await browser.newPage({
    viewport: { width: 1500, height: 1000 },
  });
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.addInitScript(
    (token) => localStorage.setItem("kvmhelm.access-token", token),
    pat,
  );
  await page.goto(gateway + "/computers");
  await page
    .getByRole("button", { name: "Fixture", exact: true })
    .first()
    .click();
  await expect(page.locator(".screen img")).toBeVisible();
  await expect(page.locator(".plugin-notice")).toHaveCount(0);
  const side = page.getByRole("complementary", { name: "Seitenpanel Fixture" });
  const toggle = page.getByRole("button", {
    name: /Seitenpanel (anzeigen|ausblenden)/,
  });
  try {
    await expect(toggle).toBeVisible();
    if ((await toggle.getAttribute("aria-expanded")) !== "true")
      await toggle.click();
  } catch (error) {
    console.error("Panel diagnostics", {
      extensions: await api("/ui-extensions"),
      buttons: await page.getByRole("button").allTextContents(),
      pageErrors: errors,
    });
    throw error;
  }
  await expect(side).toBeVisible();
  await expect(
    side.getByText("Safe text <script>bad()</script>", { exact: true }),
  ).toBeVisible();
  await expect(side.locator("mark").first()).toHaveText("text");
  await side.getByText("Einstellungen", { exact: true }).first().click();
  await side.getByLabel("Notify strings").fill("Browser\nUpdate");
  await side.getByLabel("German", { exact: true }).check();
  await side.getByLabel("Threshold", { exact: true }).fill("9");
  await side.getByRole("button", { name: "Save strings", exact: true }).click();
  await expect(side.getByLabel("Live notes", { exact: true })).toHaveText(
    "Browser, Update / de",
  );
  await expect(page.locator(".plugin-notice")).toHaveCount(1);
  await page.reload();
  await expect(side).toBeVisible();
  await expect(page.locator(".plugin-notice")).toHaveCount(0);
  assert.deepEqual(errors, []);
  await browser.close();
  browser = undefined;
  const before = (await runtime()).pid;
  await api("/plugins/local.panels/disable", "POST", {});
  await until(async () => !(await alive(before)), "Disable reaps plugin");
  assert.equal((await plugins())[0].panels.length, 0);
  assert.equal((await plugins())[0].enabled, false);
  await until(
    async () =>
      (await consumers(devices[0].device_id, "local.panels")).length === 0,
    "Disable releases every media consumer",
  );
  await api("/plugins/local.panels/enable", "POST", {});
  await until(
    async () => (await runtime())?.received > 0,
    "Enable recreates media subscription",
  );
  assert.equal((await action("crash")).body.error.code, "PLUGIN_ERROR");
  await until(
    async () => (await plugins())[0].status === "error",
    "Crash closes runtime",
  );
  await api("/plugins/local.panels/restart", "POST", {});
  await until(
    async () =>
      (await runtime())?.received > 0 &&
      (await plugins())[0].status === "running",
    "Recovery after crash",
  );
  assert.equal((await action("silence-health")).status, 200);
  const sick = (await runtime()).pid;
  await until(
    async () => (await plugins())[0].status === "error",
    "Missing health stops consumers",
    45000,
  );
  await until(async () => !(await alive(sick)), "Missing health reaps plugin");
  await until(
    async () =>
      (await consumers(devices[0].device_id, "local.panels")).length === 0,
    "Missing health releases media",
  );
  await api("/plugins/local.panels/restart", "POST", {});
  await until(
    async () => (await runtime())?.received > 0,
    "Restart after health failure",
  );
  const secondPath = join(dir, "other-plugin");
  await mkdir(secondPath);
  await writeFile(
    join(secondPath, "kvmhelm.plugin.json"),
    JSON.stringify({ ...manifest, id: "local.other" }),
  );
  await writeFile(join(secondPath, "index.mjs"), pluginSource);
  await api("/plugins", "POST", {
    path: secondPath,
    devices: [devices[0].device_id],
  });
  await until(
    async () => (await runtime("local.other"))?.received > 0,
    "Second plugin",
  );
  const otherRuntime = await runtime("local.other");
  assert.equal(
    otherRuntime.broker,
    (await runtime()).broker,
    "one broker for multiple plugins",
  );
  assert.equal((await action("block")).status, 200);
  await until(
    async () =>
      (await consumers(devices[0].device_id, "local.panels")).length === 0,
    "Slow media receiver is bounded and detached",
    12000,
  );
  assert.equal(
    (await plugins()).find((p) => p.id === "local.other").status,
    "running",
  );
  assert.ok(await alive(otherRuntime.pid));
  assert.ok(
    (await consumers(devices[0].device_id, "local.other")).length > 0,
    "other plugin continues after a slow receiver",
  );
  await api("/plugins/local.other/uninstall", "POST", {});
  await api("/plugins/local.panels/restart", "POST", {});
  await until(
    async () => (await runtime())?.received > 0,
    "Restart after slow receiver",
  );
  manifest.version = "2";
  await writeFile(
    join(pluginPath, "kvmhelm.plugin.json"),
    JSON.stringify(manifest),
  );
  const cli = spawnSync(
    executable,
    [
      "plugin",
      "install",
      pluginPath,
      "--device",
      devices[1].device_id,
      "--url",
      gateway,
      ...args,
    ],
    { ...options, encoding: "utf8" },
  );
  assert.equal(cli.status, 0, cli.stderr);
  const cliList = spawnSync(
    executable,
    ["plugin", "list", "--url", gateway, ...args],
    { ...options, encoding: "utf8" },
  );
  assert.equal(cliList.status, 0, cliList.stderr);
  assert.equal(JSON.parse(cliList.stdout)[0].version, "2");
  await until(
    async () =>
      (await runtime())?.devices[0] === devices[1].device_id &&
      (await runtime())?.received > 0,
    "Update changes only explicit grants",
  );
  assert.equal((await plugins())[0].version, "2");
  assert.equal((await runtime()).broker, broker);
  await api("/plugins/local.panels/uninstall", "POST", {});
  assert.deepEqual(await plugins(), []);
  // A disposed plugin which ignores stop must still be reaped after two seconds.
  await writeFile(
    join(pluginPath, "index.mjs"),
    `process.on('message',m=>{if(m.type==='initialize'){process.send({type:'ready'});process.send({type:'panel',panel:{id:'runtime',title:'pid',slot:'kvm.sidepanel',text:JSON.stringify({pid:process.pid,broker:process.ppid})}});}if(m.type==='health')process.send({type:'health'});});setInterval(()=>{},1000);`,
  );
  await api("/plugins", "POST", {
    path: pluginPath,
    devices: [devices[0].device_id],
  });
  await until(async () => !!(await runtime()), "Uncooperative plugin starts");
  const stubborn = (await runtime()).pid,
    at = Date.now();
  await api("/plugins/local.panels/disable", "POST", {});
  assert.ok(Date.now() - at < 5000);
  await until(
    async () => !(await alive(stubborn)),
    "Forced stop reaps own child",
  );
  await api("/plugins/local.panels/uninstall", "POST", {});
  await writeFile(join(pluginPath, "index.mjs"), pluginSource);
  await api("/plugins", "POST", {
    path: pluginPath,
    devices: [devices[0].device_id],
  });
  await until(async () => (await runtime())?.received > 0, "Final restart");
  const interrupted = await runtime();
  process.kill(interrupted.broker, "SIGKILL");
  await api("/plugins/local.panels/restart", "POST", {});
  await until(
    async () =>
      (await runtime())?.received > 0 &&
      (await plugins())[0].status === "running" &&
      (await runtime()).broker !== interrupted.broker,
    "Explicit restart replaces a failed broker without stale exit events",
  );
  await until(
    async () => !(await alive(interrupted.pid)),
    "Failed broker reaps its forked child",
  );
  const final = await runtime();
  await stopDaemon();
  await until(
    async () => !(await alive(final.pid)) && !(await alive(final.broker)),
    "Daemon death reaps owned plugin tree",
  );
  console.log(
    "Native plugins: optional Node, real advanced IPC Buffers, scoped media/panels/actions, event budget, unknown action without replay, Vue forms/notifications, CLI update, health failure, slow-receiver isolation, crash/broker recovery, forced stop and process-tree cleanup passed",
  );
} catch (error) {
  console.error("Native plugin fixture failed:", error);
  throw error;
} finally {
  await browser?.close();
  await stopDaemon();
  assert.ok(resolve(dir).startsWith(resolve(tmpdir()) + sep));
  await rm(dir, {
    recursive: true,
    force: true,
    maxRetries: 40,
    retryDelay: 100,
  });
}
