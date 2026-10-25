// Native Core <-> actual Chromium WebRTC H.264/data channel. All devices,
// credentials, input and media are synthetic; no physical device is contacted.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { copyFile, chmod, mkdtemp, readFile, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve, sep } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { chromium } from "playwright";
import { WebSocketServer, WebSocket } from "ws";
import { JetKvmDriver } from "../dist/server/drivers/jetkvm.js";
import { deviceSchema } from "../dist/shared/contracts.js";

assert.ok(process.env.KVMHELM_TEST_BINARY, "Set KVMHELM_TEST_BINARY");
const dir = await mkdtemp(join(tmpdir(), "kvmhelm-native-jet-"));
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
const helperDirectory = process.env.KVMHELM_TEST_HELPER_DIRECTORY;
if (!helperDirectory) {
  env.KVMHELM_FFMPEG = createRequire(import.meta.url)("ffmpeg-static");
  assert.ok(env.KVMHELM_FFMPEG, "Fixture FFmpeg required");
}
const args = ["--data", join(dir, "data"), "--secrets", join(dir, "secrets")];
const options = { cwd: dir, env, windowsHide: true };
const lanOnly = process.env.KVMHELM_TEST_ICE_LAN === "1";
let daemon,
  browser,
  gateway = "",
  pat = "",
  logins = 0,
  connections = 0;
let daemonLogs = "";
let mode = "normal",
  dropInputReply = false,
  dropped = 0;
const calls = [],
  pages = new Set();
const fixture = createServer(async (q, r) => {
  r.setHeader("content-type", "application/json");
  if (q.url === "/device/status")
    return r.end(JSON.stringify({ isSetup: mode !== "setup" }));
  if (q.url === "/auth/login-local") {
    let text = "";
    for await (const data of q) {
      text += data;
      assert.ok(text.length < 4096);
    }
    if (JSON.parse(text).password !== "fixture-only") {
      r.writeHead(401);
      return r.end("{}");
    }
    logins++;
    r.setHeader("set-cookie", [
      "unrelated=ignore; Path=/",
      "authToken=fixture-token; HttpOnly; Path=/",
    ]);
    return r.end("{}");
  }
  if (q.headers.cookie !== "authToken=fixture-token") {
    r.writeHead(401);
    return r.end("{}");
  }
  if (q.url === "/device")
    return r.end(JSON.stringify({ authMode: "password" }));
  r.writeHead(404);
  r.end("{}");
});
const wss = new WebSocketServer({ noServer: true, maxPayload: 65536 });
fixture.on("upgrade", (q, s, h) => {
  if (
    q.url !== "/webrtc/signaling/client" ||
    q.headers.cookie !== "authToken=fixture-token"
  )
    return s.destroy();
  wss.handleUpgrade(q, s, h, (ws) => wss.emit("connection", ws, q));
});
wss.on("connection", (ws) => {
  connections++;
  const selectedMode = mode;
  let page,
    chain = Promise.resolve();
  ws.on("error", () => {});
  ws.on("close", () => {
    if (page) {
      pages.delete(page);
      void page.close().catch(() => {});
    }
  });
  ws.on("message", (raw) => {
    chain = chain
      .then(async () => {
        const message = JSON.parse(raw.toString());
        if (message.type === "offer") {
          if (selectedMode === "public-candidate") {
            ws.send(
              JSON.stringify({
                type: "new-ice-candidate",
                data: {
                  candidate: "candidate:fixture 1 udp 1 8.8.8.8 443 typ host",
                  sdpMid: "0",
                  sdpMLineIndex: 0,
                },
              }),
            );
            return;
          }
          page = await browser.newPage();
          pages.add(page);
          await page.exposeFunction("candidate", (data) => {
            if (lanOnly && data.candidate.split(/\s+/)[4]?.startsWith("127."))
              return;
            if (ws.readyState === 1)
              ws.send(JSON.stringify({ type: "new-ice-candidate", data }));
          });
          await page.exposeFunction("rpc", (m) => {
            calls.push({ method: m.method, params: m.params });
            if (
              dropInputReply &&
              m.method === "keyboardReport" &&
              m.params.keys.length
            ) {
              dropInputReply = false;
              dropped++;
              return { drop: true };
            }
            return {
              result:
                m.method === "getVideoState"
                  ? { ready: true, width: 640, height: 480 }
                  : m.method === "getUSBState"
                    ? "configured"
                    : null,
            };
          });
          const answer = await page.evaluate(
            async ({ sd, lanOnly }) => {
              const pc = (window.pc = new RTCPeerConnection({
                iceServers: [],
              }));
              pc.onicecandidate = (e) => {
                if (e.candidate) window.candidate(e.candidate.toJSON());
              };
              const canvas = document.createElement("canvas");
              canvas.width = 640;
              canvas.height = 480;
              const ctx = canvas.getContext("2d");
              let n = 0;
              setInterval(() => {
                ctx.fillStyle = n++ % 2 ? "#244" : "#466";
                ctx.fillRect(0, 0, 640, 480);
              }, 30);
              pc.addTrack(canvas.captureStream(30).getVideoTracks()[0]);
              // Exercise negotiation of the existing incoming-audio direction too;
              // native PCM distribution remains a separately tracked integration.
              const audio = new AudioContext(),
                oscillator = audio.createOscillator(),
                destination = audio.createMediaStreamDestination();
              oscillator.connect(destination);
              oscillator.start();
              await audio.resume();
              pc.addTrack(destination.stream.getAudioTracks()[0]);
              pc.ondatachannel = (e) => {
                window.channel = e.channel;
                e.channel.onmessage = async (event) => {
                  const m = JSON.parse(event.data),
                    response = await window.rpc(m);
                  if (!response.drop && e.channel.readyState === "open")
                    e.channel.send(
                      JSON.stringify({
                        jsonrpc: "2.0",
                        id: m.id,
                        error: null,
                        result: response.result,
                      }),
                    );
                };
              };
              const offer = JSON.parse(atob(sd));
              if (lanOnly) {
                offer.sdp = offer.sdp
                  .split(/\r?\n/)
                  .filter(
                    (line) =>
                      !line.startsWith("a=candidate:") ||
                      !line.split(/\s+/)[4]?.startsWith("127."),
                  )
                  .join("\r\n");
                if (!offer.sdp.includes("a=candidate:"))
                  throw Error("LAN fixture needs a private IPv4 interface");
              }
              await pc.setRemoteDescription(offer);
              await pc.setLocalDescription(await pc.createAnswer());
              return btoa(JSON.stringify(pc.localDescription));
            },
            { sd: message.data.sd, lanOnly },
          );
          if (ws.readyState === 1)
            ws.send(JSON.stringify({ type: "answer", data: answer }));
        } else if (message.type === "new-ice-candidate" && page) {
          await page.evaluate(
            (data) => window.pc.addIceCandidate(data),
            message.data,
          );
        }
      })
      .catch((error) => {
        console.error("Synthetic peer failed:", error.message);
        ws.close();
      });
  });
});
async function until(check, label, timeout = 12000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    if (await check()) return;
    await delay(50);
  }
  throw Error(label + " timed out");
}
async function api(path, method = "GET", body) {
  const response = await fetch(gateway + "/api/v1" + path, {
    method,
    headers: {
      authorization: "Bearer " + pat,
      "content-type": "application/json",
    },
    body: body ? JSON.stringify(body) : undefined,
    signal: AbortSignal.timeout(40000),
  });
  assert.equal(response.status, 200, path);
  return response.json();
}
async function tool(name, body, allowError = false) {
  const result = await api("/tools/" + name, "POST", body);
  if (result.isError && !allowError) {
    console.error("Fixture state:", {
      logins,
      connections,
      calls: calls.length,
    });
    console.error(daemonLogs);
    for (const page of pages)
      console.error(
        await page.evaluate(() => ({
          pc: window.pc?.connectionState,
          ice: window.pc?.iceConnectionState,
          channel: window.channel?.readyState,
        })),
      );
  }
  if (!allowError)
    assert.equal(
      result.isError,
      false,
      JSON.stringify(result.structuredContent),
    );
  return result;
}
async function remove(device) {
  return api("/devices/" + device.device_id, "DELETE", {
    revision: device.revision,
    confirmed: true,
  });
}
function reference(frame) {
  return {
    frame_id: frame.frame_id,
    view_id: frame.view_id,
    input_revision: frame.input_revision,
  };
}
async function notify(method, params) {
  assert.equal(pages.size, 1);
  await [...pages][0].evaluate(
    ({ method, params }) =>
      window.channel.send(JSON.stringify({ jsonrpc: "2.0", method, params })),
    { method, params },
  );
}
try {
  await copyFile(resolve(process.env.KVMHELM_TEST_BINARY), executable);
  if (helperDirectory) {
    const helper = process.platform === "win32" ? "ffmpeg.exe" : "ffmpeg";
    await copyFile(join(resolve(helperDirectory), helper), join(dir, helper));
    await chmod(join(dir, helper), 0o700);
  }
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
    daemonLogs = (daemonLogs + chunk.toString()).slice(-8000);
    const found = daemonLogs.match(/KVMHelm listening on (http:\/\/[^\s]+)/);
    if (found) gateway = found[1];
  });
  await until(async () => {
    if (daemon.exitCode !== null) throw Error(daemonLogs);
    return (
      gateway &&
      fetch(gateway + "/healthz")
        .then((r) => r.ok)
        .catch(() => false)
    );
  }, "Daemon startup");
  browser = await chromium.launch({
    headless: true,
    args: [
      "--disable-features=WebRtcHideLocalIpsWithMdns",
      "--allow-loopback-in-peer-connection",
      "--autoplay-policy=no-user-gesture-required",
    ],
  });
  await new Promise((r) => fixture.listen(0, "127.0.0.1", r));
  const address = `http://127.0.0.1:${fixture.address().port}`;
  const device = await api("/devices", "POST", {
    device: {
      name: "Synthetic native JetKVM",
      driver_id: "jetkvm",
      model: "JetKVM",
      address,
      keyboard_layout: "de",
      media_profile: "warm",
      minimum_action_latency_ms: 0,
    },
    secret: { username: "", password: "fixture-only" },
  });
  let opened = await tool("open_computer", {
    computer_id: device.device_id,
    mode: "control",
    request_id: "open",
  });
  let session = opened.structuredContent.session.session_id;
  let frame = opened.structuredContent.frames[0];
  assert.equal(frame.width, 640);
  assert.equal(frame.height, 480);
  assert.equal(opened.content[1].mimeType, "image/jpeg");
  assert.ok(Buffer.from(opened.content[1].data, "base64").length > 100);
  await until(
    async () => {
      return [...pages][0].evaluate(async () => {
        const stats = await window.pc.getStats();
        return [...stats.values()].some(
          (r) =>
            r.type === "outbound-rtp" && r.kind === "video" && r.pliCount > 0,
        );
      });
    },
    "RTCP keyframe request",
    5000,
  );
  assert.equal(logins, 1);
  if (lanOnly) {
    const remote = await [...pages][0].evaluate(async () => {
      const stats = await window.pc.getStats();
      for (const report of stats.values())
        if (report.type === "transport" && report.selectedCandidatePairId) {
          const pair = stats.get(report.selectedCandidatePairId);
          return stats.get(pair.remoteCandidateId).address;
        }
    });
    assert.ok(
      remote && !remote.startsWith("127."),
      "actual WebRTC must use the gathered LAN interface",
    );
  }
  // Independent RTP tracks need not become observable with the first video
  // frame. Await actual incoming audio discovery before opening its ticket.
  await until(
    async () => (await api("/devices"))[0].capabilities.audio.from_target,
    "Incoming Opus track discovery",
    5000,
  );
  const listed = await api("/devices");
  assert.equal(listed[0].capabilities.keyboard.hid, true);
  assert.equal(listed[0].capabilities.audio.to_target, false);
  assert.equal(listed[0].capabilities.audio.from_target, true);
  const audioTicket = await api(`/sessions/${session}/audio`, "POST", {
    microphone: false,
  });
  const audio = new WebSocket(
    gateway.replace("http:", "ws:") + "/api/v1/audio",
    { origin: gateway },
  );
  const audioMessages = [];
  audio.on("message", (data, binary) =>
    audioMessages.push(
      binary ? Buffer.from(data) : JSON.parse(data.toString()),
    ),
  );
  await new Promise((resolve, reject) => {
    audio.once("open", resolve);
    audio.once("error", reject);
  });
  audio.send(JSON.stringify({ ticket: audioTicket.ticket }));
  try {
    await until(() => audioMessages.some((m) => m.ready), "Native audio ready");
    await until(
      () =>
        audioMessages.some((m) => Buffer.isBuffer(m) && m.some((v) => v !== 0)),
      "Decoded Chromium Opus tone",
    );
    assert.ok(
      audioMessages
        .filter(Buffer.isBuffer)
        .every((m) => m.length % 4 === 0 && m.length <= 23040),
    );
  } finally {
    const closed = new Promise((resolve) => audio.once("close", resolve));
    audio.close();
    await closed;
  }
  const unsupportedMic = await fetch(
    gateway + `/api/v1/sessions/${session}/audio`,
    {
      method: "POST",
      headers: {
        authorization: "Bearer " + pat,
        "content-type": "application/json",
      },
      body: JSON.stringify({ microphone: true }),
    },
  );
  assert.equal((await unsupportedMic.json()).error.code, "UNSUPPORTED_ACTION");
  // Compare exact reports against actual inherited TS planning and Jet USB
  // conversion, rather than a second expected implementation in this fixture.
  const actions = [
    { type: "click", x: 639, y: 479, button: "right" },
    { type: "type", text: "Ä@^" },
    { type: "keypress", keys: ["CTRL", "A"] },
    { type: "keypress", keys: ["F24", "ShiftRight", "ALTGR"] },
    { type: "keypress", keys: ["A", "B", "C", "D", "E", "F"] },
    { type: "scroll", x: 0, y: 0, scroll_x: 950, scroll_y: -50 },
    { type: "double_click", x: 2, y: 3 },
    {
      type: "drag",
      path: [
        { x: 0, y: 0 },
        { x: 50, y: 50 },
        { x: 639, y: 479 },
      ],
    },
  ];
  const expected = [];
  const legacy = new JetKvmDriver(
    deviceSchema.parse({
      name: "Reference report planner",
      device_id: device.device_id,
      driver_id: "jetkvm",
      address,
      keyboard_layout: "de",
    }),
    {},
  );
  legacy.caps.keyboard.hid = true;
  legacy.caps.mouse.absolute = true;
  legacy.rpc = async (method, params) => {
    expected.push({ method, params: structuredClone(params) });
    return null;
  };
  const context = {
    signal: AbortSignal.timeout(10000),
    width: 640,
    height: 480,
    assertControl() {},
  };
  legacy.validate(actions, 640, 480);
  for (const action of actions) await legacy.execute(action, context);
  const before = calls.length;
  const input = {
    session_id: session,
    request_id: "reports",
    reference: reference(frame),
    actions,
    observation: { mode: "none" },
  };
  const executed = await tool("computer", input);
  assert.deepEqual(calls.slice(before), expected);
  assert.ok(
    executed.structuredContent.actions.every((a) => a.status === "sent"),
  );
  assert.deepEqual(await tool("computer", input), executed);
  assert.equal(calls.length, before + expected.length);
  const screenshot = await tool("computer_screenshot", { session_id: session });
  frame = screenshot.structuredContent.frames[0];
  const invalidCount = calls.length;
  const invalid = await tool(
    "computer",
    {
      session_id: session,
      request_id: "rollover",
      reference: reference(frame),
      actions: [
        { type: "click", x: 1, y: 1 },
        { type: "keypress", keys: ["A", "B", "C", "D", "E", "F", "G"] },
      ],
      observation: { mode: "none" },
    },
    true,
  );
  assert.equal(invalid.structuredContent.error.code, "INVALID_ARGUMENT");
  assert.equal(calls.length, invalidCount);
  const unsupported = await tool(
    "computer",
    {
      session_id: session,
      request_id: "unsupported-text",
      reference: reference(frame),
      actions: [
        { type: "click", x: 1, y: 1 },
        { type: "type", text: "🙂" },
      ],
      observation: { mode: "none" },
    },
    true,
  );
  assert.equal(unsupported.structuredContent.error.code, "UNSUPPORTED_TEXT");
  assert.equal(calls.length, invalidCount);
  dropInputReply = true;
  const uncertainInput = {
    session_id: session,
    request_id: "lost-rpc",
    reference: reference(frame),
    actions: [{ type: "keypress", keys: ["B"] }],
    observation: { mode: "none" },
  };
  const uncertain = await tool("computer", uncertainInput, true);
  assert.equal(dropped, 1);
  assert.equal(uncertain.structuredContent.actions[0].status, "unknown");
  const afterUnknown = calls.length;
  assert.deepEqual(await tool("computer", uncertainInput, true), uncertain);
  assert.equal(calls.length, afterUnknown);
  assert.deepEqual(
    calls.filter((c) => c.method === "keyboardReport").at(-1).params,
    { modifier: 0, keys: [] },
  );
  await notify("videoState", { ready: false });
  await delay(100);
  const absent = await tool(
    "computer_screenshot",
    { session_id: session },
    true,
  );
  assert.equal(absent.structuredContent.error.code, "NO_SIGNAL");
  await notify("videoState", { ready: true });
  await delay(100);
  assert.equal(
    (await tool("computer_screenshot", { session_id: session })).content[1]
      .mimeType,
    "image/jpeg",
  );
  const stream = await fetch(gateway + `/api/v1/sessions/${session}/video`, {
    headers: { authorization: "Bearer " + pat },
    signal: AbortSignal.timeout(6000),
  });
  assert.equal(stream.status, 200);
  const reader = stream.body.getReader();
  assert.ok((await reader.read()).value.length > 100);
  await reader.cancel();
  await tool("close_computer", { session_id: session });
  // Signaling fault disposes the peer/decoder and reconnect retains auth cookie.
  for (const ws of wss.clients) ws.close();
  await until(() => pages.size === 0, "Peer cleanup");
  await delay(1200);
  opened = await tool("open_computer", {
    computer_id: device.device_id,
    mode: "control",
    request_id: "reopen",
  });
  session = opened.structuredContent.session.session_id;
  assert.equal(logins, 1, "reconnect must reuse login cookie");
  assert.ok(connections >= 2);
  await notify("usbState", "unconfigured");
  await delay(100);
  const count = calls.length;
  const noUsb = await tool(
    "computer",
    {
      session_id: session,
      request_id: "usb-off",
      reference: reference(opened.structuredContent.frames[0]),
      actions: [{ type: "keypress", keys: ["A"] }],
      observation: { mode: "none" },
    },
    true,
  );
  assert.equal(noUsb.structuredContent.error.code, "UNSUPPORTED_ACTION");
  assert.equal(calls.length, count);
  await notify("usbState", "configured");
  await delay(100);
  await notify("otherSessionConnected", {});
  await until(
    async () => (await api("/devices"))[0].lease === null,
    "Control release after peer takeover",
  );
  const pressedBefore = calls.filter(
    (c) => c.method === "keyboardReport" && c.params.keys.length,
  ).length;
  const lost = await tool(
    "computer",
    {
      session_id: session,
      request_id: "after-takeover",
      reference: reference(opened.structuredContent.frames[0]),
      actions: [{ type: "keypress", keys: ["C"] }],
      observation: { mode: "none" },
    },
    true,
  );
  assert.equal(lost.isError, true);
  assert.equal(
    calls.filter((c) => c.method === "keyboardReport" && c.params.keys.length)
      .length,
    pressedBefore,
  );
  await delay(1200);
  const third = await tool("open_computer", {
    computer_id: device.device_id,
    mode: "control",
    request_id: "third",
  });
  await tool("close_computer", {
    session_id: third.structuredContent.session.session_id,
  });
  assert.equal(logins, 1);
  await remove(device);
  await until(() => pages.size === 0, "Deleted peer cleanup");
  const beforeAuth = connections;
  const wrong = await api("/devices", "POST", {
    device: {
      name: "Wrong password fixture",
      driver_id: "jetkvm",
      address,
      media_profile: "warm",
    },
    secret: { username: "", password: "wrong-fixture-only" },
  });
  const denied = await tool(
    "open_computer",
    { computer_id: wrong.device_id, mode: "observe" },
    true,
  );
  assert.equal(denied.structuredContent.error.code, "AUTH_FAILED");
  assert.equal(connections, beforeAuth);
  await remove(wrong);
  mode = "public-candidate";
  const guarded = await api("/devices", "POST", {
    device: {
      name: "Invalid ICE fixture",
      driver_id: "jetkvm",
      address,
      media_profile: "warm",
    },
    secret: { username: "", password: "fixture-only" },
  });
  const guardCount = calls.length;
  const blocked = await tool(
    "open_computer",
    { computer_id: guarded.device_id, mode: "observe" },
    true,
  );
  assert.equal(blocked.isError, true);
  assert.equal(calls.length, guardCount, "no RPC after rejected public ICE");
  await remove(guarded);
  mode = "setup";
  const setup = await api("/devices", "POST", {
    device: {
      name: "Unconfigured fixture",
      driver_id: "jetkvm",
      address,
      media_profile: "warm",
    },
  });
  const setupResult = await tool(
    "open_computer",
    { computer_id: setup.device_id, mode: "observe" },
    true,
  );
  assert.equal(
    setupResult.structuredContent.error.code,
    "DEVICE_NOT_CONFIGURED",
  );
  await remove(setup);
  console.log(
    "Native JetKVM: actual H.264/JPEG and Opus/PCM, exact USB reports, auth/reconnect, replay/unknown RPC, live state, stream cleanup and ICE guards passed",
  );
} finally {
  // Delete configured devices first, so owned helpers are reaped even when a
  // failed assertion precedes an ungraceful Windows test-daemon termination.
  if (gateway && daemon?.exitCode === null) {
    for (const device of await api("/devices").catch(() => []))
      await remove(device).catch(() => {});
  }
  for (const ws of wss.clients) ws.terminate();
  wss.close();
  await browser?.close();
  fixture.closeAllConnections();
  if (fixture.listening) await new Promise((r) => fixture.close(r));
  if (daemon?.exitCode === null) {
    const ended = new Promise((r) => daemon.once("exit", r));
    daemon.kill();
    await ended;
  }
  const root = resolve(tmpdir()) + sep;
  assert.ok(resolve(dir).startsWith(root));
  await rm(dir, { recursive: true, force: true });
}
