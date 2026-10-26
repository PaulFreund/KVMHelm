// Measurement helpers, not an application runtime dependency. Windows counters
// cover the daemon and every observed descendant, excluding this harness/peers.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { get } from "node:http";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { performance } from "node:perf_hooks";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

export const benchmark = !!process.env.KVMHELM_BENCH_OUTPUT;
export const reference = process.env.KVMHELM_TEST_DAEMON === "typescript";
export const seconds = Number(process.env.KVMHELM_BENCH_SECONDS ?? 20);
export const warmup = Number(process.env.KVMHELM_BENCH_WARMUP ?? 5);
assert.ok(seconds >= 4 && seconds <= 120 && warmup >= 0 && warmup <= 30);
const script = fileURLToPath(
  new URL("./measure-process-tree.ps1", import.meta.url),
);
export const digest = (bytes) =>
  createHash("sha256").update(bytes).digest("hex");
export function normalizeBenchmarkEnvironment(env) {
  for (const key of Object.keys(env))
    if (
      [
        "NODE_OPTIONS",
        "NODE_EXTRA_CA_CERTS",
        "NODE_INSPECT_RESUME_ON_START",
        "UV_THREADPOOL_SIZE",
        "TOKIO_WORKER_THREADS",
      ].includes(key.toUpperCase())
    )
      delete env[key];
  env.NODE_ENV = "production";
}
export function stats(values) {
  assert.ok(values.length);
  const sorted = [...values].sort((a, b) => a - b);
  const percentile = (p) =>
    sorted[Math.min(sorted.length - 1, Math.ceil(p * sorted.length) - 1)];
  return {
    n: values.length,
    min: sorted[0],
    p50: percentile(0.5),
    p95: percentile(0.95),
    max: sorted.at(-1),
    mean: values.reduce((a, b) => a + b, 0) / values.length,
  };
}
export function checked(value) {
  assert.equal(value.isError, false, JSON.stringify(value.structuredContent));
  assert.equal(value.structuredContent.ok, true);
  return value.structuredContent;
}
export async function traffic({ tool, session, frame, ms, images = true }) {
  const deadline = performance.now() + ms;
  const latencies = {},
    imageBytes = [],
    counts = {};
  let next = performance.now(),
    sequence = 0,
    missed = 0;
  while (performance.now() < deadline) {
    const kind = images
      ? ["fresh_screenshot", "cached_screenshot", "click_and_image"][
          sequence++ % 3
        ]
      : "screenshot";
    const name =
      kind === "click_and_image" ? "computer" : "computer_screenshot";
    const args =
      kind === "click_and_image"
        ? {
            session_id: session,
            request_id: randomUUID(),
            reference: {
              frame_id: frame.frame_id,
              view_id: frame.view_id,
              input_revision: frame.input_revision,
            },
            actions: [{ type: "click", x: 0, y: 0 }],
          }
        : {
            session_id: session,
            max_age_ms: kind === "cached_screenshot" ? 5000 : 0,
          };
    const start = performance.now();
    const response = await tool(name, args);
    const elapsed = performance.now() - start;
    const body = checked(response);
    frame = body.frames.at(-1);
    assert.ok(frame && frame.width > 0 && frame.height > 0);
    (latencies[kind] ??= []).push(elapsed);
    counts[kind] = (counts[kind] ?? 0) + 1;
    imageBytes.push(
      response.content
        .filter((c) => c.type === "image")
        .reduce((n, c) => n + Buffer.byteLength(c.data, "base64"), 0),
    );
    next += 250;
    if (next < performance.now()) {
      missed++;
      next = performance.now();
    }
    await delay(Math.max(0, Math.min(next, deadline) - performance.now()));
  }
  return {
    latencies_ms: Object.fromEntries(
      Object.entries(latencies).map(([k, v]) => [k, stats(v)]),
    ),
    counts,
    image_bytes: stats(imageBytes),
    cadence_misses: missed,
    frame,
  };
}
export async function stream(
  url,
  token,
  session,
  paused = false,
  client = "runtime-benchmark",
) {
  let response;
  const result = { bytes: 0, boundaries: 0 };
  const request = get(
    url + "/api/v1/sessions/" + session + "/video",
    {
      headers: {
        authorization: "Bearer " + token,
        ...(client ? { "x-kvmhelm-client": client } : {}),
      },
    },
    (r) => {
      response = r;
    },
  );
  await new Promise((done, reject) => {
    request.once("response", (r) => {
      if (r.statusCode !== 200) {
        r.resume();
        reject(Error("Stream returned HTTP " + r.statusCode));
        return;
      }
      if (paused) r.pause();
      else {
        let tail = "";
        r.on("data", (bytes) => {
          result.bytes += bytes.length;
          const text = tail + bytes.toString("latin1");
          result.boundaries += (text.match(/--kvmhelm\r\n/g) ?? []).length;
          tail = text.slice(-10);
        });
      }
      done();
    });
    request.once("error", reject);
  });
  return {
    result,
    close: () => {
      response?.destroy();
      request.destroy();
    },
  };
}
export async function phase(name, rootPid, work) {
  assert.equal(
    process.platform,
    "win32",
    "This sampler measures Windows; do not relabel its results as other platforms",
  );
  const samples = [];
  let errorText = "",
    lines = "",
    ready;
  const first = new Promise((resolve) => {
    ready = resolve;
  });
  const sampler = spawn(
    "pwsh.exe",
    [
      "-NoProfile",
      "-File",
      script,
      "-RootProcessId",
      String(rootPid),
      "-Seconds",
      String(seconds),
      "-IntervalMs",
      "1000",
    ],
    { windowsHide: true, stdio: ["ignore", "pipe", "pipe"] },
  );
  const ended = new Promise((done, reject) =>
    sampler.once("exit", (code) => {
      code === 0
        ? done()
        : reject(Error("Process sampler failed: " + errorText));
    }),
  );
  // Observe rejection immediately, even when readiness/work is still pending.
  ended.catch(() => {});
  sampler.stderr.on("data", (bytes) => {
    errorText = (errorText + bytes).slice(-4000);
  });
  sampler.stdout.on("data", (bytes) => {
    lines += bytes;
    let newline;
    while ((newline = lines.indexOf("\n")) >= 0) {
      const line = lines.slice(0, newline).trim();
      lines = lines.slice(newline + 1);
      if (!line) continue;
      const sample = JSON.parse(line);
      samples.push(sample);
      ready();
    }
  });
  try {
    await Promise.race([
      first,
      ended.then(() => {
        throw Error("No resource samples");
      }),
      delay(30000, undefined, { ref: false }).then(() => {
        throw Error("Sampler readiness timeout");
      }),
    ]);
    const measured = await work(seconds * 1000);
    await Promise.race([
      ended,
      delay((seconds + 30) * 1000, undefined, { ref: false }).then(() => {
        throw Error("Sampler completion timeout");
      }),
    ]);
    assert.ok(samples.length >= 3);
    const begin = samples[0],
      end = samples.at(-1),
      span = end.elapsed_ms - begin.elapsed_ms;
    const systemTotal = end.system_total_ticks - begin.system_total_ticks;
    const processes = [
      ...new Set(samples.flatMap((s) => s.processes.map((p) => p.name))),
    ].sort();
    const report = {
      name,
      requested_seconds: seconds,
      resource_span_seconds: span / 1000,
      working_set_bytes: stats(samples.map((s) => s.resident_bytes)),
      private_commit_bytes: stats(samples.map((s) => s.private_bytes)),
      cpu_core_percent:
        ((end.cpu_observed_ms - begin.cpu_observed_ms) / span) * 100,
      host_cpu_percent:
        (1 - (end.system_idle_ticks - begin.system_idle_ticks) / systemTotal) *
        100,
      process_count: stats(samples.map((s) => s.processes.length)),
      process_names: processes,
      memory_first_third_mean: stats(
        samples
          .slice(0, Math.max(1, Math.floor(samples.length / 3)))
          .map((s) => s.private_bytes),
      ).mean,
      memory_last_third_mean: stats(
        samples
          .slice(-Math.max(1, Math.floor(samples.length / 3)))
          .map((s) => s.private_bytes),
      ).mean,
      workload: measured,
    };
    if (report.workload) delete report.workload.frame;
    console.log(
      JSON.stringify({
        phase: name,
        cpu_core_percent: report.cpu_core_percent,
        working_set_mib: report.working_set_bytes.mean / 1048576,
        process_names: processes,
      }),
    );
    return report;
  } finally {
    if (sampler.exitCode === null && sampler.signalCode === null)
      sampler.kill();
  }
}
export async function save(report) {
  const path = resolve(process.env.KVMHELM_BENCH_OUTPUT);
  await mkdir(dirname(path), { recursive: true });
  await writeFile(
    path,
    JSON.stringify(
      {
        schemaVersion: 1,
        date: new Date().toISOString(),
        platform: process.platform,
        runtime: reference ? "typescript" : "native",
        application_environment:
          "NODE_ENV=production; default Node/Tokio worker settings; no NODE_OPTIONS or ambient KVMHELM configuration",
        seconds,
        warmup,
        ...report,
      },
      null,
      2,
    ) + "\n",
    { flag: "wx" },
  );
}
export async function installMeasurementPlugin(
  api,
  directory,
  device,
  source = "video",
) {
  await mkdir(directory);
  const manifest = {
    id: "benchmark.observer",
    version: "1",
    apiVersion: "kvmhelm.plugin/v1",
    kind: "feature",
    runtime: "external-process",
    entry: "index.mjs",
    permissions: [
      "ui:panel",
      source === "video" ? "video:read" : "audio:read:from_target",
    ],
    uiSlots: ["kvm.sidepanel"],
  };
  await writeFile(directory + "/kvmhelm.plugin.json", JSON.stringify(manifest));
  await writeFile(
    directory + "/index.mjs",
    `let device,count=0;process.on('message',m=>{if(m.type==='initialize'){device=m.devices[0];process.send({type:'ready'});}if(m.type==='start')process.send({type:'subscribe',id:'source',source:${JSON.stringify(source)},device_id:device});if(m.type==='health')process.send({type:'health'});if(m.type==='media'){if(!Buffer.isBuffer(m.chunk.data))process.exit(31);if(++count===1)process.send({type:'panel',panel:{id:'proof',title:'IPC proof',slot:'kvm.sidepanel',device_id:device,text:'Buffer received',actions:[]}});}if(m.type==='dispose')process.exit(0);});`,
  );
  await api("/plugins", "POST", { path: directory, devices: [device] });
  const deadline = performance.now() + 15000;
  while (performance.now() < deadline) {
    if ((await api("/plugins"))[0]?.panels.some((p) => p.id === "proof"))
      return;
    await delay(50);
  }
  throw Error("Measurement plugin did not receive a Buffer");
}
