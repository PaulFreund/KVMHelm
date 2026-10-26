// Sequential paired Windows measurements. Partial runs can resume only with
// identical binaries, reference source, harness bytes and measurement settings.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFile, writeFile, mkdir, access } from "node:fs/promises";
import { join, dirname, resolve } from "node:path";
import os from "node:os";
const args = new Map();
for (let n = 2; n < process.argv.length; n += 2) {
  assert.ok(
    [
      "--binary",
      "--helpers",
      "--out",
      "--seconds",
      "--warmup",
      "--rounds",
    ].includes(process.argv[n]) && process.argv[n + 1],
  );
  args.set(process.argv[n], process.argv[n + 1]);
}
assert.equal(
  process.platform,
  "win32",
  "The resource sampler currently implements Windows counters only",
);
const binary = resolve(
  args.get("--binary") ?? "target/benchmark-package/24ae930/kvmhelm.exe",
);
const helpers = resolve(args.get("--helpers") ?? dirname(binary));
const output = resolve(
  args.get("--out") ??
    "target/runtime-measurements/windows-" +
      new Date().toISOString().replace(/[:.]/g, "-"),
);
const seconds = Number(args.get("--seconds") ?? 20),
  warmup = Number(args.get("--warmup") ?? 5),
  rounds = Number(args.get("--rounds") ?? 2);
assert.ok(
  seconds >= 4 &&
    seconds <= 120 &&
    warmup >= 0 &&
    warmup <= 30 &&
    Number.isInteger(rounds) &&
    rounds >= 1 &&
    rounds <= 3,
);
function command(program, values) {
  const result = spawnSync(program, values, {
    encoding: "utf8",
    windowsHide: true,
  });
  assert.equal(result.status, 0, result.stderr);
  return result.stdout.trim();
}
const hash = async (path) =>
  createHash("sha256")
    .update(await readFile(path))
    .digest("hex");
assert.equal(
  command("git", [
    "diff",
    "b40b2dd",
    "--name-only",
    "--",
    "server",
    "shared",
    "web",
  ]),
  "",
  "Reference implementation changed; investigate before comparing",
);
const product = JSON.parse(
  await readFile(join(helpers, "package.json"), "utf8"),
);
assert.equal(
  product.source.dirty,
  false,
  "Use a native package from a clean recorded source revision",
);
assert.equal(
  await hash(binary),
  product.files.find((f) => f.path === "kvmhelm.exe").sha256,
);
const scripts = [
  "measure-process-tree.ps1",
  "runtime-measurements.mjs",
  "measure-core-runtime.mjs",
  "native-jetkvm-interop.mjs",
  "native-audio-interop.mjs",
  "measure-native-runtime.mjs",
];
const metadata = {
  schemaVersion: 1,
  platform: {
    name: os.type(),
    release: os.release(),
    version: os.version(),
    architecture: os.arch(),
    logical_cpus: os.cpus().length,
    cpu: os.cpus()[0].model,
    total_memory_bytes: os.totalmem(),
  },
  native: {
    source_revision: product.source.revision,
    sha256: await hash(binary),
    rust_version: product.rustVersion,
  },
  reference: {
    source_revision: command("git", ["rev-parse", "b40b2dd"]),
    compiled_cli_sha256: await hash("dist/server/cli.js"),
    npm_lock_sha256: await hash("package-lock.json"),
  },
  helpers: {
    node: {
      version: command(join(helpers, "node.exe"), ["--version"]),
      sha256: await hash(join(helpers, "node.exe")),
    },
    ffmpeg: {
      version: "6.1.1",
      sha256: await hash(join(helpers, "ffmpeg.exe")),
    },
  },
  harness: Object.fromEntries(
    await Promise.all(
      scripts.map(async (s) => [s, await hash(join("scripts", s))]),
    ),
  ),
  seconds,
  warmup,
  rounds,
  order:
    "AB / BA; a fresh daemon for each core, JetKVM and audio scenario group",
};
await mkdir(output, { recursive: true });
const metadataFile = join(output, "metadata.json");
if (
  await access(metadataFile).then(
    () => true,
    () => false,
  )
)
  assert.deepEqual(
    JSON.parse(await readFile(metadataFile, "utf8")),
    metadata,
    "Existing run has different inputs; use a new output directory",
  );
else
  await writeFile(metadataFile, JSON.stringify(metadata, null, 2) + "\n", {
    flag: "wx",
  });
const runs = [];
for (let round = 1; round <= rounds; round++)
  for (const runtime of round % 2
    ? ["native", "typescript"]
    : ["typescript", "native"]) {
    for (const [group, script] of [
      ["core", "measure-core-runtime.mjs"],
      ["jetkvm", "native-jetkvm-interop.mjs"],
      ["audio", "native-audio-interop.mjs"],
    ]) {
      const path = join(output, `${round}-${runtime}-${group}.json`);
      if (
        !(await access(path).then(
          () => true,
          () => false,
        ))
      ) {
        console.log(`START round=${round} runtime=${runtime} group=${group}`);
        const child = spawn(process.execPath, [join("scripts", script)], {
          windowsHide: true,
          stdio: "inherit",
          env: {
            ...process.env,
            KVMHELM_TEST_BINARY: binary,
            KVMHELM_TEST_HELPER_DIRECTORY: helpers,
            KVMHELM_TEST_DAEMON: runtime,
            KVMHELM_BENCH_NODE: join(helpers, "node.exe"),
            KVMHELM_BENCH_SECONDS: String(seconds),
            KVMHELM_BENCH_WARMUP: String(warmup),
            KVMHELM_BENCH_OUTPUT: path,
          },
        });
        const code = await new Promise((done, reject) => {
          child.once("exit", done);
          child.once("error", reject);
        });
        assert.equal(
          code,
          0,
          `Measurement failed: ${round}/${runtime}/${group}. Preserve partial data; never silently publish failed samples.`,
        );
      } else
        console.log(`REUSE round=${round} runtime=${runtime} group=${group}`);
      const result = JSON.parse(await readFile(path, "utf8"));
      assert.equal(result.runtime, runtime);
      assert.equal(result.scenario_group, group);
      assert.equal(result.seconds, seconds);
      assert.equal(result.warmup, warmup);
      runs.push({ round, ...result });
    }
  }
for (const result of runs.filter((r) => r.scenario_group === "core"))
  assert.deepEqual(
    result.fixture,
    runs.find((r) => r.scenario_group === "core").fixture,
    "Core fixtures must be byte-identical across all runs",
  );
const summary = { schemaVersion: 1, metadata, runs };
await writeFile(
  join(output, "results.json"),
  JSON.stringify(summary, null, 2) + "\n",
);
console.log("Completed paired measurements: " + output);
