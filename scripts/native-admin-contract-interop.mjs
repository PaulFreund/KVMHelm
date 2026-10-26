// Compare actual original/native HTTP methods and offline CLI side effects.
// All stores, tokens and the deliberately executable plugin belong to this fixture.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { copyFile, chmod, mkdir, mkdtemp, readFile, writeFile, rm, realpath } from "node:fs/promises";
import { createServer } from "node:net";
import { join, resolve, sep } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { referencePath, referenceRoot } from "./reference.mjs";

assert.ok(process.env.KVMHELM_TEST_BINARY, "Set KVMHELM_TEST_BINARY");
const parent = resolve("target/admin-contract-tests");
await mkdir(parent, { recursive: true });
const temporary = await mkdtemp(join(parent, "fixture-"));
const native = join(temporary, process.platform === "win32" ? "kvmhelm.exe" : "kvmhelm");
await copyFile(resolve(process.env.KVMHELM_TEST_BINARY), native);
await chmod(native, 0o700);
const env = Object.fromEntries(Object.entries(process.env).filter(([name]) =>
  !name.toUpperCase().startsWith("KVMHELM_") && !["NODE_OPTIONS", "NODE_EXTRA_CA_CERTS"].includes(name)));
env.KVMHELM_NODE = process.execPath;
const paths = ["/healthz", "/readyz", "/", "/overview", "/api/v1/me", "/api/v1/devices", "/api/v1/tokens", "/api/v1/preferences", "/api/v1/sessions", "/api/v1/plugins", "/api/v1/ui-extensions", "/api/v1/diagnostics", "/api/v1/absent", "/api/v1/audio", "/mcp"];
const report = {};
let daemon;
async function until(check, label) {
  const end = Date.now() + 30000;
  while (Date.now() < end) { if (await check()) return; await delay(50); }
  throw Error(label + " timed out");
}
const alive = pid => { try { process.kill(pid, 0); return true; } catch (e) { if (e.code === "ESRCH") return false; throw e; } };
async function stop() {
  if (!daemon || daemon.exitCode !== null) return;
  const ended = new Promise(done => daemon.once("exit", done));
  daemon.kill(); await ended; daemon = null;
}
try {
  for (const backend of ["reference", "native"]) {
    const directory = join(temporary, backend);
    await mkdir(directory);
    const state = ["--data", join(directory, "data"), "--secrets", join(directory, "secrets")];
    const executable = backend === "reference" ? process.execPath : native;
    const prefix = backend === "reference" ? [referencePath("dist/server/cli.js")] : [];
    const options = { cwd: backend === "reference" ? referenceRoot : directory, env: { ...env, ...(backend === "native" ? { PATH: "" } : {}) }, windowsHide: true };
    const cli = args => {
      const result = spawnSync(executable, [...prefix, ...args, ...state], { ...options, encoding: "utf8", timeout: 30000 });
      assert.equal(result.status, 0, result.stderr || result.error?.message);
    };
    cli(["token", "bootstrap"]);
    const token = (await readFile(join(directory, "secrets/cli.pat"), "utf8")).trim();
    const probe = createServer();
    await new Promise(done => probe.listen(0, "127.0.0.1", done));
    const port = probe.address().port;
    await new Promise(done => probe.close(done));
    const base = `http://127.0.0.1:${port}`;
    daemon = spawn(executable, [...prefix, "serve", "--host", "127.0.0.1", "--port", String(port), ...state], { ...options, stdio: ["ignore", "ignore", "pipe"] });
    let logs = "";
    daemon.stderr.on("data", bytes => { logs = (logs + bytes).slice(-4000); });
    await until(async () => {
      if (daemon.exitCode !== null) throw Error(logs);
      return fetch(base + "/readyz").then(r => r.ok, () => false);
    }, "Daemon startup");
    const request = async (path, method = "GET", body, authenticated = true) => {
      const response = await fetch(base + path, { method, headers: { ...(authenticated ? { authorization: "Bearer " + token } : {}), ...(body ? { "content-type": "application/json" } : {}) }, body: body ? JSON.stringify(body) : undefined, signal: AbortSignal.timeout(10000) });
      const text = await response.text();
      return { response, text };
    };
    const methods = {};
    for (const path of paths) {
      for (const method of ["HEAD", "OPTIONS"]) {
        const { response, text } = await request(path, method);
        methods[method + " " + path] = { status: response.status, type: response.headers.get("content-type")?.split(";")[0] ?? null, allow: response.headers.get("allow"), empty: text.length === 0 };
        if (method === "HEAD") assert.equal(text, "", "HEAD must never send a body");
      }
    }
    for (const authenticated of [false, true]) {
      const { response, text } = await request("/api/v1/devices", "HEAD", undefined, authenticated);
      methods["HEAD authorization " + authenticated] = { status: response.status, empty: !text };
    }
    const plugin = join(directory, "plugin"), marker = join(directory, "starts.jsonl");
    await mkdir(plugin);
    await writeFile(join(plugin, "kvmhelm.plugin.json"), JSON.stringify({ id: "contract.offline", version: "1", apiVersion: "kvmhelm.plugin/v1", kind: "feature", runtime: "external-process", entry: "index.mjs", permissions: [] }));
    await writeFile(join(plugin, "index.mjs"), `import {appendFileSync} from 'node:fs'; appendFileSync(${JSON.stringify(marker)},JSON.stringify({pid:process.pid})+String.fromCharCode(10)); process.on('message',m=>{if(m.type==='initialize')process.send({type:'ready'});if(m.type==='health')process.send({type:'health'});if(m.type==='dispose')process.exit(0);}); process.on('disconnect',()=>process.exit(0));`);
    const installed = await request("/api/v1/plugins", "POST", { path: plugin, devices: [] });
    assert.equal(installed.response.status, 200, installed.text);
    await until(() => readFile(marker, "utf8").then(s => !!s, () => false), "Fixture plugin start");
    await stop();
    const before = await readFile(marker, "utf8");
    const pids = before.trim().split("\n").filter(Boolean).map(s => JSON.parse(s).pid);
    await until(() => pids.every(pid => !alive(pid)), "Fixture plugin cleanup");
    cli(["demo"]);
    const after = await readFile(marker, "utf8");
    report[backend] = { methods, demoStartedPlugin: before !== after };
  }
  assert.equal(report.reference.demoStartedPlugin, false, "Original demo must stay offline");
  assert.deepEqual(report.native, report.reference, "Native public methods/offline CLI must preserve the original");
  console.log("PASS: original/native HEAD, OPTIONS, authorization and offline demo without plugin execution.");
} finally {
  await stop();
  const actual = await realpath(temporary), base = await realpath(parent);
  assert.ok(actual.startsWith(base + sep) && actual.slice(base.length + 1).startsWith("fixture-"));
  await rm(actual, { recursive: true, force: true });
}
