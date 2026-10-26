// A known three-process, allocated-memory, CPU-active tree validates that the
// monitor includes descendants without attributing its own harness to the app.
import assert from "node:assert/strict";
import { fork } from "node:child_process";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

if (process.argv[2] === "--child") {
  const depth = Number(process.argv[3]);
  const allocation = Buffer.alloc(16 * 1024 * 1024, 123);
  const child = depth
    ? fork(fileURLToPath(import.meta.url), ["--child", String(depth - 1)], {
        windowsHide: true,
        stdio: ["ignore", "ignore", "inherit", "ipc"],
      })
    : undefined;
  const work = setInterval(() => {
    assert.equal(allocation[0], 123);
    if (!depth) {
      const end = performance.now() + 75;
      while (performance.now() < end) {}
    }
  }, 100);
  let stopping = false;
  async function stop() {
    if (stopping) return;
    stopping = true;
    clearInterval(work);
    if (child) {
      const exited = new Promise((r) => child.once("exit", r));
      child.send("stop");
      await exited;
    }
    process.exit(0);
  }
  process.on("message", (m) => {
    if (m === "stop") void stop();
  });
  process.on("disconnect", () => void stop());
  if (child) child.once("message", () => process.send("ready"));
  else process.send("ready");
} else {
  process.env.KVMHELM_BENCH_SECONDS = "4";
  const { phase } = await import("./runtime-measurements.mjs");
  const root = fork(fileURLToPath(import.meta.url), ["--child", "2"], {
    windowsHide: true,
    stdio: ["ignore", "ignore", "inherit", "ipc"],
  });
  try {
    await new Promise((r, reject) => {
      root.once("message", r);
      root.once("error", reject);
    });
    const measured = await phase("sampler_selfcheck", root.pid, (ms) =>
      delay(ms),
    );
    assert.equal(measured.process_count.min, 3);
    assert.equal(measured.process_count.max, 3);
    assert.ok(measured.private_commit_bytes.min > 48 * 1024 * 1024);
    assert.ok(measured.cpu_core_percent > 5 && measured.cpu_core_percent < 180);
    console.log(
      "PASS: only the owned three-process tree, allocated memory and descendant CPU are measured",
    );
  } finally {
    const exited = new Promise((r) => root.once("exit", r));
    root.send("stop");
    await exited;
  }
}
