// Exercise the copied native daemon/stdio binary with the original TypeScript
// SDK, including a response lost after accepted input. All targets are synthetic.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import {
  copyFile,
  chmod,
  mkdtemp,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve, sep } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { imagePresentation, instructions } from "../dist/server/mcp.js";

assert.ok(
  process.env.KVMHELM_TEST_BINARY,
  "Set KVMHELM_TEST_BINARY to the built native executable",
);
const dir = await mkdtemp(join(tmpdir(), "kvmhelm-native-mcp-"));
const executable = join(
  dir,
  process.platform === "win32" ? "kvmhelm.exe" : "kvmhelm",
);
const childEnv = Object.fromEntries(
  Object.entries(process.env).filter(
    ([key]) => !key.toUpperCase().startsWith("KVMHELM_"),
  ),
);
childEnv.PATH = "";
const args = ["--data", join(dir, "data"), "--secrets", join(dir, "secrets")];
const options = { cwd: dir, env: childEnv, windowsHide: true };
const patFile = join(dir, "secrets", "cli.pat");
const referenceDaemon = process.env.KVMHELM_TEST_DAEMON === "typescript";
const daemonCommand = referenceDaemon ? process.execPath : executable;
const daemonPrefix = referenceDaemon ? [resolve("dist/server/cli.js")] : [];
let daemon,
  url = "",
  proxy,
  upstream,
  http,
  stdio;
let dropCalls = 0,
  acceptedDrop;
async function start() {
  if (referenceDaemon && !url) {
    // The reference CLI reports the requested port rather than the assigned
    // port for --port 0. Reserve a free test port before starting that daemon.
    const probe = createServer();
    await new Promise((resolve) => probe.listen(0, "127.0.0.1", resolve));
    url = `http://127.0.0.1:${probe.address().port}`;
    await new Promise((resolve) => probe.close(resolve));
  }
  daemon = spawn(
    daemonCommand,
    [
      ...daemonPrefix,
      "serve",
      "--host",
      "127.0.0.1",
      "--port",
      url ? new URL(url).port : "0",
      ...args,
    ],
    { ...options, stdio: ["ignore", "ignore", "pipe"] },
  );
  let logs = "";
  daemon.stderr.on("data", (chunk) => {
    logs = (logs + chunk.toString()).slice(-8000);
    const found = logs.match(/KVMHelm listening on (http:\/\/[^\s]+)/);
    if (found) url = found[1];
  });
  for (let n = 0; n < 120; n++) {
    if (daemon.exitCode !== null) throw Error("Native daemon stopped: " + logs);
    if (
      url &&
      (await fetch(url + "/healthz")
        .then((r) => r.ok)
        .catch(() => false))
    )
      return;
    await delay(100);
  }
  throw Error("Native daemon startup timeout");
}
async function stop() {
  if (!daemon || daemon.exitCode !== null) return;
  const ended = new Promise((resolve) => daemon.once("exit", resolve));
  daemon.kill();
  await ended;
}
function packed(text) {
  if (text.startsWith("{")) return JSON.parse(text).result;
  for (const line of text.split("\n")) {
    if (line.startsWith("data: ")) {
      const message = JSON.parse(line.slice(6));
      if (message.result) return message.result;
    }
  }
  throw Error("Missing MCP response");
}
function image(result) {
  assert.equal(result.isError, false, JSON.stringify(result.structuredContent));
  assert.equal(result.content[1].type, "image");
  assert.equal(result.content[1].mimeType, "image/png");
  assert.deepEqual(result.content[1].annotations, {
    audience: ["user", "assistant"],
    priority: 1,
  });
  assert.equal(result.content[2].text, imagePresentation);
  assert.deepEqual(
    JSON.parse(result.content[0].text),
    result.structuredContent,
  );
  assert.ok(Buffer.from(result.content[1].data, "base64").length > 100);
}
try {
  await copyFile(resolve(process.env.KVMHELM_TEST_BINARY), executable);
  await chmod(executable, 0o700);
  const bootstrap = spawnSync(executable, ["token", "bootstrap", ...args], {
    ...options,
    encoding: "utf8",
  });
  assert.equal(bootstrap.status, 0, bootstrap.stderr);
  let pat = (await readFile(patFile, "utf8")).trim();
  assert.ok(!bootstrap.stdout.includes(pat));
  await start();
  const api = async (path, method = "GET", body) => {
    const response = await fetch(url + "/api/v1" + path, {
      method,
      headers: {
        Authorization: "Bearer " + pat,
        "Content-Type": "application/json",
      },
      body: body ? JSON.stringify(body) : undefined,
    });
    assert.equal(response.status, 200, path);
    return response.json();
  };
  const device = await api("/devices", "POST", {
    device: {
      name: "Native interop simulator",
      driver_id: "simulator",
      transport: "simulator",
      media_profile: "warm",
      minimum_action_latency_ms: 0,
    },
  });
  const blockedData = join(dir, "remote-cli-must-not-open-this-file");
  await writeFile(blockedData, "The bridge must use the running daemon");
  proxy = createServer(async (request, response) => {
    try {
      const chunks = [];
      let bytes = 0;
      for await (const chunk of request) {
        bytes += chunk.length;
        if (bytes > 256 * 1024) throw Error("Fixture body budget");
        chunks.push(chunk);
      }
      const body = Buffer.concat(chunks).toString();
      const message = body ? JSON.parse(body) : {};
      if (
        message.method === "tools/call" &&
        message.params?.arguments?.request_id === "rate-denied"
      ) {
        response.writeHead(429, { "content-type": "application/json" });
        response.end(JSON.stringify({ error: { code: "RATE_LIMIT" } }));
        return;
      }
      const headers = {};
      for (const name of [
        "authorization",
        "content-type",
        "accept",
        "mcp-session-id",
        "mcp-protocol-version",
      ]) {
        if (request.headers[name]) headers[name] = request.headers[name];
      }
      const target = await fetch(url + "/mcp", {
        method: request.method,
        headers,
        body: body || undefined,
        signal: AbortSignal.timeout(15000),
      });
      const text = await target.text();
      if (
        message.method === "tools/call" &&
        message.params?.name === "computer" &&
        message.params.arguments.request_id === "uncertain-drop"
      ) {
        dropCalls++;
        acceptedDrop = packed(text);
        request.socket.destroy();
        return;
      }
      const outgoing = {};
      for (const name of ["content-type", "mcp-session-id"])
        if (target.headers.has(name)) outgoing[name] = target.headers.get(name);
      response.writeHead(target.status, outgoing);
      response.end(text);
    } catch {
      if (!response.destroyed) {
        response.writeHead(503);
        response.end();
      }
    }
  });
  await new Promise((resolve) => proxy.listen(0, "127.0.0.1", resolve));
  const proxyUrl = `http://127.0.0.1:${proxy.address().port}`;
  http = new Client({ name: "Native HTTP interop", version: "1" });
  stdio = new Client({ name: "Native stdio interop", version: "1" });
  upstream = new StreamableHTTPClientTransport(new URL(url + "/mcp"), {
    requestInit: { headers: { Authorization: "Bearer " + pat } },
  });
  const bridge = new StdioClientTransport({
    command: executable,
    args: [
      "mcp",
      "--transport",
      "stdio",
      "--url",
      proxyUrl,
      "--pat-file",
      patFile,
      "--data",
      blockedData,
    ],
    env: childEnv,
    cwd: dir,
    stderr: "pipe",
  });
  await http.connect(upstream);
  await stdio.connect(bridge);
  assert.equal(http.getInstructions(), instructions);
  assert.equal(stdio.getInstructions(), instructions);
  assert.equal((await http.listTools()).tools.length, 6);
  const expectedMetadata = JSON.parse(
    await readFile(resolve("native/src/mcp-contract.json"), "utf8"),
  );
  assert.deepEqual((await http.listTools()).tools, expectedMetadata.tools);
  assert.deepEqual(http.getServerCapabilities(), expectedMetadata.capabilities);
  assert.deepEqual(stdio.getServerCapabilities(), { tools: {} });
  assert.deepEqual(http.getServerVersion(), expectedMetadata.serverInfo);
  assert.deepEqual(
    (await stdio.listTools()).tools,
    (await http.listTools()).tools,
  );
  const opened = await http.callTool({
    name: "open_computer",
    arguments: {
      computer_id: device.device_id,
      mode: "control",
      request_id: "open",
    },
  });
  image(opened);
  const denied = await stdio.callTool({
    name: "open_computer",
    arguments: {
      computer_id: device.device_id,
      mode: "control",
      request_id: "busy",
    },
  });
  assert.equal(denied.structuredContent.error.code, "CONTROL_BUSY");
  const observed = await stdio.callTool({
    name: "open_computer",
    arguments: { computer_id: device.device_id, mode: "observe" },
  });
  image(observed);
  await http.callTool({
    name: "close_computer",
    arguments: { session_id: opened.structuredContent.session.session_id },
  });
  const sid = observed.structuredContent.session.session_id;
  assert.equal(
    (
      await stdio.callTool({
        name: "computer_control",
        arguments: {
          session_id: sid,
          request_id: "acquire",
          operation: "acquire",
        },
      })
    ).isError,
    false,
  );
  const screenshot = await stdio.callTool({
    name: "computer_screenshot",
    arguments: { session_id: sid },
  });
  image(screenshot);
  const frame = screenshot.structuredContent.frames[0];
  await assert.rejects(
    stdio.callTool({
      name: "computer",
      arguments: {
        session_id: sid,
        request_id: "rate-denied",
        reference: {
          frame_id: frame.frame_id,
          view_id: frame.view_id,
          input_revision: frame.input_revision,
        },
        actions: [{ type: "click", x: 20, y: 20 }],
      },
    }),
  );
  assert.ok(
    (await api("/devices"))[0].lease,
    "A rejected request closed valid control",
  );
  const lost = await stdio.callTool({
    name: "computer",
    arguments: {
      session_id: sid,
      request_id: "uncertain-drop",
      reference: {
        frame_id: frame.frame_id,
        view_id: frame.view_id,
        input_revision: frame.input_revision,
      },
      actions: [{ type: "click", x: 20, y: 20 }],
    },
  });
  assert.equal(acceptedDrop?.isError, false, JSON.stringify(acceptedDrop));
  assert.equal(acceptedDrop.structuredContent.actions[0].status, "sent");
  assert.equal(lost.structuredContent.error.code, "MCP_CONNECTION_LOST");
  assert.equal(
    dropCalls,
    1,
    "An uncertain input POST was automatically repeated",
  );
  assert.equal(
    (await stdio.callTool({ name: "list_computers", arguments: {} }))
      .structuredContent.computers[0].lease,
    null,
  );
  const reopened = await stdio.callTool({
    name: "open_computer",
    arguments: { computer_id: device.device_id, mode: "observe" },
  });
  image(reopened);
  await stop();
  await start();
  const interrupted = await stdio.callTool({
    name: "computer_control",
    arguments: {
      session_id: reopened.structuredContent.session.session_id,
      request_id: "must-not-replay",
      operation: "acquire",
    },
  });
  assert.equal(interrupted.structuredContent.error.code, "MCP_CONNECTION_LOST");
  assert.equal(
    (await stdio.callTool({ name: "list_computers", arguments: {} })).isError,
    false,
  );
  await stop();
  assert.equal(
    (await stdio.callTool({ name: "list_computers", arguments: {} }))
      .structuredContent.error.code,
    "MCP_CONNECTION_LOST",
  );
  await start();
  assert.equal(
    (await stdio.callTool({ name: "list_computers", arguments: {} })).isError,
    false,
  );
  assert.equal((await stdio.listTools()).tools.length, 6);
  image(
    await stdio.callTool({
      name: "open_computer",
      arguments: { computer_id: device.device_id, mode: "observe" },
    }),
  );
  const owner = (await api("/tokens"))[0];
  const rotated = await api("/tokens", "POST", {
    name: "Rotated fixture owner",
    scopes: owner.scopes,
    devices: ["*"],
  });
  await writeFile(patFile, "\uFEFF" + rotated.token + "\n");
  await api("/tokens/" + owner.id, "DELETE");
  pat = rotated.token;
  await assert.rejects(stdio.listTools());
  assert.equal(
    (await stdio.callTool({ name: "list_computers", arguments: {} })).isError,
    false,
    "Reconnect did not reread the protected PAT file",
  );
  image(
    await stdio.callTool({
      name: "open_computer",
      arguments: { computer_id: device.device_id, mode: "observe" },
    }),
  );
  assert.equal((await api("/diagnostics")).health.sessions, 1);
  await stdio.close();
  for (let n = 0; n < 40; n++) {
    if ((await api("/diagnostics")).health.sessions === 0) break;
    await delay(50);
  }
  assert.equal(
    (await api("/diagnostics")).health.sessions,
    0,
    "Closing stdio retained daemon sessions",
  );
  assert.equal((await fetch(url + "/healthz")).status, 200);
  assert.equal(dropCalls, 1);
  console.log(
    `PASS: ${referenceDaemon ? "reference TypeScript" : "native"} daemon and copied native stdio binary with empty PATH, exact metadata/images, shared control, no replay after accepted input loses its response, two daemon restarts, offline recovery, protected PAT reread after rotation and session cleanup on stdio EOF.`,
  );
} finally {
  await stdio?.close().catch(() => {});
  await upstream?.terminateSession().catch(() => {});
  await http?.close().catch(() => {});
  if (proxy) {
    proxy.closeAllConnections();
    await new Promise((resolve) => proxy.close(resolve));
  }
  await stop();
  const target = resolve(dir),
    base = resolve(tmpdir()) + sep;
  assert.ok(
    target.startsWith(base) &&
      target.slice(base.length).startsWith("kvmhelm-native-mcp-"),
    "Unsafe cleanup target",
  );
  await rm(target, { recursive: true, force: true });
}
