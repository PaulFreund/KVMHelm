// Actual extracted release, original SQLite/Secrets/Auth/Core, optional helpers.
// All state and input belong to this simulator fixture. Never installs a service.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { cp, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { basename, dirname, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const script = fileURLToPath(import.meta.url);
const suffix = process.platform === "win32" ? ".exe" : "";
const buildEnv = Object.fromEntries(
  Object.entries(process.env).filter(
    ([key]) => !key.toUpperCase().startsWith("KVMHELM_"),
  ),
);
const cleanEnv = Object.fromEntries(
  Object.entries(buildEnv).filter(
    ([key]) => !["HOME", "USERPROFILE"].includes(key.toUpperCase()),
  ),
);
async function hash(path) {
  const value = createHash("sha256");
  for await (const bytes of createReadStream(path)) value.update(bytes);
  return value.digest("hex");
}
function command(program, args, options = {}) {
  const result = spawnSync(program, args, {
    cwd: root,
    env: buildEnv,
    windowsHide: true,
    encoding: "utf8",
    maxBuffer: 8 * 1024 * 1024,
    timeout: 240000,
    ...options,
  });
  assert.equal(
    result.status,
    0,
    `${basename(program)}: ${result.stderr || result.error?.message}`,
  );
  return result.stdout;
}

async function legacy(mode, directory) {
  const { Store, Secrets } = await import("../dist/server/store.js");
  const { Auth } = await import("../dist/server/auth.js");
  const { Core } = await import("../dist/server/core.js");
  const { scopes } = await import("../dist/shared/contracts.js");
  const { manifestSchema } = await import("../dist/server/plugins.js");
  await mkdir(join(directory, "data"), { recursive: true, mode: 0o700 });
  const store = new Store(join(directory, "data"));
  const secrets = await Secrets.open(join(directory, "secrets"));
  const auth = new Auth(store);
  let core;
  try {
    await auth.load();
    core = new Core(store, secrets, auth);
    await core.load();
    if (mode === "prepare") {
      const token = await auth.create({
        name: "reference owner",
        scopes: [...scopes],
        devices: ["*"],
      });
      const device = await core.saveDevice(
        auth.authenticate(token.token, "reference-client"),
        {
          name: "Migration simulator",
          driver_id: "simulator",
          transport: "simulator",
          keyboard_layout: "de",
          media_profile: "warm",
          minimum_action_latency_ms: 0,
          tags: ["migration"],
        },
        { username: "fixture", password: "Grüße 🙂" },
      );
      const record = await store.get("devices", device.device_id);
      const preferences = await store.put("preferences", "owner", {
        theme: "dark",
        sidebar: true,
        selected: device.device_id,
        order: [device.device_id],
        visible: [device.device_id],
        tile_size: 360,
      });
      const plugin = join(dirname(directory), "fixture-plugin");
      const manifest = manifestSchema.parse({
        id: "package.fixture",
        version: "1",
        apiVersion: "kvmhelm.plugin/v1",
        kind: "feature",
        runtime: "external-process",
        entry: "index.mjs",
        permissions: ["ui:panel", "video:read"],
        uiSlots: ["kvm.sidepanel"],
      });
      await mkdir(plugin, { mode: 0o700 });
      await writeFile(
        join(plugin, "kvmhelm.plugin.json"),
        JSON.stringify(manifest),
      );
      await writeFile(
        join(plugin, "index.mjs"),
        `
let device, own, received=0;
const publish=()=>process.send({type:'panel',panel:{id:'runtime',title:'Packaged runtime',slot:'kvm.sidepanel',device_id:device,text:JSON.stringify({execPath:process.execPath,version:process.version,pid:process.pid,broker:process.ppid,own,received,leaked:!!process.env.KVMHELM_SYNTHETIC_SECRET}),actions:[]}});
process.on('message',m=>{
 if(m.type==='initialize'){device=m.devices[0];own=m.data_path;process.send({type:'ready'});publish();}
 if(m.type==='start')process.send({type:'subscribe',id:'video',source:'video',device_id:device});
 if(m.type==='media'){if(!Buffer.isBuffer(m.chunk.data)||m.chunk.data[0]!==137)process.exit(31);if(++received===1)publish();}
 if(m.type==='health')process.send({type:'health'});
 if(m.type==='dispose')process.exit(0);
});
`,
      );
      await store.put("plugins", manifest.id, {
        id: manifest.id,
        manifest,
        path: plugin,
        enabled: false,
        devices: [device.device_id],
      });
      await writeFile(
        join(directory, "expected.json"),
        JSON.stringify({
          device: record,
          preferences,
          token: token.token,
        }),
        { mode: 0o600 },
      );
    } else if (mode === "verify-backup") {
      const expected = JSON.parse(
        await readFile(join(directory, "expected.json"), "utf8"),
      );
      assert.deepEqual(
        await store.get("devices", expected.device.device_id),
        expected.device,
      );
      assert.deepEqual(
        await store.get("preferences", "owner"),
        expected.preferences,
      );
      assert.deepEqual(await secrets.get(expected.device.secret_ref), {
        username: "fixture",
        password: "Grüße 🙂",
      });
      auth.check(
        auth.authenticate(expected.token, "backup-client"),
        "devices:read",
        expected.device.device_id,
      );
      assert.equal((await store.list("plugins"))[0].enabled, false);
    } else {
      assert.equal(mode, "verify");
      const expected = JSON.parse(
        await readFile(join(directory, "expected.json"), "utf8"),
      );
      const saved = await store.get("devices", expected.device.device_id);
      assert.equal(saved.name, "Native update");
      assert.equal(saved.revision, expected.device.revision + 1);
      assert.deepEqual(await secrets.get(saved.secret_ref), {
        username: "native",
        password: "updated 🙂",
      });
      assert.equal((await store.get("preferences", "owner")).theme, "light");
      const nativeToken = await readFile(join(directory, "native.pat"), "utf8");
      auth.check(
        auth.authenticate(nativeToken, "rollback-client"),
        "devices:read",
        saved.device_id,
      );
      auth.check(
        auth.authenticate(expected.token, "original-client"),
        "devices:read",
        saved.device_id,
      );
      const plugins = await store.list("plugins");
      assert.equal(plugins[0].enabled, true);
      assert.equal(plugins[0].manifest.id, "package.fixture");
      assert.equal(core.sessions.size, 0);
    }
  } finally {
    await core?.shutdown();
    await store.close();
  }
}
if (process.argv[2] === "--legacy") {
  await legacy(process.argv[3], resolve(process.argv[4]));
} else {
  assert.ok(
    process.env.KVMHELM_TEST_PACKAGE_ARCHIVE,
    "Set KVMHELM_TEST_PACKAGE_ARCHIVE",
  );
  const archive = resolve(process.env.KVMHELM_TEST_PACKAGE_ARCHIVE);
  const parent = join(root, "target/package-tests");
  await mkdir(parent, { recursive: true });
  const temporary = await mkdtemp(join(parent, "run-"));
  let daemon,
    gateway,
    logs = "";
  const owned = new Set();
  async function until(check, label, timeout = 20000) {
    const deadline = Date.now() + timeout;
    while (Date.now() < deadline) {
      if (await check()) return;
      await delay(40);
    }
    throw Error(label + " timed out: " + logs);
  }
  function alive(pid) {
    try {
      process.kill(pid, 0);
      return true;
    } catch (error) {
      if (error.code === "ESRCH") return false;
      throw error;
    }
  }
  async function stop() {
    const child = daemon;
    daemon = undefined;
    if (child && child.exitCode === null && child.signalCode === null) {
      const exited = new Promise((done) => child.once("exit", done));
      child.kill();
      await Promise.race([
        exited,
        delay(15000).then(() => {
          throw Error("Daemon stop timeout");
        }),
      ]);
    }
    await until(
      () => [...owned].every((pid) => !alive(pid)),
      "Owned helper cleanup",
    );
    owned.clear();
  }
  try {
    const checksum = (await readFile(archive + ".sha256", "utf8")).split(
      " ",
    )[0];
    assert.match(checksum, /^[0-9a-f]{64}$/);
    assert.equal(await hash(archive), checksum);
    const prefix = basename(archive).replace(/\.tar\.gz$/, "");
    const listing = command("tar", ["-tzf", archive]).trim().split(/\r?\n/);
    assert.ok(listing.length > 100 && listing.length < 10000);
    for (const path of listing) {
      assert.ok(path.startsWith(prefix + "/") && !/[\\\r\n]/.test(path));
      assert.ok(!path.split("/").some((p) => p === ".." || !p));
    }
    assert.ok(
      command("tar", ["-tvzf", archive])
        .trim()
        .split(/\r?\n/)
        .every((line) => line.startsWith("-")),
      "Only regular files may be extracted",
    );
    command("tar", ["-xzf", archive, "-C", temporary]);
    const packageRoot = join(temporary, prefix);
    const manifest = JSON.parse(
      await readFile(join(packageRoot, "package.json"), "utf8"),
    );
    assert.equal(manifest.profile, "full");
    assert.deepEqual(manifest.helpers, [
      { name: "ffmpeg", version: "6.1.1" },
      { name: "node", version: "24.18.0" },
    ]);
    assert.ok(
      !manifest.files.some((f) => /(^|\/)(node_modules|server)\//.test(f.path)),
    );
    const notices = JSON.parse(
      await readFile(join(packageRoot, "THIRD-PARTY.json"), "utf8"),
    );
    assert.ok(notices.cargo.length > 100 && notices.web.packages.length > 0);
    const verify = () =>
      process.platform === "win32"
        ? spawnSync(
            "powershell.exe",
            [
              "-NoProfile",
              "-ExecutionPolicy",
              "Bypass",
              "-File",
              join(packageRoot, "verify.ps1"),
            ],
            { encoding: "utf8", windowsHide: true },
          )
        : spawnSync("sh", [join(packageRoot, "verify.sh")], {
            encoding: "utf8",
          });
    let checked = verify();
    assert.equal(checked.status, 0, checked.stderr);
    const readme = await readFile(join(packageRoot, "README.txt"));
    await writeFile(join(packageRoot, "README.txt"), "modified");
    assert.notEqual(
      verify().status,
      0,
      "Payload modification must fail verification",
    );
    await writeFile(join(packageRoot, "README.txt"), readme);
    const binary = join(packageRoot, "kvmhelm" + suffix);
    const repeat = [];
    for (const cycle of [1, 2]) {
      const out = join(temporary, "repeat-" + cycle);
      command(
        process.execPath,
        [
          join(root, "scripts/package-native.mjs"),
          "--target",
          manifest.target,
          "--profile",
          "full",
          "--binary",
          binary,
          "--out",
          out,
        ],
        {
          env: {
            ...buildEnv,
            SOURCE_DATE_EPOCH: String(manifest.source.epoch),
          },
        },
      );
      repeat.push(await hash(join(out, basename(archive))));
    }
    assert.equal(
      repeat[0],
      repeat[1],
      "Same supplied binary/inputs must assemble identically",
    );
    const reference = join(temporary, "reference");
    command(process.execPath, [script, "--legacy", "prepare", reference]);
    // Close the original store first, then back up the entire fixture state.
    const working = join(temporary, "working");
    await cp(reference, working, { recursive: true, errorOnExist: true });
    const expected = JSON.parse(
      await readFile(join(reference, "expected.json"), "utf8"),
    );
    const state = [
      "--data",
      join(working, "data"),
      "--secrets",
      join(working, "secrets"),
    ];
    async function api(path, method = "GET", body) {
      const response = await fetch(gateway + "/api/v1" + path, {
        method,
        headers: {
          authorization: "Bearer " + expected.token,
          "content-type": "application/json",
        },
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: AbortSignal.timeout(12000),
      });
      const value = await response.json();
      assert.equal(response.status, 200, JSON.stringify(value));
      return value;
    }
    async function runtime() {
      const plugin = (await api("/plugins"))[0];
      const panel = plugin.panels.find((p) => p.id === "runtime");
      return panel ? JSON.parse(panel.text) : undefined;
    }
    for (const cycle of [1, 2]) {
      const versionDirectory = join(temporary, "installation-" + cycle);
      await cp(packageRoot, versionDirectory, {
        recursive: true,
        errorOnExist: true,
      });
      gateway = "";
      logs = "";
      daemon = spawn(
        join(versionDirectory, "kvmhelm" + suffix),
        ["serve", "--host", "127.0.0.1", "--port", "0", ...state],
        {
          cwd: temporary,
          env: {
            ...cleanEnv,
            PATH: "",
            KVMHELM_SYNTHETIC_SECRET: "must-not-reach-plugin",
          },
          windowsHide: true,
          stdio: ["ignore", "ignore", "pipe"],
        },
      );
      daemon.stderr.on("data", (bytes) => {
        logs = (logs + bytes.toString()).slice(-8000);
        gateway =
          logs.match(/KVMHelm listening on (http:\/\/[^\s]+)/)?.[1] ?? gateway;
      });
      await until(() => {
        if (daemon.exitCode !== null)
          throw Error("Native startup failed: " + logs);
        return gateway;
      }, "Extracted package startup");
      assert.equal(
        (await fetch(gateway + "/readyz").then((r) => r.json())).ok,
        true,
      );
      const devices = await api("/devices");
      assert.equal(devices[0].device_id, expected.device.device_id);
      assert.equal(devices[0].revision, expected.device.revision);
      assert.deepEqual(await api("/preferences"), expected.preferences);
      assert.deepEqual(await api("/sessions"), []);
      if (cycle === 1) {
        assert.equal((await api("/plugins"))[0].enabled, false);
        await api("/plugins/package.fixture/enable", "POST", {});
      }
      await until(
        async () => (await runtime())?.received > 0,
        "Packaged Node advanced Buffer IPC",
      );
      const running = await runtime();
      owned.add(running.pid);
      owned.add(running.broker);
      assert.equal(
        resolve(running.execPath),
        resolve(join(versionDirectory, "node" + suffix)),
      );
      assert.equal(running.version, "v24.18.0");
      assert.equal(running.leaked, false);
      assert.ok(
        resolve(running.own).startsWith(
          resolve(join(working, "data/plugins")) + sep,
        ),
      );
      if (cycle === 2) {
        await api("/preferences", "PUT", {
          ...expected.preferences,
          theme: "light",
        });
        const updated = await api("/devices", "POST", {
          device: { ...expected.device, name: "Native update" },
          secret: { username: "native", password: "updated 🙂" },
        });
        assert.equal(updated.revision, expected.device.revision + 1);
        const token = await api("/tokens", "POST", {
          name: "native rollback token",
          scopes: ["devices:read"],
          devices: [expected.device.device_id],
        });
        await writeFile(join(working, "native.pat"), token.token, {
          mode: 0o600,
        });
      }
      await stop();
    }
    command(process.execPath, [script, "--legacy", "verify", working]);
    command(process.execPath, [script, "--legacy", "verify-backup", reference]);
    // The untouched backup remains readable by the old implementation as well.
    assert.deepEqual(
      JSON.parse(await readFile(join(reference, "expected.json"), "utf8")),
      expected,
    );
    console.log(
      JSON.stringify({
        result: "PASS",
        target: manifest.target,
        packageFiles: listing.length,
        deterministicAssembly: repeat[0],
        migration:
          "original TypeScript Store/Auth/Core -> extracted native daemon -> original Core",
        restart:
          process.platform === "win32"
            ? "process termination/WAL recovery (SCM requires separate CI test)"
            : "SIGTERM cleanup",
        helpers: "packaged Node with empty PATH and real Buffer media IPC",
      }),
    );
  } finally {
    await stop();
    assert.ok(
      resolve(temporary).startsWith(resolve(parent) + sep) &&
        basename(temporary).startsWith("run-"),
    );
    await rm(temporary, {
      recursive: true,
      force: true,
      maxRetries: 40,
      retryDelay: 100,
    });
  }
}
