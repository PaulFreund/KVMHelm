import { test } from "node:test";
import assert from "node:assert/strict";
import { serve } from "../server/server.js";
import { fixture } from "./fixture.js";
import { safetyRequest } from "../server/request-limits.js";

test("media budgets cannot block stop, release, health or a second token", async (t) => {
  const f = await fixture(t);
  const daemon = await serve(f.core, f.plugins, {
    host: "127.0.0.1",
    port: 0,
    origins: [],
    limits: { media: 2, control: 8 },
  });
  t.after(() => daemon.close());
  const address = daemon.server.address() as { port: number },
    base = `http://127.0.0.1:${address.port}`;
  const request = (
    path: string,
    method = "GET",
    body?: unknown,
    token = f.token.token,
  ) =>
    fetch(base + path, {
      method,
      headers: {
        Authorization: `Bearer ${token}`,
        "Content-Type": "application/json",
      },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  const opened = await (
    await request("/api/v1/tools/open_computer", "POST", {
      computer_id: f.device.device_id,
      mode: "control",
      request_id: "open",
    })
  ).json();
  const session = opened.structuredContent.session.session_id;
  for (let n = 0; n < 2; n++) {
    const r = await request(`/api/v1/sessions/${session}/frame`);
    assert.equal(r.status, 200);
    await r.arrayBuffer();
  }
  const rate = await request(`/api/v1/sessions/${session}/frame`);
  assert.equal(rate.status, 429);
  assert.equal((await rate.json()).error.code, "RATE_LIMIT");
  for (let n = 0; n < 10; n++) {
    const r = await request("/api/v1/devices");
    await r.arrayBuffer();
  }
  const limited = await request("/api/v1/devices");
  assert.equal(limited.status, 429);
  await limited.arrayBuffer();
  const release = await request("/api/v1/tools/computer_control", "POST", {
    session_id: session,
    request_id: "release",
    operation: "release",
  });
  assert.equal(release.status, 200);
  assert.equal((await release.json()).isError, false);
  const stop = await request(
    `/api/v1/devices/${f.device.device_id}/stop`,
    "POST",
    {},
  );
  assert.equal(stop.status, 200);
  await stop.arrayBuffer();
  const close = await request("/api/v1/tools/close_computer", "POST", {
    session_id: session,
  });
  assert.equal(close.status, 200);
  await close.arrayBuffer();
  const other = await f.auth.create({
    name: "other",
    scopes: ["devices:read"],
    devices: ["*"],
  });
  const list = await request("/api/v1/devices", "GET", undefined, other.token);
  assert.equal(list.status, 200);
  await list.arrayBuffer();
  const denied = await request(
    `/api/v1/devices/${f.device.device_id}/stop`,
    "POST",
    {},
    other.token,
  );
  assert.equal(denied.status, 403);
  await denied.arrayBuffer();
  for (const path of ["/healthz", "/readyz"]) {
    const r = await fetch(base + path);
    assert.equal(r.status, 200);
    await r.arrayBuffer();
  }
  await f.store.close();
  const failed = await fetch(base + "/readyz");
  assert.equal(failed.status, 503);
  await failed.arrayBuffer();
  const live = await fetch(base + "/healthz");
  assert.equal(live.status, 200);
  await live.arrayBuffer();
});

test("MCP and HTTP safety classification exempts only safe operations", () => {
  const check = (path: string, body: unknown, method = "POST") =>
    safetyRequest({ path, body, method } as any);
  assert.equal(
    check("/api/v1/tools/computer_control", { operation: "release" }),
    true,
  );
  assert.equal(
    check("/api/v1/tools/computer_control", { operation: "acquire" }),
    false,
  );
  assert.equal(
    check("/mcp", {
      method: "tools/call",
      params: { name: "computer_control", arguments: { operation: "release" } },
    }),
    true,
  );
  assert.equal(
    check("/mcp", {
      method: "tools/call",
      params: { name: "computer", arguments: { operation: "release" } },
    }),
    false,
  );
});

test("event stream exposes daemon identity and discards a stale restart cursor", async (t) => {
  const f = await fixture(t);
  const daemon = await serve(f.core, f.plugins, {
    host: "127.0.0.1",
    port: 0,
    origins: [],
  });
  t.after(() => daemon.close());
  const port = (daemon.server.address() as { port: number }).port;
  const controller = new AbortController();
  t.after(() => controller.abort());
  const response = await fetch(`http://127.0.0.1:${port}/api/v1/events`, {
    headers: {
      Authorization: `Bearer ${f.token.token}`,
      "Last-Event-ID": "999999",
      "X-KVMHelm-Instance": "previous-daemon",
    },
    signal: controller.signal,
  });
  assert.equal(response.headers.get("X-KVMHelm-Instance"), f.core.instanceId);
  const data = await response.body!.getReader().read();
  assert.match(new TextDecoder().decode(data.value), /device.updated/);
  controller.abort();
});
