import { test } from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";
import { readFile, readdir, utimes } from "node:fs/promises";
import { setTimeout as delay } from "node:timers/promises";
import { randomUUID } from "node:crypto";
import { Store } from "../server/store.js";
import { GatewayError } from "../shared/contracts.js";
import { fixture } from "./fixture.js";
import { Media } from "../server/media.js";
import {
  localIceOnly,
  checkIceCandidate,
  checkIceDescription,
  localIceDescription,
} from "../server/lan-ice.js";
import { RTCPeerConnection } from "werift";
import { InputQueue } from "../web/input-queue.js";

test("control acquisition cannot relabel a cached frame", async (t) => {
  const { core, identity: i, device: d } = await fixture(t);
  const open = await core.tool(i, "open_computer", {
    computer_id: d.device_id,
    mode: "observe",
  });
  const session_id = open.session!.session_id,
    r = core.get(d.device_id);
  await r.media.pause();
  const before = await core.tool(i, "computer_screenshot", { session_id });
  const original = before.images![0];
  await core.tool(i, "computer_control", {
    session_id,
    request_id: "acquire",
    operation: "acquire",
  });
  assert.equal(
    r.media.age(original).info.input_revision,
    original.info.input_revision,
  );
  const fresh = await core.tool(i, "computer_screenshot", {
    session_id,
    max_age_ms: 5000,
  });
  assert.equal(fresh.ok, true);
  assert.notEqual(fresh.images![0].info.frame_id, original.info.frame_id);
  const bad = await core.tool(i, "computer", {
    session_id,
    request_id: "old",
    reference: {
      frame_id: original.info.frame_id,
      view_id: original.info.view_id,
      input_revision: r.media.revision,
    },
    actions: [{ type: "click", x: 1, y: 1 }],
  });
  assert.equal(bad.error?.code, "STALE_REFERENCE");
});

test("capture started before a revision change retains that revision", async (t) => {
  const { core, device } = await fixture(t),
    r = core.get(device.device_id);
  await core.connect(r);
  let finish!: (value: Awaited<ReturnType<typeof r.driver.snapshot>>) => void;
  const original = await r.driver.snapshot(AbortSignal.timeout(1000));
  r.driver.snapshot = () =>
    new Promise((resolve) => {
      finish = resolve;
    });
  const media = new Media(device.device_id, r.driver);
  const pending = (media as any).capture();
  media.revision = 4;
  finish(original);
  const frame = await pending;
  assert.equal(frame.info.input_revision, 0);
  assert.equal(media.age(frame).info.input_revision, 0);
});

test("parallel opens reserve global capacity and release failed reservations", async (t) => {
  const { core, identity: i, device } = await fixture(t);
  core.limits.maxSessions = 2;
  const results = await Promise.all(
    Array.from({ length: 8 }, () =>
      core.tool(i, "open_computer", {
        computer_id: device.device_id,
        mode: "observe",
      }),
    ),
  );
  assert.equal(results.filter((r) => r.ok).length, 2);
  assert.equal(core.sessions.size, 2);
  for (const session of [...core.sessions.values()]) await core.close(session);
  const bad = await core.tool(i, "open_computer", {
    computer_id: "missing",
    mode: "observe",
  });
  assert.equal(bad.ok, false);
  assert.equal(
    (
      await core.tool(i, "open_computer", {
        computer_id: device.device_id,
        mode: "observe",
      })
    ).ok,
    true,
  );
});

test("full journal still releases control without forgetting old execution IDs", async (t) => {
  const { core, identity: i, device } = await fixture(t);
  const opened = await core.tool(i, "open_computer", {
    computer_id: device.device_id,
    mode: "control",
    request_id: "open",
  });
  const session_id = opened.session!.session_id;
  for (let n = 0; n < 256; n++)
    assert.equal(
      (
        await core.tool(i, "computer_control", {
          session_id,
          request_id: String(n),
          operation: "renew",
        })
      ).ok,
      true,
    );
  assert.equal(
    (
      await core.tool(i, "computer_control", {
        session_id,
        request_id: "0",
        operation: "renew",
      })
    ).ok,
    true,
  );
  assert.equal(
    (
      await core.tool(i, "computer_control", {
        session_id,
        request_id: "0",
        operation: "acquire",
      })
    ).error?.code,
    "REQUEST_CONFLICT",
  );
  const result = await core.tool(i, "computer_control", {
    session_id,
    request_id: "release",
    operation: "release",
  });
  assert.equal(result.ok, true);
  assert.equal(result.session_closed, true);
  assert.equal(core.get(device.device_id).lease, undefined);
  assert.equal(core.sessions.has(session_id), false);
});

test("repeated absent-signal frames never report ready", async (t) => {
  const { core, device } = await fixture(t),
    r = core.get(device.device_id);
  await core.connect(r);
  const raw = await r.driver.snapshot(AbortSignal.timeout(1000));
  for (let n = 0; n < 4; n++) {
    r.media.ingest({ ...raw, signal: "absent" });
    assert.equal(r.status, "no_signal");
  }
  r.media.ingest({ ...raw, signal: "present" });
  assert.equal(r.status, "ready");
});

test("failed and closed database workers reject subsequent requests", async (t) => {
  const { dir, store } = await fixture(t);
  const broken = new Store(join(dir, "missing-directory"));
  t.after(() => broken.close());
  await assert.rejects(broken.list("devices"), /STORAGE_ERROR|unavailable/);
  await assert.rejects(broken.list("devices"), /STORAGE_ERROR|unavailable/);
  assert.equal(broken.state, "failed");
  assert.equal((await store.health()).state, "ready");
  await store.close();
  await assert.rejects(store.list("devices"), /STORAGE_ERROR/);
});

test("credential replacement conflicts and device deletion clean up secrets", async (t) => {
  const { core, identity: i, secrets, store } = await fixture(t);
  const d = await core.saveDevice(
    i,
    { name: "Credentials", driver_id: "simulator", transport: "simulator" },
    { password: "synthetic" },
    "synthetic-ca",
  );
  const {
    status,
    error,
    capabilities,
    lease,
    control_suspended,
    consumers,
    media,
    ...config
  } = d;
  const results = await Promise.allSettled([
    core.saveDevice(i, config, { password: "one" }, "one"),
    core.saveDevice(i, config, { password: "two" }, "two"),
  ]);
  assert.equal(results.filter((r) => r.status === "fulfilled").length, 1);
  const current = core.get(d.device_id).device;
  assert.equal(
    (await readdir(secrets.dir)).filter((f) => f.endsWith(".secret")).length,
    2,
  );
  assert.ok(await secrets.get(current.secret_ref));
  await core.deleteDevice(i, current.device_id, current.revision, true);
  assert.equal(
    (await readdir(secrets.dir)).filter((f) => f.endsWith(".secret")).length,
    0,
  );
});

test("offline secret cleanup preserves referenced and recent files", async (t) => {
  const { secrets } = await fixture(t);
  const old = `ca-${randomUUID()}`,
    referenced = `ca-${randomUUID()}`,
    recent = `ca-${randomUUID()}`;
  for (const ref of [old, referenced, recent])
    await secrets.set(ref, { pem: "synthetic" });
  const yesterday = new Date(Date.now() - 2 * 86400000);
  for (const ref of [old, referenced])
    await utimes(secrets.path(ref), yesterday, yesterday);
  assert.deepEqual(await secrets.prune(new Set([referenced])), [old]);
  assert.ok(await secrets.get(referenced));
  assert.ok(await secrets.get(recent));
});

test("duplicate plugin subscriptions cannot overwrite cancellation handles", async (t) => {
  const { core, plugins, device } = await fixture(t);
  const entry = {
    id: "fixture",
    enabled: true,
    manifest: { permissions: ["video:read"] },
    devices: [device.device_id],
    subscriptions: new Map(),
    process: { send: () => true },
  };
  const host = plugins as any;
  await host.message(entry, {
    type: "subscribe",
    id: "same",
    source: "video",
    device_id: device.device_id,
  });
  const first = entry.subscriptions.get("same");
  await assert.rejects(
    host.message(entry, {
      type: "subscribe",
      id: "same",
      source: "video",
      device_id: device.device_id,
    }),
    /Subscription ID/,
  );
  assert.equal(entry.subscriptions.get("same"), first);
  await host.message(entry, { type: "unsubscribe", id: "same" });
  assert.equal(first.signal.aborted, true);
  await delay(400);
  assert.equal(core.get(device.device_id).media.consumers.size, 0);
});

test("LAN ICE disables implicit public STUN and rejects external candidates", async () => {
  const peer = new RTCPeerConnection({ iceServers: [] });
  try {
    peer.addTransceiver("audio", { direction: "recvonly" });
    localIceOnly(peer);
    assert.ok(peer.iceTransports.length);
    assert.ok(
      peer.iceTransports.every((t) => t.connection.stunServer === undefined),
    );
    checkIceCandidate("candidate:1 1 UDP 1 192.168.1.2 9000 typ host");
    assert.throws(
      () => checkIceCandidate("candidate:1 1 UDP 1 8.8.8.8 9000 typ host"),
      /local\/LAN/,
    );
    assert.throws(
      () =>
        checkIceDescription(
          "v=0\r\na=candidate:1 1 UDP 1 8.8.8.8 9000 typ host\r\n",
        ),
      /local\/LAN/,
    );
  } finally {
    await peer.close();
  }
});

test("Janus mixed ICE offers preserve LAN candidates and SDP while removing external addresses", () => {
  const lines = [
    "v=0",
    "m=audio 9 UDP/TLS/RTP/SAVPF 111",
    "a=candidate:1 1 UDP 1 10.111.0.11 9000 typ host",
    "a=candidate:2 1 UDP 1 8.8.8.8 9001 typ srflx raddr 10.111.0.11 rport 9000",
    "a=candidate:3 1 UDP 1 fd00::11 9002 typ host",
    "a=candidate:4 1 UDP 1 2001:4860::1 9003 typ host",
    "a=candidate:5 1 UDP 1 untrusted.example 9004 typ host",
    "a=sendonly",
    "",
  ];
  const filtered = localIceDescription(lines.join("\r\n"));
  assert.equal(
    filtered,
    [lines[0], lines[1], lines[2], lines[4], lines[7], ""].join("\r\n"),
  );
  assert.doesNotThrow(() => checkIceDescription(filtered));
  assert.equal(localIceDescription(filtered), filtered);
  assert.equal(localIceDescription("v=0\na=candidate:broken\n"), "v=0\r\n");
});

test("input queue is bounded, merges scrolls and cancels pending input", async () => {
  let finish!: () => void;
  const calls: unknown[] = [];
  const queue = new InputQueue(async (actions) => {
    calls.push(actions);
    await new Promise<void>((resolve) => {
      finish = resolve;
    });
  }, 2);
  const active = queue.enqueue([{ type: "keypress", keys: ["KeyA"] }]);
  const scroll = {
    type: "scroll" as const,
    x: 1,
    y: 1,
    scroll_x: 0,
    scroll_y: 100,
  };
  const a = queue.enqueue([scroll]),
    b = queue.enqueue([scroll]);
  const c = queue.enqueue([{ type: "keypress", keys: ["KeyB"] }]);
  assert.equal(queue.pending, 2);
  assert.equal(
    await queue.enqueue([{ type: "keypress", keys: ["KeyC"] }]),
    false,
  );
  queue.cancel();
  finish();
  assert.deepEqual(await Promise.all([active, a, b, c]), [
    true,
    false,
    false,
    false,
  ]);
  assert.equal(calls.length, 1);
});

test("a reconnect discards an older in-flight capture and format changes invalidate views", async (t) => {
  const { core, device } = await fixture(t),
    r = core.get(device.device_id);
  const raw = await r.driver.snapshot(AbortSignal.timeout(1000));
  let finish!: (value: typeof raw) => void;
  r.driver.snapshot = () =>
    new Promise((resolve) => {
      finish = resolve;
    });
  const media = new Media(device.device_id, r.driver);
  const pending = (media as any).capture();
  media.reconnect();
  finish(raw);
  await assert.rejects(pending, /VIEW_CHANGED/);
  assert.equal(media.latest, undefined);
  const a = media.ingest(raw),
    b = media.ingest({ ...raw, mime_type: "image/jpeg" });
  assert.notEqual(a.info.view_id, b.info.view_id);
});

test("input queue executes merged scroll distance once", async () => {
  let finish!: () => void;
  const calls: any[] = [];
  const queue = new InputQueue(async (actions) => {
    calls.push(actions);
    if (calls.length === 1)
      await new Promise<void>((resolve) => {
        finish = resolve;
      });
  });
  const active = queue.enqueue([{ type: "keypress", keys: ["KeyA"] }]);
  const scroll = {
    type: "scroll" as const,
    x: 1,
    y: 1,
    scroll_x: 0,
    scroll_y: 100,
  };
  const a = queue.enqueue([scroll]),
    b = queue.enqueue([scroll]);
  finish();
  assert.deepEqual(await Promise.all([active, a, b]), [true, true, true]);
  assert.equal(calls.length, 2);
  assert.equal(calls[1][0].scroll_y, 200);
});

test("mouse movement coalesces only across adjacent moves and preserves clicks", async () => {
  let finish!: () => void;
  const calls: any[] = [];
  const queue = new InputQueue(async (actions) => {
    calls.push(actions);
    if (calls.length === 1)
      await new Promise<void>((resolve) => {
        finish = resolve;
      });
  });
  const pending = [queue.enqueue([{ type: "move", x: 0, y: 0 }])];
  pending.push(queue.enqueue([{ type: "move", x: 1, y: 1 }]));
  pending.push(queue.enqueue([{ type: "move", x: 9, y: 9 }]));
  pending.push(queue.enqueue([{ type: "click", x: 9, y: 9, button: "left" }]));
  pending.push(queue.enqueue([{ type: "move", x: 10, y: 10 }]));
  finish();
  assert.deepEqual(await Promise.all(pending), [true, true, true, true, true]);
  assert.deepEqual(
    calls.map((a) => a[0].type),
    ["move", "move", "click", "move"],
  );
  assert.equal(calls[1][0].x, 9);
});

test("input queue reports rejected and skipped execution without claiming delivery", async () => {
  const skipped = new InputQueue(async () => false);
  assert.equal(
    await skipped.enqueue([{ type: "type", text: "preserve draft" }]),
    false,
  );
  const failed = new InputQueue(async () => {
    throw Error("offline");
  });
  assert.equal(
    await failed.enqueue([{ type: "type", text: "preserve draft" }]),
    false,
  );
});
