// Copied native binary with an empty PATH, synthetic Janus/KVMD and Opus/PCM.
// No actual KVM, microphone, speaker, or production state is used.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { copyFile, chmod, mkdtemp, readFile, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve, sep } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { WebSocket, WebSocketServer } from "ws";
import {
  RTCPeerConnection,
  RTCRtpCodecParameters,
  MediaStreamTrack,
  RtpHeader,
  RtpPacket,
} from "werift";
import OpusScript from "opusscript";
import { chromium, expect } from "@playwright/test";
import { localIceOnly } from "../dist/server/lan-ice.js";

assert.ok(process.env.KVMHELM_TEST_BINARY, "Set KVMHELM_TEST_BINARY");
const dir = await mkdtemp(join(tmpdir(), "kvmhelm-native-audio-"));
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
const args = ["--data", join(dir, "data"), "--secrets", join(dir, "secrets")],
  options = { cwd: dir, env, windowsHide: true };
let browser;
let daemon,
  gateway = "",
  pat = "",
  logs = "",
  connections = 0,
  nextHandle = 0,
  received = 0,
  nonSilent = false;
const watched = [],
  peers = new Map(),
  timers = new Map(),
  sockets = new Set(),
  errors = [];
const encoder = new OpusScript(48000, 2, OpusScript.Application.AUDIO),
  decoder = new OpusScript(48000, 1, OpusScript.Application.VOIP);
const pcm = Buffer.alloc(3840);
for (let i = 0; i < 960; i++)
  for (let c = 0; c < 2; c++)
    pcm.writeInt16LE(
      Math.round(12000 * Math.sin((i * 2 * Math.PI * 1000) / 48000)),
      i * 4 + c * 2,
    );
// A valid one-pixel PNG suffices for Core's opening observation.
const image = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+j63sAAAAASUVORK5CYII=",
  "base64",
);
const fixture = createServer((q, r) => {
  assert.equal(q.headers["x-kvmd-user"], "fixture");
  assert.equal(q.headers["x-kvmd-passwd"], "audio-only");
  if (q.url === "/api/streamer/snapshot") {
    r.setHeader("content-type", "image/png");
    return r.end(image);
  }
  if (q.url === "/streamer/stream") {
    r.writeHead(503);
    return r.end();
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
const janus = new WebSocketServer({ noServer: true, maxPayload: 65536 }),
  hid = new WebSocketServer({ noServer: true, maxPayload: 65536 });
fixture.on("upgrade", (q, s, h) => {
  assert.equal(q.headers["x-kvmd-user"], "fixture");
  assert.equal(q.headers["x-kvmd-passwd"], "audio-only");
  if (q.url === "/janus/ws") {
    assert.equal(q.headers["sec-websocket-protocol"], "janus-protocol");
    janus.handleUpgrade(q, s, h, (ws) => janus.emit("connection", ws));
  } else if (q.url === "/api/ws")
    hid.handleUpgrade(q, s, h, (ws) => hid.emit("connection", ws));
  else s.destroy();
});
hid.on("connection", (ws) =>
  ws.on("message", (raw) => {
    if (JSON.parse(raw.toString()).event_type === "ping")
      ws.send(JSON.stringify({ event_type: "pong", event: {} }));
  }),
);
async function discard(id) {
  clearInterval(timers.get(id));
  timers.delete(id);
  await peers.get(id)?.close();
  peers.delete(id);
}
janus.on("connection", (socket) => {
  const attempt = ++connections,
    owned = new Set();
  let chain = Promise.resolve();
  socket.on("close", () => {
    for (const id of owned) void discard(id);
  });
  socket.on("message", (raw) => {
    chain = chain
      .then(async () => {
        const m = JSON.parse(raw.toString()),
          id = m.handle_id;
        const send = (data) => {
          if (socket.readyState === 1) socket.send(JSON.stringify(data));
        };
        const success = (data = {}) =>
          send({ janus: "success", transaction: m.transaction, data });
        if (m.janus === "create") success({ id: attempt });
        if (m.janus === "attach") {
          assert.equal(m.plugin, "janus.plugin.ustreamer");
          const handle = ++nextHandle;
          owned.add(handle);
          success({ id: handle });
        }
        if (m.janus === "detach") {
          await discard(id);
          owned.delete(id);
          success();
        }
        if (m.janus === "destroy") {
          for (const handle of owned) await discard(handle);
          success();
          socket.close();
        }
        // The first unanswered probe must recover on the owned 1 s retry.
        if (m.body?.request === "features" && attempt > 1)
          send({
            janus: "event",
            sender: id,
            plugindata: {
              data: {
                result: {
                  status: "features",
                  features: { audio: true, mic: true },
                },
              },
            },
          });
        if (m.body?.request === "watch") {
          assert.ok(
            !watched.some((w) => w.id === id),
            "stopped Janus handles cannot be reused",
          );
          watched.push({ id, mic: m.body.params.mic });
          assert.deepEqual(m.body.params, {
            orientation: 0,
            audio: true,
            mic: m.body.params.mic,
            camera: false,
          });
          const peer = new RTCPeerConnection({
            iceServers: [],
            codecs: {
              audio: [
                new RTCRtpCodecParameters({
                  mimeType: "audio/opus",
                  clockRate: 48000,
                  channels: 2,
                  payloadType: 111,
                }),
              ],
            },
          });
          peers.set(id, peer);
          localIceOnly(peer);
          const track = new MediaStreamTrack({ kind: "audio" });
          peer.addTrack(track);
          peer.onTrack.subscribe((remote) =>
            remote.onReceiveRtp.subscribe((packet) => {
              received++;
              const data = decoder.decode(packet.payload);
              nonSilent ||= data.some((v) => v !== 0);
            }),
          );
          await peer.setLocalDescription(await peer.createOffer());
          send({
            janus: "event",
            sender: id,
            jsep: {
              type: "offer",
              sdp:
                peer.localDescription.sdp +
                "a=candidate:99 1 UDP 1 8.8.8.8 9000 typ host\r\n",
            },
          });
          let sequence = 0;
          timers.set(
            id,
            setInterval(() => {
              if (peer.connectionState === "connected") {
                sequence++;
                track.writeRtp(
                  new RtpPacket(
                    new RtpHeader({
                      payloadType: 111,
                      sequenceNumber: sequence % 65536,
                      timestamp: sequence * 960,
                      ssrc: 123,
                    }),
                    encoder.encode(pcm, 960),
                  ),
                );
              }
            }, 20),
          );
          send({
            janus: "event",
            sender: id - 1,
            jsep: { type: "offer", sdp: "invalid stale offer" },
          });
        }
        if (m.body?.request === "start") {
          const active = watched.find((w) => w.id === id);
          assert.ok(active);
          if (!active.mic)
            assert.ok(
              m.jsep.sdp.includes("a=recvonly"),
              "listen-only answer must not negotiate microphone transmission",
            );
          await peers.get(id).setRemoteDescription(m.jsep);
        }
        if (m.janus === "trickle" && !m.candidate.completed)
          await peers.get(id)?.addIceCandidate(m.candidate);
      })
      .catch((e) => errors.push(e));
  });
});
async function until(check, label, timeout = 12000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    if (errors.length) throw errors[0];
    if (await check()) return;
    await delay(20);
  }
  throw Error(label + " timed out; " + logs);
}
async function api(path, method = "GET", body, token = pat) {
  const response = await fetch(gateway + "/api/v1" + path, {
    method,
    headers: {
      authorization: "Bearer " + token,
      "content-type": "application/json",
      "x-kvmhelm-client": "audio-fixture",
    },
    body: body ? JSON.stringify(body) : undefined,
    signal: AbortSignal.timeout(15000),
  });
  const value = await response.json();
  assert.equal(response.status, 200, JSON.stringify(value));
  return value;
}
async function tool(name, body, token = pat) {
  const value = await api("/tools/" + name, "POST", body, token);
  assert.equal(value.isError, false, JSON.stringify(value));
  return value.structuredContent;
}
async function connect(sid, microphone = false, token = pat) {
  const ticket = await api(
    `/sessions/${sid}/audio`,
    "POST",
    { microphone },
    token,
  );
  const ws = new WebSocket(gateway.replace("http:", "ws:") + "/api/v1/audio", {
    origin: gateway,
  });
  sockets.add(ws);
  const messages = [];
  ws.on("message", (data, binary) => {
    messages.push(binary ? Buffer.from(data) : JSON.parse(data.toString()));
    if (messages.length > 128) messages.shift();
  });
  ws.on("close", () => sockets.delete(ws));
  await new Promise((resolve, reject) => {
    ws.once("open", resolve);
    ws.once("error", reject);
  });
  ws.send(JSON.stringify({ ticket: ticket.ticket }));
  await until(() => messages.some((m) => m.ready), "Audio ready");
  return { ws, messages };
}
async function tone(connection) {
  await until(
    () =>
      connection.messages.some(
        (m) => Buffer.isBuffer(m) && m.some((v) => v !== 0),
      ),
    "Non-silent decoded Opus",
  );
  assert.ok(
    connection.messages
      .filter(Buffer.isBuffer)
      .every((m) => m.length % 4 === 0 && m.length <= 23040),
  );
}
async function stop(connection) {
  if (connection.ws.readyState >= 2) return;
  const closed = new Promise((r) => connection.ws.once("close", r));
  connection.ws.close();
  await closed;
}
try {
  await copyFile(resolve(process.env.KVMHELM_TEST_BINARY), executable);
  await chmod(executable, 0o700);
  const bootstrap = spawnSync(executable, ["token", "bootstrap", ...args], {
    ...options,
    encoding: "utf8",
  });
  assert.equal(bootstrap.status, 0, bootstrap.stderr);
  pat = (await readFile(join(dir, "secrets", "cli.pat"), "utf8")).trim();
  daemon = spawn(
    executable,
    ["serve", "--host", "127.0.0.1", "--port", "0", ...args],
    { ...options, stdio: ["ignore", "ignore", "pipe"] },
  );
  daemon.stderr.on("data", (chunk) => {
    logs = (logs + chunk.toString()).slice(-8000);
    const match = logs.match(/KVMHelm listening on (http:\/\/[^\s]+)/);
    if (match) gateway = match[1];
  });
  await until(() => {
    if (daemon.exitCode !== null) throw Error(logs);
    return gateway;
  }, "Daemon startup");
  await new Promise((r) => fixture.listen(0, "127.0.0.1", r));
  const device = await api("/devices", "POST", {
    device: {
      name: "Synthetic Janus",
      driver_id: "pikvm-v4",
      address: `http://127.0.0.1:${fixture.address().port}`,
      media_profile: "warm",
      minimum_action_latency_ms: 0,
    },
    secret: { username: "fixture", password: "audio-only" },
  });
  const opened = await tool("open_computer", {
      computer_id: device.device_id,
      mode: "control",
      request_id: "open",
    }),
    sid = opened.session.session_id;
  await until(async () => {
    const devices = await api("/devices");
    return devices[0].capabilities.audio.from_target;
  }, "Probe retry recovery");
  assert.equal(connections, 2);
  assert.ok(
    (await api("/devices"))[0].capabilities.keyboard.hid,
    "audio probe failure must not break HID",
  );
  for (let round = 0; round < 3; round++) {
    const first = await connect(sid);
    await tone(first);
    const before = watched.length,
      other = await connect(sid);
    await tone(other);
    assert.equal(watched.length, before, "listeners share one target peer");
    await stop(other);
    first.messages.length = 0;
    await tone(first);
    await stop(first);
    await until(() => peers.size === 0, "Last listener detaches target peer");
  }
  assert.equal(watched.length, 3);
  const microphone = await connect(sid, true);
  await tone(microphone);
  const mono = Buffer.alloc(1920);
  for (let i = 0; i < 960; i++)
    mono.writeInt16LE(
      Math.round(10000 * Math.sin((i * 2 * Math.PI * 700) / 48000)),
      i * 2,
    );
  for (let i = 0; i < 50; i++) {
    microphone.ws.send(mono);
    await delay(20);
  }
  await until(
    () => received > 10 && nonSilent,
    "Target receives encoded microphone tone",
  );
  await tool("computer_control", {
    session_id: sid,
    operation: "release",
    request_id: "release",
  });
  await until(
    () => microphone.ws.readyState === WebSocket.CLOSED,
    "Control release closes microphone",
  );
  await delay(300);
  const after = received;
  await delay(200);
  assert.equal(received, after, "no microphone media after control loss");
  await until(() => peers.size === 0, "Control release detaches target peer");
  const reader = await connect(sid);
  await tone(reader);
  await stop(reader);
  await until(
    () => peers.size === 0,
    "Playback restarts after microphone stop",
  );
  const interrupted = await connect(sid);
  await tone(interrupted);
  for (const ws of janus.clients) ws.close();
  await until(
    () => interrupted.ws.readyState === WebSocket.CLOSED,
    "Janus disconnect closes old listeners",
  );
  await until(async () => {
    const devices = await api("/devices");
    return connections === 3 && devices[0].capabilities.audio.from_target;
  }, "Janus reconnect restores advertised audio");
  const recovered = await connect(sid);
  await tone(recovered);
  await stop(recovered);
  await until(() => peers.size === 0, "Recovered listener cleanup");
  await tool("close_computer", { session_id: sid });
  browser = await chromium.launch({
    headless: true,
    args: ["--mute-audio", "--autoplay-policy=no-user-gesture-required"],
  });
  const page = await browser.newPage();
  const pageErrors = [];
  page.on("pageerror", (error) => pageErrors.push(error.message));
  await page.addInitScript(() => {
    // Exercise the existing Vue microphone UI with a generated stream. Never
    // delegate to the browser's physical capture implementation.
    Object.defineProperty(navigator.mediaDevices, "enumerateDevices", {
      value: async () => [],
    });
    Object.defineProperty(navigator.mediaDevices, "getUserMedia", {
      value: async () => {
        const context = new AudioContext({ sampleRate: 48000 }),
          oscillator = context.createOscillator(),
          destination = context.createMediaStreamDestination();
        oscillator.frequency.value = 700;
        oscillator.connect(destination);
        oscillator.start();
        await context.resume();
        const stream = destination.stream,
          track = stream.getAudioTracks()[0],
          stop = track.stop.bind(track);
        track.stop = () => {
          stop();
          void context.close();
        };
        return stream;
      },
    });
  });
  await page.goto(gateway + "/computers");
  await page.getByLabel("Personal Access Token").fill(pat);
  await page.getByRole("button", { name: "Anmelden →", exact: true }).click();
  await page.getByRole("button", { name: "Ziel hören", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "Ton ausschalten", exact: true }),
  ).toBeVisible();
  await expect
    .poll(() => page.getByLabel("Zielaudiopegel").evaluate((el) => el.value))
    .toBeGreaterThan(0);
  await page
    .getByRole("button", { name: "Steuerung übernehmen", exact: true })
    .click();
  await expect(
    page.getByRole("button", { name: "Steuerung freigeben", exact: true }),
  ).toBeVisible();
  const beforeUiMic = received;
  await page
    .getByRole("button", { name: "Mikrofon einschalten", exact: true })
    .click();
  await expect(
    page.getByRole("button", { name: "● Mikrofon ausschalten", exact: true }),
  ).toBeVisible();
  await until(
    () => received > beforeUiMic + 5,
    "Existing Vue UI transmits synthetic microphone",
  );
  await page.evaluate(() => window.dispatchEvent(new Event("blur")));
  await expect(
    page.getByRole("button", { name: "Mikrofon einschalten", exact: true }),
  ).toBeVisible();
  await until(
    () => watched.at(-1)?.mic === false,
    "Focus loss keeps playback while stopping microphone",
  );
  await page.getByLabel("Mikrofonmodus").selectOption("hold");
  const hold = page.getByRole("button", {
    name: "Zum Sprechen halten",
    exact: true,
  });
  const box = await hold.boundingBox();
  assert.ok(box);
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  const beforeHold = received;
  await page.mouse.down();
  try {
    await expect(
      page.getByRole("button", {
        name: "● Loslassen zum Stummschalten",
        exact: true,
      }),
    ).toBeVisible();
    await until(
      () => received > beforeHold + 3,
      "Vue push-to-talk sends while held",
    );
  } finally {
    await page.mouse.up();
  }
  await expect(hold).toBeVisible();
  await until(
    () => watched.at(-1)?.mic === false,
    "Push-to-talk release stops microphone",
  );
  await expect(
    page.getByRole("button", { name: "Ton ausschalten", exact: true }),
  ).toBeVisible();
  await page
    .getByRole("button", { name: "Ton ausschalten", exact: true })
    .click();
  await page.close();
  await browser.close();
  browser = undefined;
  await until(() => peers.size === 0, "Vue disposal stops target audio");
  assert.deepEqual(pageErrors, []);
  await api("/devices/" + device.device_id, "DELETE", {
    revision: device.revision,
    confirmed: true,
  });
  await until(
    () => janus.clients.size === 0 && hid.clients.size === 0,
    "Driver disposal closes owned signaling",
  );
  // The explicit Comet adapters retain their manufacturer/model identity and
  // delegate only the actually supported GL-KVMD/Janus audio protocol.
  for (const model of ["GL-RM1", "GL-RM10"]) {
    const comet = await api("/devices", "POST", {
      device: {
        name: "Synthetic " + model,
        driver_id: "glinet-comet",
        model,
        address: `http://127.0.0.1:${fixture.address().port}`,
        media_profile: "warm",
        minimum_action_latency_ms: 0,
      },
      secret: { username: "fixture", password: "audio-only" },
    });
    const state = await tool("open_computer", {
      computer_id: comet.device_id,
      mode: "control",
      request_id: "open-" + model,
    });
    await until(async () => {
      const devices = await api("/devices");
      const caps = devices[0].capabilities;
      return (
        caps.audio.from_target &&
        caps.audio.to_target &&
        caps.extensions.model === model
      );
    }, model + " feature response");
    const channel = await connect(state.session.session_id, true);
    await tone(channel);
    const before = received;
    for (let i = 0; i < 15; i++) {
      channel.ws.send(mono);
      await delay(20);
    }
    await until(() => received > before + 3, model + " microphone tone");
    await stop(channel);
    await until(() => peers.size === 0, model + " audio stop");
    await tool("close_computer", { session_id: state.session.session_id });
    await api("/devices/" + comet.device_id, "DELETE", {
      revision: comet.revision,
      confirmed: true,
    });
    await until(
      () => janus.clients.size === 0 && hid.clients.size === 0,
      model + " disposal",
    );
  }
  assert.deepEqual(errors, []);
  console.log(
    "Native audio: empty PATH, Janus probe retry, fresh handles, shared listeners, stereo Opus decode, mono microphone encode, existing Vue audio/microphone UI, control/focus loss and complete cleanup passed",
  );
} finally {
  await browser?.close();
  if (gateway && daemon?.exitCode === null)
    for (const device of await api("/devices").catch(() => []))
      await api("/devices/" + device.device_id, "DELETE", {
        revision: device.revision,
        confirmed: true,
      }).catch(() => {});
  for (const ws of sockets) ws.terminate();
  for (const timer of timers.values()) clearInterval(timer);
  for (const peer of peers.values()) await peer.close();
  encoder.delete();
  decoder.delete();
  for (const wss of [janus, hid]) {
    for (const ws of wss.clients) ws.terminate();
    wss.close();
  }
  fixture.closeAllConnections();
  if (fixture.listening) await new Promise((r) => fixture.close(r));
  if (daemon?.exitCode === null) {
    const ended = new Promise((r) => daemon.once("exit", r));
    daemon.kill();
    await ended;
  }
  assert.ok(resolve(dir).startsWith(resolve(tmpdir()) + sep));
  await rm(dir, { recursive: true, force: true });
}
