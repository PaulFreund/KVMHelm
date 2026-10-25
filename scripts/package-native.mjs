// Build-time tooling only. Packages contain the native daemon, embedded web
// assets, explicit optional helpers and their notices; no reference backend.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { createReadStream, createWriteStream } from "node:fs";
import {
  copyFile,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  readdir,
  rename,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { homedir } from "node:os";
import { basename, dirname, join, relative, resolve, sep } from "node:path";
import { Readable, Transform } from "node:stream";
import { pipeline } from "node:stream/promises";
import { fileURLToPath } from "node:url";
import { createGunzip, createGzip } from "node:zlib";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const args = new Map();
for (let i = 2; i < process.argv.length; i += 2) {
  const key = process.argv[i];
  assert.ok(
    ["--target", "--profile", "--binary", "--out"].includes(key) &&
      process.argv[i + 1],
    "Use --target TRIPLE --profile core|media|full [--binary FILE] [--out DIRECTORY]",
  );
  assert.ok(!args.has(key), "Repeated package option");
  args.set(key, process.argv[i + 1]);
}
function command(program, values, env = process.env) {
  const result = spawnSync(program, values, {
    cwd: root,
    env,
    windowsHide: true,
    encoding: "utf8",
    maxBuffer: 32 * 1024 * 1024,
  });
  assert.equal(
    result.status,
    0,
    `${program} failed: ${result.stderr || result.error?.message}`,
  );
  if (program === "cargo" && values[0] === "build")
    assert.doesNotMatch(
      result.stderr,
      /LNK(?:4098|4217|4286)/,
      "Native C libraries must use the same CRT as the Rust release",
    );
  return result.stdout.trim();
}
const rustVersion = command("rustc", ["-vV"]);
assert.match(
  rustVersion,
  /^release: 1\.99\.1$/m,
  "Use the pinned Rust 1.99.1 toolchain",
);
const target = args.get("--target") ?? rustVersion.match(/^host: (.+)$/m)?.[1];
const lock = JSON.parse(
  await readFile(join(root, "packaging/runtime-lock.json"), "utf8"),
);
const runtime = lock.targets[target];
assert.ok(runtime, "No pinned helper/ABI definition for target " + target);
const profile = args.get("--profile") ?? "full";
assert.ok(
  ["core", "media", "full"].includes(profile),
  "Unknown package profile",
);
const windows = target.includes("windows");
const suffix = windows ? ".exe" : "";
const version = JSON.parse(
  await readFile(join(root, "package.json"), "utf8"),
).version;
const name = `kvmhelm-${version}-${target}-${profile}`;
const out = resolve(args.get("--out") ?? join(root, "target/packages"));
const staging = join(root, "target/package-staging");
const cache = join(root, "target/runtime-cache");
await Promise.all([
  mkdir(staging, { recursive: true }),
  mkdir(cache, { recursive: true }),
  mkdir(out, { recursive: true }),
]);
const temporary = await mkdtemp(join(staging, "assembly-"));
const destination = join(temporary, name);
await mkdir(destination);
const sha = (data) => createHash("sha256").update(data).digest("hex");
async function fileHash(file) {
  const hash = createHash("sha256");
  for await (const bytes of createReadStream(file)) hash.update(bytes);
  return hash.digest("hex");
}
async function put(path, data) {
  await mkdir(dirname(join(destination, path)), { recursive: true });
  await writeFile(join(destination, path), data, { flag: "wx" });
}
async function copy(from, path) {
  await put(path, await readFile(from));
}
async function prepareMsvcCrt() {
  // Older Opus builds used a dynamic CRT and CMake only loads a newly selected
  // toolchain on its first configure. Remove that package's obsolete build
  // cache through Cargo, confined explicitly to this repository's target dir.
  const workspace = await realpath(root);
  const targetDirectory = await realpath(join(root, "target"));
  assert.ok(targetDirectory.startsWith(workspace + sep));
  assert.ok(!(await lstat(join(root, "target"))).isSymbolicLink());
  const builds = join(targetDirectory, target, "release/build");
  if (!(await stat(builds).catch(() => null))) return;
  assert.ok((await realpath(builds)).startsWith(targetDirectory + sep));
  for (const entry of await readdir(builds, { withFileTypes: true })) {
    if (!/^opusic-sys-[0-9a-f]+$/.test(entry.name)) continue;
    assert.ok(entry.isDirectory() && !entry.isSymbolicLink());
    const file = join(builds, entry.name, "out/build/CMakeCache.txt");
    if (!(await stat(file).catch(() => null))) continue;
    assert.ok((await realpath(file)).startsWith(targetDirectory + sep));
    const contents = await readFile(file, "utf8");
    if (/^OPUS_STATIC_RUNTIME:BOOL=ON$/m.test(contents.replaceAll("\r", "")))
      continue;
    console.log(
      "Discarding the repository's obsolete dynamic-CRT Opus release cache",
    );
    command("cargo", [
      "clean",
      "--package",
      "opusic-sys",
      "--release",
      "--target",
      target,
      "--target-dir",
      targetDirectory,
    ]);
    break;
  }
}
async function files(directory, prefix = "") {
  const result = [];
  for (const entry of (await readdir(directory, { withFileTypes: true })).sort(
    (a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0),
  )) {
    assert.ok(!entry.isSymbolicLink(), "Package links are not supported");
    const path = prefix + entry.name;
    assert.ok(
      !/[\r\n\\]/.test(entry.name) && entry.name !== "." && entry.name !== "..",
      "Invalid package filename",
    );
    if (entry.isDirectory())
      result.push(...(await files(join(directory, entry.name), path + "/")));
    else if (entry.isFile()) result.push(path);
    else throw Error("Unsupported package file " + path);
  }
  return result;
}
async function download(asset) {
  assert.match(asset.sha256, /^[0-9a-f]{64}$/);
  assert.ok(asset.url.startsWith("https://"));
  const path = join(cache, asset.sha256);
  const existing = await lstat(path).catch(() => null);
  if (existing) {
    assert.ok(existing.isFile() && !existing.isSymbolicLink());
    assert.equal(await fileHash(path), asset.sha256, "Corrupt helper cache");
    return path;
  }
  const partial = path + ".partial-" + randomUUID();
  console.log("Fetching " + new URL(asset.url).pathname.split("/").at(-1));
  const response = await fetch(asset.url, {
    signal: AbortSignal.timeout(120000),
  });
  assert.equal(response.status, 200, "Cannot fetch pinned helper");
  let length = 0;
  try {
    await pipeline(
      Readable.fromWeb(response.body),
      new Transform({
        transform(chunk, _, done) {
          length += chunk.length;
          done(
            length > 512 * 1024 * 1024
              ? Error("Helper download too large")
              : null,
            chunk,
          );
        },
      }),
      createWriteStream(partial, { flags: "wx" }),
    );
    if (asset.bytes)
      assert.equal(length, asset.bytes, "Pinned helper size changed");
    assert.equal(
      await fileHash(partial),
      asset.sha256,
      "Pinned helper checksum mismatch",
    );
    await rename(partial, path);
  } finally {
    await rm(partial, { force: true });
  }
  return path;
}
function peImports(bytes) {
  const pe = bytes.readUInt32LE(0x3c);
  assert.equal(bytes.toString("ascii", pe, pe + 4), "PE\0\0");
  assert.equal(bytes.readUInt16LE(pe + 4), 0x8664, "Expected Windows x64");
  const optional = pe + 24;
  assert.equal(bytes.readUInt16LE(optional), 0x20b);
  const sections = optional + bytes.readUInt16LE(pe + 20);
  function offset(rva) {
    for (let n = 0; n < bytes.readUInt16LE(pe + 6); n++) {
      const at = sections + 40 * n,
        start = bytes.readUInt32LE(at + 12);
      if (
        rva >= start &&
        rva - start <
          Math.max(bytes.readUInt32LE(at + 8), bytes.readUInt32LE(at + 16))
      )
        return bytes.readUInt32LE(at + 20) + rva - start;
    }
    throw Error("Invalid PE address");
  }
  const imports = [],
    table = offset(bytes.readUInt32LE(optional + 120));
  for (let at = table; at < bytes.length - 20; at += 20) {
    const rva = bytes.readUInt32LE(at + 12);
    if (!rva) break;
    const from = offset(rva),
      end = bytes.indexOf(0, from);
    assert.ok(end >= from && end - from < 256);
    imports.push(bytes.toString("ascii", from, end));
  }
  assert.ok(
    !imports.some((name) => /^(vcruntime|msvcp)\d/i.test(name)),
    "Package must not require a separately installed VC runtime",
  );
  return imports.sort();
}
async function inspect(file) {
  const bytes = await readFile(file);
  if (windows) return { format: "PE-x64", imports: peImports(bytes) };
  if (target.includes("linux")) {
    assert.equal(bytes.toString("hex", 0, 4), "7f454c46");
    assert.equal(bytes[4], 2);
    assert.equal(bytes.readUInt16LE(18), 62);
    return { format: "ELF-x64" };
  }
  assert.equal(
    bytes.readUInt32LE(0),
    0xfeedfacf,
    "Expected a thin 64-bit Mach-O",
  );
  assert.equal(
    bytes.readUInt32LE(4),
    target.startsWith("aarch64") ? 0x100000c : 0x1000007,
  );
  return { format: target.startsWith("aarch64") ? "MachO-arm64" : "MachO-x64" };
}
async function rustNotices() {
  const m = JSON.parse(
    command("cargo", [
      "metadata",
      "--locked",
      "--format-version",
      "1",
      "--filter-platform",
      target,
      "--features",
      "embedded-web",
    ]),
  );
  const nodes = new Map(m.resolve.nodes.map((n) => [n.id, n])),
    selected = new Set();
  function visit(id) {
    if (selected.has(id)) return;
    selected.add(id);
    for (const dep of nodes.get(id)?.deps ?? [])
      if (dep.dep_kinds.some((k) => k.kind !== "dev")) visit(dep.pkg);
  }
  visit(m.resolve.root);
  const supplements = JSON.parse(
    await readFile(join(root, "packaging/license-sources.json"), "utf8"),
  ).packages;
  const inventory = [];
  for (const pkg of m.packages
    .filter((p) => selected.has(p.id) && p.source)
    .sort((a, b) => (a.name + a.version < b.name + b.version ? -1 : 1))) {
    const directory = dirname(pkg.manifest_path),
      prefix = `licenses/cargo/${pkg.name}-${pkg.version}/`,
      copied = [];
    for (const path of await files(directory)) {
      if (
        !/^(licen[sc]e|copying|copyright|notice)([._-]|$)/i.test(basename(path))
      )
        continue;
      assert.ok(
        (await stat(join(directory, path))).size <= 1024 * 1024,
        "Oversize license text",
      );
      await copy(join(directory, path), prefix + path);
      copied.push(prefix + path);
    }
    const supplemental = supplements[pkg.name + "@" + pkg.version];
    if (supplemental)
      for (const source of supplemental.files) {
        const bytes = await readFile(join(root, "packaging", source.file));
        assert.equal(sha(bytes), source.sha256, "Changed supplemental license");
        const path = prefix + "upstream/" + basename(source.file);
        await put(path, bytes);
        copied.push(path);
      }
    assert.ok(
      copied.length,
      "Missing license text: " + pkg.name + "@" + pkg.version,
    );
    // SQLite's public-domain blessing is part of its source header, separate
    // from the Rust wrapper's MIT license. Preserve it beside those notices.
    if (pkg.name === "libsqlite3-sys") {
      const source = await readFile(
        join(directory, "sqlite3/sqlite3.c"),
        "utf8",
      );
      await put(
        prefix + "SQLITE-SOURCE-NOTICE.txt",
        source.slice(0, source.indexOf("*/") + 2) + "\n",
      );
      copied.push(prefix + "SQLITE-SOURCE-NOTICE.txt");
    }
    inventory.push({
      name: pkg.name,
      version: pkg.version,
      license: pkg.license,
      source: `https://crates.io/api/v1/crates/${pkg.name}/${pkg.version}/download`,
      repository: pkg.repository,
      notices: copied,
      ...(supplemental ? { supplemental } : {}),
    });
  }
  return inventory;
}
function tarHeader(path, size, mode, epoch, type = "0") {
  const block = Buffer.alloc(512),
    slash = path.lastIndexOf("/");
  const leaf = Buffer.byteLength(path) <= 100 ? path : path.slice(slash + 1),
    prefix = leaf === path ? "" : path.slice(0, slash);
  if (Buffer.byteLength(leaf) > 100 || Buffer.byteLength(prefix) > 155)
    return null;
  block.write(leaf, 0, 100, "utf8");
  block.write(prefix, 345, 155, "utf8");
  const octal = (at, width, value) => {
    const text = value.toString(8).padStart(width - 1, "0") + "\0";
    assert.equal(text.length, width);
    block.write(text, at, width, "ascii");
  };
  octal(100, 8, mode);
  octal(108, 8, 0);
  octal(116, 8, 0);
  octal(124, 12, size);
  octal(136, 12, epoch);
  block.fill(32, 148, 156);
  block.write(type, 156);
  block.write("ustar\0", 257);
  block.write("00", 263);
  const checksum =
    [...block]
      .reduce((a, b) => a + b, 0)
      .toString(8)
      .padStart(6, "0") + "\0 ";
  block.write(checksum, 148, 8, "ascii");
  return block;
}
async function archive(path, paths, epoch) {
  async function* records() {
    for (const relative of paths) {
      const source = join(destination, relative),
        size = (await stat(source)).size;
      const mode = [
        "kvmhelm" + suffix,
        "ffmpeg" + suffix,
        "node" + suffix,
        "verify.sh",
      ].includes(relative)
        ? 0o755
        : 0o644;
      const full = name + "/" + relative;
      let header = tarHeader(full, size, mode, epoch);
      if (!header) {
        const record = "path=" + full + "\n";
        let length = Buffer.byteLength(record) + 3;
        while (length !== Buffer.byteLength(record) + String(length).length + 1)
          length = Buffer.byteLength(record) + String(length).length + 1;
        const data = Buffer.from(length + " " + record);
        yield tarHeader(
          "PaxHeaders/" + sha(Buffer.from(full)).slice(0, 16),
          data.length,
          0o644,
          epoch,
          "x",
        );
        yield data;
        yield Buffer.alloc((512 - (data.length % 512)) % 512);
        header = tarHeader("payload", size, mode, epoch);
      }
      yield header;
      for await (const chunk of createReadStream(source)) yield chunk;
      yield Buffer.alloc((512 - (size % 512)) % 512);
    }
    yield Buffer.alloc(1024);
  }
  // Normalize gzip's OS marker as well as tar owner/mode/order/time metadata.
  let first = true;
  await pipeline(
    Readable.from(records()),
    createGzip({ level: 9 }),
    new Transform({
      transform(bytes, _, done) {
        if (first) {
          assert.ok(bytes.length >= 10);
          bytes[9] = 255;
          first = false;
        }
        done(null, bytes);
      },
    }),
    createWriteStream(path, { flags: "wx" }),
  );
}
try {
  const binary = args.get("--binary")
    ? resolve(args.get("--binary"))
    : join(root, "target", target, "release", "kvmhelm" + suffix);
  if (!args.has("--binary")) {
    if (windows) await prepareMsvcCrt();
    const flags = [
      `--remap-path-prefix=${root}=/src/kvmhelm`,
      `--remap-path-prefix=${process.env.CARGO_HOME ?? join(homedir(), ".cargo")}=/cargo`,
      ...(windows
        ? [
            "-C",
            "target-feature=+crt-static",
            "-C",
            "link-arg=/Brepro",
            // opusic-sys does not register CMake environment changes with
            // Cargo. Include the policy bytes in its build fingerprint so an
            // older dynamic-CRT CMake cache cannot silently be reused.
            "--cfg",
            `kvmhelm_msvc_static_crt="${await fileHash(join(root, "packaging/msvc-static.cmake"))}"`,
          ]
        : []),
    ];
    command(
      "cargo",
      [
        "build",
        "--locked",
        "--release",
        "--target",
        target,
        "--target-dir",
        join(root, "target"),
        "--features",
        "embedded-web",
      ],
      {
        ...process.env,
        CARGO_BUILD_JOBS: process.env.CARGO_BUILD_JOBS ?? "2",
        CARGO_INCREMENTAL: "0",
        CARGO_ENCODED_RUSTFLAGS: flags.join("\x1f"),
        ...(windows
          ? { CMAKE_TOOLCHAIN_FILE: join(root, "packaging/msvc-static.cmake") }
          : {}),
      },
    );
  }
  const executable = "kvmhelm" + suffix;
  const binaryInfo = await inspect(binary);
  await copy(binary, executable);
  const helpers = [];
  if (profile !== "core") {
    const source = await download(runtime.ffmpeg.binary),
      output = join(destination, "ffmpeg" + suffix);
    let bytes = 0;
    await pipeline(
      createReadStream(source),
      createGunzip(),
      new Transform({
        transform(chunk, _, done) {
          bytes += chunk.length;
          done(
            bytes > 256 * 1024 * 1024 ? Error("Oversize FFmpeg") : null,
            chunk,
          );
        },
      }),
      createWriteStream(output, { flags: "wx" }),
    );
    for (const field of ["readme", "license"])
      await copy(
        await download(runtime.ffmpeg[field]),
        "licenses/ffmpeg/" + field.toUpperCase(),
      );
    helpers.push({
      name: "ffmpeg",
      ...runtime.ffmpeg,
      binaryInfo: await inspect(output),
    });
  }
  if (profile === "full") {
    const source = await download(runtime.node.archive),
      directory = join(temporary, "node");
    await mkdir(directory);
    const nodePath =
        runtime.node.directory + (windows ? "/node.exe" : "/bin/node"),
      licensePath = runtime.node.directory + "/LICENSE";
    command("tar", ["-xf", source, "-C", directory, nodePath, licensePath]);
    const node = join(directory, nodePath);
    assert.ok(
      (await lstat(node)).isFile() &&
        (await lstat(join(directory, licensePath))).isFile(),
    );
    await copy(node, "node" + suffix);
    await copy(join(directory, licensePath), "licenses/node/LICENSE");
    helpers.push({
      name: "node",
      ...runtime.node,
      binaryInfo: await inspect(node),
    });
  }
  const cargo = await rustNotices();
  const web = JSON.parse(
    await readFile(join(root, "dist/web/third-party-notices.json"), "utf8"),
  );
  assert.ok(web.packages.length, "Build web dependency notices first");
  await put(
    "licenses/web/third-party-notices.json",
    JSON.stringify(web, null, 2) + "\n",
  );
  for (const file of [
    "verify.ps1",
    "verify.sh",
    "kvmhelm.service",
    "local.kvmhelm.plist",
    "README.txt",
  ])
    await copy(join(root, "packaging", file), file);
  await put(
    "THIRD-PARTY.json",
    JSON.stringify({ schemaVersion: 1, cargo, web, helpers }, null, 2) + "\n",
  );
  const suppliedRevision = process.env.KVMHELM_BUILD_REVISION;
  if (suppliedRevision)
    assert.match(suppliedRevision, /^(?:[0-9a-f]{40}|unversioned)$/);
  const revision = suppliedRevision ?? command("git", ["rev-parse", "HEAD"]),
    dirty = suppliedRevision
      ? true
      : command("git", ["status", "--porcelain"]) !== "";
  const epoch = Number(
    process.env.SOURCE_DATE_EPOCH ??
      (suppliedRevision ? "0" : command("git", ["log", "-1", "--format=%ct"])),
  );
  assert.ok(
    Number.isSafeInteger(epoch) && epoch >= 0 && epoch < 8 ** 11,
    "Invalid source date epoch",
  );
  const contents = [];
  for (const file of await files(destination))
    contents.push({
      path: file,
      sha256: await fileHash(join(destination, file)),
      bytes: (await stat(join(destination, file))).size,
    });
  await put(
    "package.json",
    JSON.stringify(
      {
        schemaVersion: 1,
        name: "kvmhelm",
        version,
        target,
        profile,
        source: { revision, dirty, epoch },
        rustVersion,
        binaryInfo,
        build: args.has("--binary") ? "supplied-binary" : "cargo-release",
        inputs: {
          cargoLock: await fileHash(join(root, "Cargo.lock")),
          npmLock: await fileHash(join(root, "package-lock.json")),
          helperLock: await fileHash(join(root, "packaging/runtime-lock.json")),
          packager: await fileHash(fileURLToPath(import.meta.url)),
          msvcToolchain: windows
            ? await fileHash(join(root, "packaging/msvc-static.cmake"))
            : null,
          webNotices: await fileHash(
            join(root, "dist/web/third-party-notices.json"),
          ),
        },
        helpers: helpers.map((h) => ({ name: h.name, version: h.version })),
        files: contents,
      },
      null,
      2,
    ) + "\n",
  );
  const paths = await files(destination),
    sums = [];
  for (const path of paths)
    sums.push((await fileHash(join(destination, path))) + "  " + path);
  await put("SHA256SUMS", sums.join("\n") + "\n");
  paths.push("SHA256SUMS");
  paths.sort();
  const artifact = join(out, name + ".tar.gz");
  await archive(artifact, paths, epoch);
  await writeFile(
    artifact + ".sha256",
    (await fileHash(artifact)) + "  " + basename(artifact) + "\n",
    { flag: "wx" },
  );
  console.log(
    JSON.stringify({
      artifact: relative(root, artifact),
      files: paths.length,
      cargoNotices: cargo.length,
      webNotices: web.packages.length,
      profile,
      binaryInfo,
      sourceDirty: dirty,
    }),
  );
} finally {
  assert.ok(
    resolve(temporary).startsWith(resolve(staging) + sep) &&
      basename(temporary).startsWith("assembly-"),
  );
  await rm(temporary, {
    recursive: true,
    force: true,
    maxRetries: 40,
    retryDelay: 100,
  });
}
