// Opt-in Linux integration: one transient unit, private synthetic state and an
// existing unprivileged account. Never writes/enables a persistent unit.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import {
  cp,
  mkdir,
  mkdtemp,
  readFile,
  writeFile,
  chown,
  stat,
  rm,
} from "node:fs/promises";
import { join, resolve, sep } from "node:path";
import { setTimeout as delay } from "node:timers/promises";

assert.equal(process.platform, "linux");
assert.equal(
  process.env.KVMHELM_TEST_SYSTEMD,
  "1",
  "Explicitly opt into the isolated transient-unit test",
);
assert.equal(
  process.getuid(),
  0,
  "Root only creates the transient test unit; the daemon runs unprivileged",
);
assert.ok(
  process.env.KVMHELM_TEST_HELPER_DIRECTORY,
  "Set the extracted full package directory",
);
const source = resolve(process.env.KVMHELM_TEST_HELPER_DIRECTORY);
const unit = "kvmhelm-test-" + randomUUID() + ".service";
function command(program, args, options = {}) {
  const result = spawnSync(program, args, {
    encoding: "utf8",
    timeout: 110000,
    ...options,
  });
  assert.equal(result.status, 0, result.stderr || result.error?.message);
  return result.stdout.trim();
}
const uid = Number(command("id", ["-u", "nobody"]));
const gid = Number(command("id", ["-g", "nobody"]));
assert.ok(uid > 0 && gid > 0);
const template = await readFile(join(source, "kvmhelm.service"), "utf8");
const properties = [];
let section = "";
for (const raw of template.split(/\r?\n/)) {
  const line = raw.trim();
  if (line.startsWith("[")) {
    section = line;
    continue;
  }
  if (section !== "[Service]" || !line || line.startsWith("#")) continue;
  const name = line.split("=", 1)[0];
  if (
    [
      "User",
      "Group",
      "WorkingDirectory",
      "Environment",
      "EnvironmentFile",
      "ExecStart",
      "ReadWritePaths",
    ].includes(name)
  )
    continue;
  properties.push(line);
}
assert.ok(properties.includes("KillMode=mixed"));
assert.ok(properties.includes("ProtectSystem=strict"));
assert.ok(properties.includes("NoNewPrivileges=true"));
// PrivateTmp hides both /tmp and /var/tmp from the service. Keep this owned
// runtime fixture under /run, with only its state writable in the unit.
const fixture = await mkdtemp("/run/kvmhelm-test-");
const packageRoot = join(fixture, "package");
const state = join(fixture, "state");
const pluginRoot = join(fixture, "plugin");
const ownProcesses = new Set();
let launcher,
  launcherDone,
  gateway = "",
  logs = "",
  pat,
  device;
async function until(check, label, timeout = 20000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    if (await check()) return;
    await delay(50);
  }
  throw Error(label + " timed out: " + logs);
}
async function api(path, method = "GET", body) {
  const response = await fetch(gateway + "/api/v1" + path, {
    method,
    headers: {
      authorization: "Bearer " + pat,
      "content-type": "application/json",
      "x-kvmhelm-client": "systemd-fixture",
    },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(15000),
  });
  const value = await response.json();
  assert.equal(response.status, 200, JSON.stringify(value));
  return value;
}
async function runtime() {
  const panel = (await api("/plugins"))[0]?.panels.find(
    (p) => p.id === "runtime",
  );
  return panel ? JSON.parse(panel.text) : undefined;
}
function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    if (e.code === "ESRCH") return false;
    throw e;
  }
}
async function ready() {
  await until(async () => {
    if (launcher.exitCode !== null)
      throw Error("Transient unit exited: " + logs);
    return (
      gateway &&
      (await fetch(gateway + "/readyz")
        .then((r) => r.ok)
        .catch(() => false))
    );
  }, "Unit readiness");
}
async function proof(previous) {
  let value;
  await until(async () => {
    value = await runtime();
    return value?.received > 0 && (!previous || value.pid !== previous.pid);
  }, "Unprivileged packaged Node/Buffer proof");
  assert.equal(value.uid, uid);
  assert.equal(value.gid, gid);
  assert.equal(value.version, "v24.18.0");
  ownProcesses.add(value.pid);
  ownProcesses.add(value.broker);
  assert.equal(
    Number(command("systemctl", ["show", unit, "--property=User", "--value"])),
    uid,
  );
  const pid = Number(
    command("systemctl", ["show", unit, "--property=MainPID", "--value"]),
  );
  assert.ok(pid > 1);
  assert.equal((await stat("/proc/" + pid)).uid, uid);
  ownProcesses.add(pid);
  return { ...value, daemon: pid };
}
async function start() {
  gateway = "";
  logs = "";
  launcher = spawn(
    "systemd-run",
    [
      "--quiet",
      "--wait",
      "--pipe",
      "--collect",
      "--unit=" + unit,
      ...[
        ...properties,
        "User=" + uid,
        "Group=" + gid,
        "WorkingDirectory=" + packageRoot,
        "ReadWritePaths=" + state,
      ].map((p) => "--property=" + p),
      "--setenv=PATH=",
      "--setenv=KVMHELM_DATA=" + join(state, "data"),
      "--setenv=KVMHELM_SECRETS=" + join(state, "secrets"),
      join(packageRoot, "kvmhelm"),
      "serve",
      "--host",
      "127.0.0.1",
      "--port",
      "0",
    ],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  launcherDone = new Promise((done, reject) => {
    launcher.once("exit", done);
    launcher.once("error", reject);
  });
  launcherDone.catch(() => {});
  const output = (bytes) => {
    logs = (logs + bytes.toString()).slice(-10000);
    gateway =
      [...logs.matchAll(/KVMHelm listening on (http:\/\/[^\s]+)/g)].at(
        -1,
      )?.[1] ?? gateway;
  };
  launcher.stdout.on("data", output);
  launcher.stderr.on("data", output);
  await ready();
}
async function stop() {
  if (!launcher) return;
  const stopped = spawnSync("systemctl", ["stop", unit], {
    encoding: "utf8",
    timeout: 100000,
  });
  assert.equal(stopped.status, 0, stopped.stderr);
  const code = await Promise.race([
    launcherDone,
    delay(15000, -1, { ref: false }),
  ]);
  assert.equal(code, 0, "systemd-run must observe clean native exit: " + logs);
  launcher = undefined;
  await until(
    () => [...ownProcesses].every((pid) => !alive(pid)),
    "Owned daemon/broker/plugin exit",
  );
}
try {
  await cp(source, packageRoot, { recursive: true, errorOnExist: true });
  await chown(fixture, uid, gid);
  for (const path of [state, pluginRoot]) {
    await mkdir(path, { mode: 0o700 });
    await chown(path, uid, gid);
  }
  const manifest = {
    id: "systemd.fixture",
    version: "1",
    apiVersion: "kvmhelm.plugin/v1",
    kind: "feature",
    runtime: "external-process",
    entry: "index.mjs",
    permissions: ["ui:panel", "video:read"],
    uiSlots: ["kvm.sidepanel"],
  };
  const program = `let device,received=0;process.on('message',m=>{if(m.type==='initialize'){device=m.devices[0];process.send({type:'ready'});}if(m.type==='start')process.send({type:'subscribe',id:'video',source:'video',device_id:device});if(m.type==='health')process.send({type:'health'});if(m.type==='media'){if(!Buffer.isBuffer(m.chunk.data))process.exit(31);if(++received===1)process.send({type:'panel',panel:{id:'runtime',title:'Runtime',slot:'kvm.sidepanel',device_id:device,text:JSON.stringify({uid:process.getuid(),gid:process.getgid(),pid:process.pid,broker:process.ppid,version:process.version,received})}});}if(m.type==='dispose')process.exit(0);});`;
  for (const [name, data] of [
    ["kvmhelm.plugin.json", JSON.stringify(manifest)],
    ["index.mjs", program],
  ]) {
    const file = join(pluginRoot, name);
    await writeFile(file, data, { mode: 0o600 });
    await chown(file, uid, gid);
  }
  const executable = join(packageRoot, "kvmhelm");
  command(
    executable,
    [
      "token",
      "bootstrap",
      "--data",
      join(state, "data"),
      "--secrets",
      join(state, "secrets"),
    ],
    {
      uid,
      gid,
      env: { PATH: "", LANG: "C.UTF-8" },
    },
  );
  pat = (await readFile(join(state, "secrets/cli.pat"), "utf8")).trim();
  for (const name of [
    "data",
    "secrets",
    "secrets/master.key",
    "secrets/cli.pat",
    "data/config.sqlite",
  ]) {
    const info = await stat(join(state, name));
    assert.equal(info.uid, uid);
    assert.equal(info.mode & 0o777, info.isDirectory() ? 0o700 : 0o600);
  }
  await start();
  device = await api("/devices", "POST", {
    device: {
      name: "Unit simulator",
      driver_id: "simulator",
      transport: "simulator",
      keyboard_layout: "de",
      media_profile: "warm",
      minimum_action_latency_ms: 0,
    },
  });
  await api("/preferences", "PUT", {
    ...(await api("/preferences")),
    theme: "dark",
    sidebar: false,
  });
  await api("/plugins", "POST", {
    path: pluginRoot,
    devices: [device.device_id],
  });
  const first = await proof();
  const opened = await api("/tools/open_computer", "POST", {
    computer_id: device.device_id,
    mode: "observe",
    request_id: "before-restart",
  });
  assert.equal(opened.isError, false);
  gateway = "";
  logs = "";
  command("systemctl", ["restart", unit]);
  await ready();
  const restarted = await proof(first);
  assert.notEqual(restarted.daemon, first.daemon);
  await until(
    () => [first.pid, first.broker, first.daemon].every((pid) => !alive(pid)),
    "Restart reaps old helpers",
  );
  assert.deepEqual(await api("/sessions"), []);
  assert.equal((await api("/preferences")).theme, "dark");
  await stop();
  await start();
  const reopened = await proof(restarted);
  assert.notEqual(reopened.daemon, restarted.daemon);
  assert.equal((await api("/devices"))[0].device_id, device.device_id);
  assert.deepEqual(await api("/sessions"), []);
  await stop();
  console.log(
    JSON.stringify({
      result: "PASS",
      uid,
      gid,
      lifecycle:
        "transient systemd unit: start/restart/stop/reopen; sessions released; owned helpers reaped",
      policy: properties,
      data: "private POSIX state, existing PAT and packaged Node/Buffer IPC",
    }),
  );
} finally {
  if (launcher) await stop();
  assert.ok(
    resolve(fixture).startsWith("/run/kvmhelm-test-") &&
      packageRoot.startsWith(fixture + sep),
  );
  await rm(fixture, {
    recursive: true,
    force: true,
    maxRetries: 20,
    retryDelay: 100,
  });
}
