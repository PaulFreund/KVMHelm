import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Store, Secrets } from "../server/store.js";
import { Auth } from "../server/auth.js";
import { Core } from "../server/core.js";
import { scopes } from "../shared/contracts.js";
import { textKeys } from "../server/drivers/keyboard.js";
import { actionSchema, deviceSchema } from "../shared/contracts.js";
async function fixture() {
  const dir = await mkdtemp(join(tmpdir(), "kvmhelm-test-"));
  const store = new Store(dir);
  const auth = new Auth(store);
  const token = await auth.create({
    name: "test",
    scopes: [...scopes],
    devices: ["*"],
  });
  const a = auth.authenticate(token.token, "a"),
    b = auth.authenticate(token.token, "b");
  const core = new Core(store, await Secrets.open(join(dir, "secrets")), auth);
  await core.load();
  assert.equal(core.list(a).length, 0, "fresh installation must not seed demo devices");
  const device = await core.saveDevice(a, {
    name: "Synthetic",
    driver_id: "simulator",
    transport: "simulator",
  });
  return {
    core,
    store,
    auth,
    a,
    b,
    device,
    close: async () => {
      await core.shutdown();
      await store.close();
      await rm(dir, { recursive: true, force: true });
    },
  };
}
test("device image deadline follows last input; waits count and observations do not restart it", async () => {
  const f = await fixture();
  try {
    assert.equal(deviceSchema.parse({ name: "test" }).minimum_action_latency_ms, 30);
    assert.throws(() => deviceSchema.parse({ name: "test", minimum_action_latency_ms: -1 }));
    assert.throws(() => actionSchema.parse({ type: "type", text: "W", before_ms: 1 }));
    const opened = await f.core.tool(f.a, "open_computer", { computer_id: f.device.device_id, mode: "control", request_id: "timing-open" });
    const session_id = (opened.session as any).session_id;
    const info = opened.images![0].info;
    const reference = { frame_id: info.frame_id, view_id: info.view_id, input_revision: info.input_revision };
    const r = f.core.get(f.device.device_id);
    r.device.minimum_action_latency_ms = 100;
    const dispatches: number[] = [];
    r.driver.execute = async () => { dispatches.push(performance.now()); return "sent"; };
    const request = { session_id, reference, request_id: "timed", actions: [
      { type: "type", text: "W" }, { type: "click", x: 10, y: 10 }
    ], observation: { mode: "none" } };
    const result = await f.core.tool(f.a, "computer", request);
    assert.equal(result.ok, true);
    assert.equal(dispatches.length, 2);
    assert.ok(dispatches[1] - dispatches[0] < 100, "no implicit 150 ms typing pause");
    await f.core.tool(f.a, "computer", request);
    assert.equal(dispatches.length, 2, "retry must not repeat input or reset deadline");
    const completed = r.lastInputCompletedAt!;
    const original = r.media.screenshot.bind(r.media);
    const observations: number[] = [];
    r.media.screenshot = async (...args) => { observations.push(performance.now()); return original(...args); };
    const fresh = await f.core.tool(f.a, "computer_screenshot", { session_id });
    assert.equal(fresh.ok, true);
    assert.ok(observations[0] - completed >= 100);
    assert.equal(r.lastInputCompletedAt, completed);
    // Replace capture with an immediate result to isolate any added gateway delay.
    r.media.screenshot = async () => fresh.images![0];
    const later = performance.now();
    await f.core.tool(f.a, "computer_screenshot", { session_id });
    assert.ok(performance.now() - later < 90, "later observation must not wait another 100 ms");
    const ref = fresh.images![0].info;
    r.media.screenshot = async () => { observations.push(performance.now()); return fresh.images![0]; };
    const waited = await f.core.tool(f.a, "computer", { session_id, request_id: "waited", reference: { frame_id: ref.frame_id, view_id: ref.view_id, input_revision: ref.input_revision }, actions: [
      { type: "click", x: 10, y: 10 }, { type: "wait", duration_ms: 150 }
    ] });
    assert.equal(waited.ok, true);
    assert.ok(observations.at(-1)! - r.lastInputCompletedAt! >= 150);
    assert.ok(observations.at(-1)! - r.lastInputCompletedAt! < 230, "explicit wait consumes the minimum deadline");
  } finally { await f.close(); }
});

test("central lease is exclusive; duplicate action does not execute twice; emergency stop grants no control", async () => {
  const f = await fixture();
  try {
    const [x, y] = await Promise.all([
      f.core.tool(f.a, "open_computer", {
        computer_id: f.device.device_id,
        mode: "control",
        request_id: "open",
      }),
      f.core.tool(f.b, "open_computer", {
        computer_id: f.device.device_id,
        mode: "control",
        request_id: "open",
      }),
    ]);
    assert.equal(x.ok, true);
    assert.equal(y.ok, false);
    assert.equal((y.error as any).code, "CONTROL_BUSY");
    const s = (x.session as any).session_id,
      frame = x.images![0].info;
    const request = {
      session_id: s,
      request_id: "action",
      reference: {
        frame_id: frame.frame_id,
        view_id: frame.view_id,
        input_revision: frame.input_revision,
      },
      actions: [{ type: "click", x: 100, y: 100 }],
    };
    const [r1, r2] = await Promise.all([
      f.core.tool(f.a, "computer", request),
      f.core.tool(f.a, "computer", request),
    ]);
    assert.equal(r1.ok, true);
    assert.deepEqual(r1, r2);
    assert.equal((f.core.get(f.device.device_id).driver as any).count, 1);
    const conflict = await f.core.tool(f.a, "computer", {
      ...request,
      actions: [{ type: "click", x: 200, y: 100 }],
    });
    assert.equal((conflict.error as any).code, "REQUEST_CONFLICT");
    await f.core.stopControl(f.b, f.device.device_id);
    assert.equal(f.core.get(f.device.device_id).lease, undefined);
    const stopped = await f.core.tool(f.a, "computer_control", {
      session_id: s,
      request_id: "renew",
      operation: "acquire",
    });
    assert.equal((stopped.error as any).code, "CONTROL_SUSPENDED");
    await f.core.stopControl(f.b, f.device.device_id, true);
    assert.equal(f.core.get(f.device.device_id).lease, undefined);
  } finally {
    await f.close();
  }
});
test("a session ID is not authorization; revocation removes live sessions", async () => {
  const f = await fixture();
  try {
    const opened = await f.core.tool(f.a, "open_computer", {
      computer_id: f.device.device_id,
      mode: "observe",
    });
    const s = (opened.session as any).session_id;
    const denied = await f.core.tool(f.b, "computer_screenshot", {
      session_id: s,
    });
    assert.equal((denied.error as any).code, "FORBIDDEN");
    await f.auth.revoke(f.a.token.id);
    assert.equal(f.core.sessions.size, 0);
    const revoked = await f.core.tool(f.a, "computer_screenshot", {
      session_id: s,
    });
    assert.equal((revoked.error as any).code, "AUTH_FAILED");
  } finally {
    await f.close();
  }
});
test("reconnection invalidates views; preferences detect write conflicts", async () => {
  const f = await fixture();
  try {
    const opened = await f.core.tool(f.a, "open_computer", {
      computer_id: f.device.device_id,
      mode: "control",
      request_id: "open",
    });
    const frame = opened.images![0].info;
    f.core.get(f.device.device_id).media.reconnect();
    const result = await f.core.tool(f.a, "computer", {
      session_id: (opened.session as any).session_id,
      request_id: "old",
      reference: {
        frame_id: frame.frame_id,
        view_id: frame.view_id,
        input_revision: frame.input_revision,
      },
      actions: [{ type: "click", x: 1, y: 1 }],
    });
    assert.equal((result.error as any).code, "VIEW_CHANGED");
    await f.store.put("preferences", "owner", { theme: "dark" }, 0);
    await assert.rejects(
      f.store.put("preferences", "owner", { theme: "light" }, 0),
      /REVISION_CONFLICT/,
    );
  } finally {
    await f.close();
  }
});
test("US/DE typing validates entire text, AltGr and line breaks without replacement", () => {
  assert.deepEqual(textKeys("Yz\n", "de"), [
    ["ShiftLeft", "KeyZ"],
    ["KeyY"],
    ["Enter"],
  ]);
  assert.deepEqual(textKeys("@ä€", "de"), [
    ["AltRight", "KeyQ"],
    ["Quote"],
    ["AltRight", "KeyE"],
  ]);
  assert.deepEqual(textKeys("@", "us"), [["ShiftLeft", "Digit2"]]);
  assert.throws(() => textKeys("ok🙂", "de"), /U\+1F642/);
});
