// Controlled loopback KVMD inputs and identical pre-encoded 1280x720 JPEGs.
// No user state, device, microphone, desktop, or clipboard is accessed.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createServer } from "node:http";
import { mkdir, mkdtemp, readFile, rm } from "node:fs/promises";
import { dirname, join, resolve, sep } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { WebSocketServer } from "ws";
import sharp from "sharp";
import {
  benchmark,
  reference,
  warmup,
  phase,
  traffic,
  stream,
  save,
  checked,
  digest,
  installMeasurementPlugin,
  normalizeBenchmarkEnvironment,
} from "./runtime-measurements.mjs";

assert.ok(
  benchmark && process.env.KVMHELM_TEST_BINARY,
  "Set KVMHELM_BENCH_OUTPUT and KVMHELM_TEST_BINARY",
);
const root = resolve("target/runtime-measurements");
await mkdir(root, { recursive: true });
const dir = await mkdtemp(join(root, "state-"));
const binary = resolve(process.env.KVMHELM_TEST_BINARY);
const helper = resolve(
  process.env.KVMHELM_TEST_HELPER_DIRECTORY ?? dirname(binary),
);
const node = resolve(
  process.env.KVMHELM_BENCH_NODE ?? join(helper, "node.exe"),
);
const executable = reference ? node : binary;
const prefix = reference ? [resolve("dist/server/cli.js")] : [];
const env = Object.fromEntries(
  Object.entries(process.env).filter(
    ([k]) => !k.toUpperCase().startsWith("KVMHELM_"),
  ),
);
delete env.HOME;
delete env.USERPROFILE;
normalizeBenchmarkEnvironment(env);
if (!reference) env.PATH = "";
env.KVMHELM_NODE = node;
const options = { cwd: dir, env, windowsHide: true };
const state = ["--data", join(dir, "data"), "--secrets", join(dir, "secrets")];
let daemon,
  url,
  pat,
  logs = "";
let snapshots = 0,
  hidPackets = 0;
const pixels = Buffer.alloc(1280 * 720 * 3);
let seed = 123456789;
for (let i = 0; i < pixels.length; i++) {
  seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
  pixels[i] = seed >>> 24;
}
const image = await sharp(pixels, {
  raw: { width: 1280, height: 720, channels: 3 },
})
  .jpeg({ quality: 75 })
  .toBuffer();
const fixture = createServer((q, r) => {
  assert.equal(q.headers["x-kvmd-user"], "fixture");
  assert.equal(q.headers["x-kvmd-passwd"], "measurement-only");
  if (q.url === "/api/streamer/snapshot") {
    snapshots++;
    r.writeHead(200, {
      "content-type": "image/jpeg",
      "content-length": image.length,
    });
    r.end(image);
    return;
  }
  if (q.url === "/streamer/stream") {
    r.writeHead(503);
    r.end();
    return;
  }
  r.setHeader("content-type", "application/json");
  r.end(
    JSON.stringify({
      ok: true,
      result:
        q.url === "/api/hid"
          ? { keyboard: { online: true }, mouse: { absolute: true } }
          : {},
    }),
  );
});
const sockets = new WebSocketServer({ noServer: true });
fixture.on("upgrade", (q, s, h) => {
  assert.equal(q.headers["x-kvmd-passwd"], "measurement-only");
  sockets.handleUpgrade(q, s, h, (ws) =>
    ws.on("message", (bytes) => {
      const m = JSON.parse(bytes.toString());
      if (q.url === "/janus/ws") {
        if (m.janus === "create" || m.janus === "attach")
          ws.send(
            JSON.stringify({
              janus: "success",
              transaction: m.transaction,
              data: { id: m.janus === "create" ? 1 : 2 },
            }),
          );
        if (m.body?.request === "features")
          ws.send(
            JSON.stringify({
              janus: "event",
              sender: m.handle_id,
              plugindata: {
                data: {
                  result: {
                    status: "features",
                    features: { audio: false, mic: false },
                  },
                },
              },
            }),
          );
      } else if (m.event_type === "ping")
        ws.send(JSON.stringify({ event_type: "pong", event: {} }));
      else hidPackets++;
    }),
  );
});
async function api(path, method = "GET", body) {
  const response = await fetch(url + "/api/v1" + path, {
    method,
    headers: {
      authorization: "Bearer " + pat,
      "content-type": "application/json",
      "x-kvmhelm-client": "runtime-benchmark",
    },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(15000),
  });
  const value = await response.json();
  assert.equal(response.status, 200, JSON.stringify(value));
  return value;
}
const tool = (name, args) => api("/tools/" + name, "POST", args);
async function createDevice(number) {
  return api("/devices", "POST", {
    device: {
      name: "Measurement " + number,
      driver_id: "pikvm-v4",
      address: `http://127.0.0.1:${fixture.address().port}`,
      media_profile: "warm",
      minimum_action_latency_ms: 0,
      keyboard_layout: "de",
    },
    secret: { username: "fixture", password: "measurement-only" },
  });
}
const reports = [];
try {
  const bootstrap = spawnSync(
    executable,
    [...prefix, "token", "bootstrap", ...state],
    { ...options, encoding: "utf8" },
  );
  assert.equal(bootstrap.status, 0, bootstrap.stderr);
  pat = (await readFile(join(dir, "secrets/cli.pat"), "utf8")).trim();
  const reservation = createServer();
  await new Promise((r) => reservation.listen(0, "127.0.0.1", r));
  const port = reservation.address().port;
  await new Promise((r) => reservation.close(r));
  url = `http://127.0.0.1:${port}`;
  daemon = spawn(
    executable,
    [
      ...prefix,
      "serve",
      "--host",
      "127.0.0.1",
      "--port",
      String(port),
      ...state,
    ],
    { ...options, stdio: ["ignore", "ignore", "pipe"] },
  );
  daemon.stderr.on("data", (b) => {
    logs = (logs + b).slice(-6000);
  });
  let ready = false;
  for (let i = 0; i < 300; i++) {
    if (daemon.exitCode !== null) throw Error(logs);
    if (
      await fetch(url + "/readyz")
        .then((r) => r.ok)
        .catch(() => false)
    ) {
      ready = true;
      break;
    }
    await delay(100);
  }
  assert.ok(ready, logs);
  await new Promise((r) => fixture.listen(0, "127.0.0.1", r));
  assert.deepEqual(await api("/devices"), []);
  await delay(warmup * 1000);
  reports.push(await phase("idle_empty", daemon.pid, (ms) => delay(ms)));
  for (const name of [
    "active_kvmd",
    "wall_four_kvmd",
    "slow_eight_clients",
    "video_plugin",
  ]) {
    const devices = [],
      sessions = [],
      readers = [];
    try {
      for (let n = 0; n < (name === "wall_four_kvmd" ? 4 : 1); n++) {
        const device = await createDevice(n);
        devices.push(device);
        const response = await tool("open_computer", {
          computer_id: device.device_id,
          mode: n === 0 ? "control" : "observe",
          profile: ["wall_four_kvmd", "slow_eight_clients"].includes(name)
            ? "overview"
            : "active",
          request_id: name + "-open-" + n,
        });
        const opened = checked(response);
        sessions.push(opened);
        const encoded = response.content.find((c) => c.type === "image");
        assert.equal(
          digest(Buffer.from(encoded.data, "base64")),
          digest(image),
          "Both runtimes must deliver the identical fixture JPEG",
        );
        if (["wall_four_kvmd", "slow_eight_clients"].includes(name))
          readers.push(await stream(url, pat, opened.session.session_id));
      }
      if (name === "slow_eight_clients")
        for (let n = 0; n < 8; n++)
          readers.push(
            await stream(url, pat, sessions[0].session.session_id, true),
          );
      if (name === "video_plugin")
        await installMeasurementPlugin(
          api,
          join(dir, "video-plugin"),
          devices[0].device_id,
        );
      const session = sessions[0].session.session_id;
      const warm = await traffic({
        tool,
        session,
        frame: sessions[0].frames[0],
        ms: warmup * 1000 || 1,
      });
      for (const reader of readers) {
        reader.result.bytes = 0;
        reader.result.boundaries = 0;
      }
      let before;
      const report = await phase(name, daemon.pid, (ms) => {
        before = { snapshots, hidPackets };
        for (const reader of readers) {
          reader.result.bytes = 0;
          reader.result.boundaries = 0;
        }
        return traffic({ tool, session, frame: warm.frame, ms });
      });
      report.fixture = {
        snapshots: snapshots - before.snapshots,
        hid_packets: hidPackets - before.hidPackets,
        streams: readers.map((r) => ({ ...r.result })),
      };
      if (readers.length)
        assert.ok(
          readers[0].result.bytes > image.length * 10,
          "The healthy reader must continue receiving complete frames",
        );
      if (name === "video_plugin")
        assert.ok(report.process_names.includes("node"));
      reports.push(report);
    } finally {
      for (const reader of readers) reader.close();
      if (name === "video_plugin")
        await api("/plugins/benchmark.observer/uninstall", "POST", {});
      for (const session of sessions)
        checked(
          await tool("close_computer", {
            session_id: session.session.session_id,
          }),
        );
      for (const device of devices)
        await api("/devices/" + device.device_id, "DELETE", {
          revision: device.revision,
          confirmed: true,
        });
    }
  }
  await save({
    scenario_group: "core",
    fixture: {
      width: 1280,
      height: 720,
      mime: "image/jpeg",
      bytes: image.length,
      sha256: digest(image),
      cadence_hz: 4,
    },
    reports,
  });
} finally {
  for (const ws of sockets.clients) ws.terminate();
  sockets.close();
  fixture.closeAllConnections();
  if (fixture.listening) await new Promise((r) => fixture.close(r));
  if (daemon?.exitCode === null && daemon.signalCode === null) {
    const ended = new Promise((r) => daemon.once("exit", r));
    daemon.kill();
    await ended;
  }
  assert.ok(resolve(dir).startsWith(root + sep));
  await rm(dir, {
    recursive: true,
    force: true,
    maxRetries: 40,
    retryDelay: 100,
  });
}
