import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { Store, Secrets } from "../server/store.js";
import { Auth } from "../server/auth.js";
import { Core } from "../server/core.js";
import { PluginHost } from "../server/plugins.js";
import { serve } from "../server/server.js";
import { scopes } from "../shared/contracts.js";
test("HTTP and stdio return MCP images and arbitrate through the same daemon", async () => {
  const dir = await mkdtemp(join(tmpdir(), "kvmhelm-interop-"));
  const store = new Store(dir),
    secrets = await Secrets.open(join(dir, "secrets")),
    auth = new Auth(store);
  const token = await auth.create({
    name: "interop",
    scopes: [...scopes],
    devices: ["*"],
  });
  const owner = auth.authenticate(token.token, "fixture");
  const core = new Core(store, secrets, auth),
    plugins = new PluginHost(core, join(dir, "plugins"));
  const d = await core.saveDevice(owner, {
    name: "Interop synthetic",
    driver_id: "simulator",
    transport: "simulator",
  });
  const port = 19876;
  let daemon = await serve(core, plugins, {
    host: "127.0.0.1",
    port,
    origins: [],
  });
  const patPath = join(dir, "test.pat");
  await writeFile(patPath, token.token, { mode: 0o600 });
  const http = new Client({ name: "HTTP smoke", version: "1" }),
    stdio = new Client({ name: "stdio smoke", version: "1" });
  const upstream = new StreamableHTTPClientTransport(
    new URL(`http://127.0.0.1:${port}/mcp`),
    { requestInit: { headers: { Authorization: "Bearer " + token.token } } },
  );
  const bridge = new StdioClientTransport({
    command: process.execPath,
    args: [
      resolve("dist/server/cli.js"),
      "mcp",
      "--transport",
      "stdio",
      "--url",
      `http://127.0.0.1:${port}`,
      "--pat-file",
      patPath,
    ],
    stderr: "pipe",
  });
  try {
    await http.connect(upstream);
    await stdio.connect(bridge);
    assert.ok(http.getInstructions()?.includes("computer.reference"));
    assert.equal(stdio.getInstructions(), http.getInstructions());
    assert.equal((await http.listTools()).tools.length, 6);
    assert.equal((await stdio.listTools()).tools.length, 6);
    const opened: any = await http.callTool({
      name: "open_computer",
      arguments: {
        computer_id: d.device_id,
        mode: "control",
        request_id: "open",
      },
    });
    assert.equal(opened.isError, false);
    assert.equal(opened.content[1].type, "image");
    assert.equal(opened.content[1].mimeType, "image/png");
    const denied: any = await stdio.callTool({
      name: "open_computer",
      arguments: {
        computer_id: d.device_id,
        mode: "control",
        request_id: "busy",
      },
    });
    assert.equal(denied.structuredContent.error.code, "CONTROL_BUSY");
    const observed: any = await stdio.callTool({
      name: "open_computer",
      arguments: { computer_id: d.device_id, mode: "observe" },
    });
    assert.equal(observed.content[1].type, "image");
    const info = opened.structuredContent.frames[0];
    const acted: any = await http.callTool({
      name: "computer",
      arguments: {
        session_id: opened.structuredContent.session.session_id,
        request_id: "click",
        reference: {
          frame_id: info.frame_id,
          view_id: info.view_id,
          input_revision: info.input_revision,
        },
        actions: [{ type: "click", x: 20, y: 20 }],
      },
    });
    assert.equal(acted.isError, false);
    assert.equal(acted.structuredContent.actions[0].status, "sent");
    await http.callTool({
      name: "close_computer",
      arguments: { session_id: opened.structuredContent.session.session_id },
    });
    const acquired: any = await stdio.callTool({
      name: "computer_control",
      arguments: {
        session_id: observed.structuredContent.session.session_id,
        request_id: "acquire",
        operation: "acquire",
      },
    });
    assert.equal(acquired.isError, false);
    // Keep the same stdio process alive while its upstream session disappears.
    await daemon.close();
    daemon = await serve(core, plugins, { host: "127.0.0.1", port, origins: [] });
    const interrupted: any = await stdio.callTool({
      name: "computer_control", arguments: {
        session_id: observed.structuredContent.session.session_id,
        request_id: "must-not-replay", operation: "acquire",
      },
    });
    assert.equal(interrupted.structuredContent.error.code, "MCP_CONNECTION_LOST");
    const recovered: any = await stdio.callTool({ name: "list_computers", arguments: {} });
    assert.equal(recovered.isError, false);
    assert.equal(recovered.structuredContent.computers[0].lease, null);
    const reopened: any = await stdio.callTool({ name: "open_computer", arguments: { computer_id: d.device_id, mode: "observe" } });
    assert.equal(reopened.content[1].type, "image");
    await daemon.close();
    const offline: any = await stdio.callTool({ name: "list_computers", arguments: {} });
    assert.equal(offline.structuredContent.error.code, "MCP_CONNECTION_LOST");
    daemon = await serve(core, plugins, { host: "127.0.0.1", port, origins: [] });
    // A read-only tool automatically recovers even before tools/list is called.
    assert.equal((await stdio.callTool({ name: "list_computers", arguments: {} })).isError, false);
    assert.equal((await stdio.listTools()).tools.length, 6);
    await stdio.close();
    await new Promise((r) => setTimeout(r, 400));
    assert.equal((await fetch(`http://127.0.0.1:${port}/healthz`)).status, 200);
  } finally {
    await stdio.close().catch(() => {});
    await upstream.terminateSession().catch(() => {});
    await http.close().catch(() => {});
    await daemon.close();
    await plugins.shutdown();
    await core.shutdown();
    await store.close();
    assert.ok(
      resolve(dir).startsWith(resolve(tmpdir())) &&
        dir.includes("kvmhelm-interop-"),
    );
    await rm(dir, { recursive: true, force: true });
  }
});
