// Optional transport only. The Rust host owns permissions, persistence, panels,
// subscriptions and actions. Real Node fork/advanced IPC preserves Buffer data.
"use strict";
const { fork } = require("node:child_process");
const children = new Map();
const MAX = 16 * 1024 * 1024,
  OUT = 1024 * 1024;
let buffer = Buffer.alloc(0),
  queued = 0,
  writing = false,
  queue = [],
  stopping = false;
function output(value) {
  let text;
  try {
    text = Buffer.from(JSON.stringify(value));
  } catch {
    return false;
  }
  if (
    text.length > OUT ||
    queued + text.length > 2 * OUT ||
    queue.length >= 256
  )
    return false;
  const frame = Buffer.allocUnsafe(text.length + 4);
  frame.writeUInt32LE(text.length);
  text.copy(frame, 4);
  queue.push(frame);
  queued += frame.length;
  flush();
  return true;
}
function flush() {
  if (writing || !queue.length) return;
  writing = true;
  const frame = queue.shift();
  process.stdout.write(frame, () => {
    queued -= frame.length;
    writing = false;
    flush();
  });
}
function kill(id) {
  const entry = children.get(id);
  if (entry) {
    entry.child.kill("SIGKILL");
  }
}
function shutdown() {
  if (stopping) return;
  stopping = true;
  for (const id of children.keys()) kill(id);
  setTimeout(() => process.exit(0), 100).unref();
}
function input(m) {
  if (m.type === "shutdown") return shutdown();
  if (m.type === "kill") return kill(m.id);
  if (m.type === "launch") {
    if (children.size >= 128 || children.has(m.id)) throw Error("limit");
    const child = fork(m.entry, [], {
      cwd: m.path,
      serialization: "advanced",
      stdio: ["ignore", "ignore", "ignore", "ipc"],
      execArgv: [],
      env: Object.fromEntries(
        ["PATH", "SystemRoot", "TEMP", "HOME"]
          .filter((k) => process.env[k] !== undefined)
          .map((k) => [k, process.env[k]]),
      ),
    });
    const entry = { child, bytes: 0 };
    children.set(m.id, entry);
    child.on("message", (message) => {
      if (!output({ type: "message", id: m.id, message })) {
        kill(m.id);
      }
    });
    child.on("error", () => {
      output({ type: "exit", id: m.id });
      kill(m.id);
    });
    child.once("exit", () => {
      children.delete(m.id);
      if (!output({ type: "exit", id: m.id })) shutdown();
    });
    return;
  }
  if (m.type !== "send") throw Error("command");
  const entry = children.get(m.id);
  if (!entry || !entry.child.connected) {
    output({ type: "delivered", request: m.request, ok: false });
    return;
  }
  const bytes = Buffer.byteLength(JSON.stringify(m));
  if (m.bytes !== undefined)
    m.message.chunk.data = Buffer.from(m.bytes, "base64");
  if (entry.bytes + bytes > MAX) {
    output({ type: "delivered", request: m.request, ok: false });
    kill(m.id);
    return;
  }
  entry.bytes += bytes;
  let accepted = true;
  try {
    accepted = entry.child.send(m.message, (error) => {
      entry.bytes -= bytes;
      if (
        !output({
          type: "delivered",
          request: m.request,
          ok: !error && accepted,
        })
      )
        shutdown();
    });
  } catch {
    entry.bytes -= bytes;
    output({ type: "delivered", request: m.request, ok: false });
    kill(m.id);
  }
}
process.stdin.on("data", (data) => {
  if (buffer.length + data.length > MAX + 4) return shutdown();
  buffer = buffer.length ? Buffer.concat([buffer, data]) : data;
  while (buffer.length >= 4) {
    const length = buffer.readUInt32LE();
    if (length > MAX) return shutdown();
    if (buffer.length < length + 4) return;
    const frame = buffer.subarray(4, 4 + length);
    buffer = buffer.subarray(4 + length);
    try {
      input(JSON.parse(frame.toString("utf8")));
    } catch {
      return shutdown();
    }
  }
});
process.stdin.on("end", shutdown);
process.stdin.on("error", shutdown);
process.stdout.on("error", shutdown);
