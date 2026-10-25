// Reference implementation for Rust compatibility tests. All state is supplied
// by the test in a temporary directory; no configured/live gateway is contacted.
import assert from "node:assert/strict";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { Store, Secrets } from "../dist/server/store.js";
import { Auth, tokenSchema } from "../dist/server/auth.js";
import { Core } from "../dist/server/core.js";
import {
  deviceSchema,
  actionSchema,
  toolSchemas,
  scopes,
} from "../dist/shared/contracts.js";
import { keyName, textKeys } from "../dist/server/drivers/keyboard.js";
import { usbKey } from "../dist/server/drivers/usb-key.js";
import { isLocalAddress } from "../dist/server/network.js";
import { PiKvmDriver } from "../dist/server/drivers/pikvm.js";
import { H264Assembler } from "../dist/server/drivers/h264.js";
import { mcpServer } from "../dist/server/mcp.js";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import {
  LATEST_PROTOCOL_VERSION,
  SUPPORTED_PROTOCOL_VERSIONS,
} from "@modelcontextprotocol/sdk/types.js";
import { ipKeyGenerator } from "express-rate-limit";

const [command, dir] = process.argv.slice(2);
const deviceId = "11111111-1111-4111-8111-111111111111";
const secretRef = `${deviceId}-22222222-2222-4222-8222-222222222222`;
const secret = {
  username: "admin",
  password: "fixture-ä🙂",
  nested: [true, 123],
};

async function storage() {
  assert.ok(dir, "explicit temporary directory required");
  await mkdir(join(dir, "data"), { recursive: true, mode: 0o700 });
  const store = new Store(join(dir, "data"));
  const secrets = await Secrets.open(join(dir, "secrets"));
  const auth = new Auth(store);
  await auth.load();
  try {
    if (command === "prepare") {
      const token = await auth.create({
        name: "legacy fixture",
        scopes: [...scopes],
        devices: ["*"],
      });
      await writeFile(join(dir, "legacy.pat"), token.token, {
        mode: 0o600,
        flag: "wx",
      });
      await secrets.set(secretRef, secret);
      await secrets.set("legacy-ca", { pem: "fixture certificate text" });
      await store.put(
        "devices",
        deviceId,
        deviceSchema.parse({
          device_id: deviceId,
          name: "Legacy fixture",
          driver_id: "simulator",
          transport: "simulator",
          secret_ref: secretRef,
          tls: { ca_ref: "legacy-ca" },
          keyboard_layout: "de",
          tags: ["compatibility"],
        }),
      );
      await store.put("preferences", "owner", {
        theme: "dark",
        sidebar: true,
        selected: deviceId,
        order: [deviceId],
        visible: [deviceId],
        tile_size: 360,
      });
      await store.put("plugins", "fixture", {
        id: "fixture",
        enabled: false,
        manifest: { permissions: [] },
        source: "fixture",
        revision: 0,
      });
    } else {
      assert.equal(command, "verify");
      assert.deepEqual(await secrets.get(secretRef), {
        ...secret,
        native: true,
      });
      assert.deepEqual(await secrets.get("native-secret"), {
        unicode: "Grüße 🙂",
        count: 7,
      });
      assert.equal(
        (await store.get("devices", deviceId)).name,
        "Native fixture",
      );
      assert.equal((await store.get("devices", deviceId)).revision, 2);
      assert.equal((await store.get("preferences", "owner")).theme, "light");
      const pat = await readFile(join(dir, "native.pat"), "utf8");
      const identity = auth.authenticate(pat, "legacy-client");
      auth.check(identity, "devices:read", deviceId);
      assert.equal(identity.token.name, "native fixture");
      assert.throws(
        () => auth.authenticate(tokenText, "client"),
        /AUTH_FAILED/,
      );
    }
  } finally {
    await store.close();
  }
}
const tokenText =
  command === "verify" ? await readFile(join(dir, "legacy.pat"), "utf8") : "";

function contracts() {
  const cases = [];
  function add(kind, input) {
    const schema =
      kind === "device"
        ? deviceSchema
        : kind === "action"
          ? actionSchema
          : kind === "token"
            ? tokenSchema
            : toolSchemas[kind];
    const parsed = schema.safeParse(input);
    cases.push({
      kind,
      input,
      ok: parsed.success,
      ...(parsed.success ? { normalized: parsed.data } : {}),
    });
  }
  for (const input of [
    { name: "test" },
    { name: "test", minimum_action_latency_ms: 1.0 },
    { name: "🙂".repeat(50) },
    { name: "🙂".repeat(51) },
    { name: "🙂".repeat(100) },
    { name: "🙂".repeat(101) },
    { name: "test", device_id: deviceId },
    { name: "test", device_id: "00000000-0000-0000-0000-000000000000" },
    { name: "test", device_id: "ffffffff-ffff-ffff-ffff-ffffffffffff" },
    { name: "test", device_id: "11111111-1111-0111-8111-111111111111" },
    { name: "test", address: "https://kvm.lan" },
    { name: "test", address: "not a URL" },
    { name: "test", tls: { fingerprint: "ab", insecure: false } },
    { name: "test", tls: [] },
  ])
    add("device", input);
  for (const [field, value] of [
    ["unknown", true],
    ["name", ""],
    ["tags", Array(51).fill("x")],
    ["tags", ["🙂".repeat(21)]],
    ["minimum_action_latency_ms", 5001],
    ["minimum_action_latency_ms", 0.25],
    ["revision", 9007199254740992],
  ])
    add("device", { name: "test", [field]: value });
  for (const field of [
    "device_id",
    "driver_id",
    "model",
    "firmware",
    "address",
    "secret_ref",
    "tls",
    "tags",
    "enabled",
    "media_profile",
    "keyboard_layout",
    "transport",
    "revision",
    "minimum_action_latency_ms",
  ])
    add("device", { name: "test", [field]: null });
  const actions = [
    { type: "click", x: 1, y: 2 },
    { type: "click", x: 1, y: 2, button: "right" },
    { type: "double_click", x: 1, y: 2 },
    { type: "move", x: 1.0, y: 0 },
    {
      type: "drag",
      path: [
        { x: 1, y: 2 },
        { x: 3, y: 4 },
      ],
    },
    { type: "scroll", x: 0, y: 0, scroll_x: -1000, scroll_y: 1000 },
    { type: "keypress", keys: ["CTRL", "a"] },
    { type: "type", text: "Grüße 🙂\r\n" },
    { type: "wait" },
    { type: "wait", duration_ms: 5000 },
    { type: "screenshot" },
  ];
  for (const action of actions) {
    add("action", action);
    add("action", { ...action, unknown: true });
  }
  for (const input of [
    { type: "click", x: -1, y: 0 },
    { type: "click", x: 1.5, y: 0 },
    { type: "drag", path: [{ x: 1, y: 2 }] },
    { type: "keypress", keys: [] },
    { type: "keypress", keys: Array(7).fill("a") },
    { type: "keypress", keys: ["x".repeat(41)] },
    { type: "type", text: "🙂".repeat(4096) },
    { type: "type", text: "🙂".repeat(4097) },
    { type: "wait", duration_ms: 5001 },
    { type: "screenshot", before_ms: 0 },
    { type: "scroll", x: 0, y: 0, scroll_x: 0, scroll_y: 1001 },
    {
      type: "drag",
      path: [
        [1, 2],
        [3, 4],
      ],
    },
  ])
    add("action", input);
  const tools = {
    list_computers: [{}, { tags: ["test"] }, { tags: null }, { bad: true }],
    open_computer: [
      { computer_id: deviceId, mode: "observe" },
      {
        computer_id: deviceId,
        mode: "control",
        request_id: "open",
        profile: "active",
      },
      { computer_id: deviceId, mode: "control", profile: null },
    ],
    computer: [
      {
        session_id: "s",
        request_id: "r",
        reference: { frame_id: "f", view_id: "v", input_revision: 1 },
        actions,
      },
      {
        session_id: "s",
        request_id: "r",
        reference: { frame_id: "f", view_id: "v", input_revision: 1 },
        actions: [{ type: "wait" }],
        observation: {},
      },
      {
        session_id: "s",
        request_id: "r",
        reference: { frame_id: "f", view_id: "v", input_revision: 1 },
        actions: [{ type: "wait" }],
        observation: { mode: "none", timeout_ms: 50 },
      },
      {
        session_id: "s",
        request_id: "r",
        reference: { frame_id: "f", view_id: "v", input_revision: 1 },
        actions: [],
      },
    ],
    computer_screenshot: [
      { session_id: "s" },
      { session_id: "s", max_age_ms: 5000, timeout_ms: 10000 },
      { session_id: "s", max_age_ms: 5001 },
      { session_id: "s", timeout_ms: 49 },
    ],
    computer_control: ["acquire", "renew", "release", "invalid"].map(
      (operation) => ({ session_id: "s", request_id: "r", operation }),
    ),
    close_computer: [{ session_id: "s" }, { session_id: "s", extra: true }],
  };
  for (const [kind, inputs] of Object.entries(tools)) {
    for (const input of inputs) add(kind, input);
    for (const input of [
      null,
      [],
      "text",
      {},
      { ...inputs[0], request_id: "" },
      { ...inputs[0], request_id: "r".repeat(129) },
    ])
      add(kind, input);
  }
  for (const input of [
    { name: "t", scopes: ["devices:read"], devices: ["*"] },
    { name: "t", scopes: ["devices:read"], devices: ["*"], expires_at: 1.5 },
    { name: "t", scopes: [], devices: ["*"] },
    { name: "t", scopes: ["invalid"], devices: ["*"] },
    { name: "t", scopes: ["devices:read"], devices: [] },
    { name: "t", scopes: ["devices:read"], devices: ["*"], extra: true },
  ])
    add("token", input);
  return cases;
}

function mapping() {
  const run = (fn, ...args) => {
    try {
      return { ok: true, value: fn(...args) };
    } catch (e) {
      return { ok: false, code: e.code };
    }
  };
  const chars = Array.from({ length: 128 }, (_, n) =>
    String.fromCharCode(n),
  ).concat([..."äöüÄÖÜß€´§🙂é"]);
  const text = ["us", "de"].flatMap((layout) =>
    chars.map((value) => ({
      layout,
      text: value,
      ...run(textKeys, value, layout),
    })),
  );
  const keys = [
    "CTRL",
    "CONTROL",
    "SHIFT",
    "ALT",
    "ALTGR",
    "META",
    "SUPER",
    "WIN",
    "ENTER",
    "RETURN",
    "ESC",
    "ESCAPE",
    "TAB",
    "SPACE",
    "UP",
    "DOWN",
    "LEFT",
    "RIGHT",
    "HOME",
    "END",
    "PAGEUP",
    "PAGEDOWN",
    "INSERT",
    "CAPSLOCK",
    "PRINTSCREEN",
    "BACKSPACE",
    "DELETE",
    ...Array.from({ length: 24 }, (_, n) => `F${n + 1}`),
    ..."abcdefghijklmnopqrstuvwxyz0123456789",
    "IntlBackslash",
    "F01",
    "F0",
    "F25",
    "keyA",
    "🙂",
    "",
  ];
  return {
    text,
    keys: keys.map((key) => ({
      key,
      dom: run(keyName, key),
      usb: run(usbKey, key),
    })),
  };
}

async function coreTranscript() {
  assert.ok(dir, "explicit temporary directory required");
  await mkdir(join(dir, "data"), { recursive: true, mode: 0o700 });
  const store = new Store(join(dir, "data"));
  const auth = new Auth(store);
  await auth.load();
  const token = await auth.create({
    name: "core fixture",
    scopes: [...scopes],
    devices: ["*"],
  });
  const a = auth.authenticate(token.token, "a"),
    b = auth.authenticate(token.token, "b");
  const core = new Core(store, await Secrets.open(join(dir, "secrets")), auth);
  const summary = (r) => ({
    ok: r.ok,
    code: r.error?.code ?? null,
    actions: r.actions ?? [],
    frames: r.images?.length ?? 0,
    mode: r.session?.mode ?? null,
    lease: !!r.lease,
    closed: r.session_closed ?? false,
    remaining: r.journal_remaining ?? r.session?.journal_remaining ?? null,
    input_revision: r.input_revision ?? null,
    suspended: r.control_suspended ?? null,
    computers: r.computers?.length ?? null,
  });
  const trace = [];
  const record = (label, r) => {
    trace.push([label, summary(r)]);
    return r;
  };
  const ref = (r) => {
    const i = r.images[0].info;
    return {
      frame_id: i.frame_id,
      view_id: i.view_id,
      input_revision: i.input_revision,
    };
  };
  try {
    await core.load();
    const device = await core.saveDevice(a, {
      name: "Core fixture",
      driver_id: "simulator",
      transport: "simulator",
      minimum_action_latency_ms: 0,
      tags: ["core"],
    });
    record("list", await core.tool(a, "list_computers", { tags: ["core"] }));
    const opened = record(
      "open",
      await core.tool(a, "open_computer", {
        computer_id: device.device_id,
        mode: "observe",
        request_id: "open",
      }),
    );
    const session_id = opened.session.session_id;
    record(
      "acquire",
      await core.tool(a, "computer_control", {
        session_id,
        request_id: "acquire",
        operation: "acquire",
      }),
    );
    const observed = record(
      "observe",
      await core.tool(a, "computer_screenshot", { session_id }),
    );
    const request = {
      session_id,
      request_id: "click",
      reference: ref(observed),
      actions: [{ type: "click", x: 10, y: 10 }],
      observation: { mode: "none" },
    };
    record("input", await core.tool(a, "computer", request));
    record("retry", await core.tool(a, "computer", request));
    record(
      "conflict",
      await core.tool(a, "computer", {
        ...request,
        actions: [{ type: "click", x: 11, y: 10 }],
      }),
    );
    record(
      "stale",
      await core.tool(a, "computer", { ...request, request_id: "stale" }),
    );
    record(
      "forbidden",
      await core.tool(b, "computer_screenshot", { session_id }),
    );
    record("stop", await core.stopControl(b, device.device_id));
    record(
      "blocked",
      await core.tool(a, "computer_control", {
        session_id,
        request_id: "blocked",
        operation: "acquire",
      }),
    );
    record("resume", await core.stopControl(b, device.device_id, true));
    record(
      "reacquire",
      await core.tool(a, "computer_control", {
        session_id,
        request_id: "reacquire",
        operation: "acquire",
      }),
    );
    record(
      "release",
      await core.tool(a, "computer_control", {
        session_id,
        request_id: "release",
        operation: "release",
      }),
    );
    record("close", await core.tool(a, "close_computer", { session_id }));
    record("lost", await core.tool(a, "computer_screenshot", { session_id }));
    return { trace, batches: core.metrics.batches };
  } finally {
    await core.shutdown();
    await store.close();
  }
}

async function hidTranscript() {
  const device = deviceSchema.parse({
    name: "HID planning fixture",
    driver_id: "pikvm-v4",
    address: "http://127.0.0.1",
    keyboard_layout: "de",
  });
  const driver = new PiKvmDriver(device, {});
  const cases = [
    {
      width: 640,
      height: 480,
      action: { type: "click", x: 639, y: 479, button: "right" },
    },
    { width: 3, height: 3, action: { type: "move", x: 1, y: 1 } },
    { width: 1, height: 1, action: { type: "move", x: 0, y: 0 } },
    {
      width: 640,
      height: 480,
      action: { type: "keypress", keys: ["CTRL", "ENTER"] },
    },
    { width: 640, height: 480, action: { type: "type", text: "Ä@´`^YZ" } },
    {
      width: 640,
      height: 480,
      action: {
        type: "drag",
        path: [
          { x: 0, y: 0 },
          { x: 10, y: 20 },
          { x: 639, y: 479 },
        ],
      },
    },
    { width: 640, height: 480, action: { type: "double_click", x: 10, y: 20 } },
    ...[50, -50, 150, -150, 1000, -1000].map((scroll_x) => ({
      width: 640,
      height: 480,
      action: { type: "scroll", x: 1, y: 1, scroll_x, scroll_y: scroll_x },
    })),
  ];
  const trace = [];
  try {
    for (const { width, height, action: raw } of cases) {
      const action = actionSchema.parse(raw),
        events = [];
      driver.event = async (event_type, event, context) => {
        context?.assertControl();
        events.push({ event_type, event });
      };
      const status = await driver.execute(action, {
        width,
        height,
        signal: AbortSignal.timeout(1000),
        assertControl() {},
      });
      trace.push({ width, height, action, status, events });
    }
    return trace;
  } finally {
    await driver.disconnect();
  }
}

if (command === "rate-keys") {
  const values = [
    "127.0.0.1",
    "192.168.1.1",
    "192.168.1.2",
    "::ffff:127.0.0.1",
    "::ffff:c0a8:101",
    "::",
    "::1",
    "2001:db8:1234:56ff::1",
    "2001:db8:1234:56aa::2",
    "2001:db8:1234:57aa::2",
    "fe80:1234:5678:12ff::9",
    "FE80:1234:5678:12AA::1",
  ];
  process.stdout.write(
    JSON.stringify(
      values.map((value) => ({ value, key: ipKeyGenerator(value) })),
    ),
  );
} else if (command === "mcp-contract") {
  // Capture the public contract from the actual SDK registration, including
  // Zod's JSON Schemas. No store, daemon or hardware is involved.
  const server = mcpServer(
    {
      tool() {
        throw Error("No tool execution in metadata fixture");
      },
    },
    {},
  );
  const client = new Client({ name: "contract fixture", version: "1" });
  const [clientTransport, serverTransport] =
    InMemoryTransport.createLinkedPair();
  try {
    await server.connect(serverTransport);
    await client.connect(clientTransport);
    const result = {
      protocolVersion: LATEST_PROTOCOL_VERSION,
      versions: SUPPORTED_PROTOCOL_VERSIONS,
      serverInfo: client.getServerVersion(),
      capabilities: client.getServerCapabilities(),
      instructions: client.getInstructions(),
      tools: (await client.listTools()).tools,
    };
    const text = JSON.stringify(result, null, 2) + "\n";
    if (dir) await writeFile(dir, text, { flag: "w" });
    else process.stdout.write(text);
  } finally {
    await client.close();
    await server.close();
  }
} else if (command === "h264") {
  const packets = [
    [65534, 1, true, "6701"],
    [65535, 2, true, "6802"],
    [0, 3, true, "6503"],
    [1, 4, false, "7c850102"],
    [2, 4, true, "7c450304"],
    [3, 5, true, "6105"],
    [3, 5, true, "6105"],
    [2, 4, true, "7c450304"],
    [5, 6, false, "7c8506"],
    [6, 6, true, "7c4507"],
    [7, 7, true, "6508"],
    [8, 8, true, "18000465"],
    [9, 9, true, "7c4509"],
    [10, 10, true, "180002680a0002670b0002650c"],
  ];
  const trace = [];
  let emitted = [],
    lost = 0;
  const assembler = new H264Assembler(
    (data, keyframe) => emitted.push({ hex: data.toString("hex"), keyframe }),
    () => lost++,
  );
  for (const [sequenceNumber, timestamp, marker, hex] of packets) {
    assembler.push({
      header: { sequenceNumber, timestamp, marker },
      payload: Buffer.from(hex, "hex"),
    });
    trace.push({
      packet: [sequenceNumber, timestamp, marker, hex],
      emitted,
      lost,
    });
    emitted = [];
    lost = 0;
  }
  process.stdout.write(JSON.stringify(trace));
} else if (command === "hid")
  process.stdout.write(JSON.stringify(await hidTranscript()));
else if (command === "core")
  process.stdout.write(JSON.stringify(await coreTranscript()));
else if (command === "prepare" || command === "verify") {
  await storage();
  process.stdout.write("ok");
} else if (command === "contracts")
  process.stdout.write(JSON.stringify(contracts()));
else if (command === "mapping") process.stdout.write(JSON.stringify(mapping()));
else if (command === "network") {
  const values = [
    "10.0.0.1",
    "127.0.0.2",
    "172.15.0.1",
    "172.16.0.1",
    "172.31.0.1",
    "172.32.0.1",
    "192.168.0.1",
    "169.254.0.1",
    "100.64.0.1",
    "8.8.8.8",
    "::1",
    "::",
    "fd12::1",
    "fe80::1",
    "2001:db8::1",
    "::ffff:192.168.0.1",
    "::ffff:8.8.8.8",
    "localhost",
  ];
  process.stdout.write(
    JSON.stringify(
      values.map((value) => ({ value, local: isLocalAddress(value) })),
    ),
  );
} else throw Error("Unknown fixture command");
