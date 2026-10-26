// Materialize the original Git commit only in ignored test storage, with its
// own locked npm installation. No tracked second backend or production fallback.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync } from "node:fs";
import {
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rename,
  rm,
  writeFile,
} from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  referenceRevision,
  referenceRoot,
  referencePath,
} from "./reference.mjs";

const root = fileURLToPath(new URL("../", import.meta.url));
function run(command, args, cwd = root, capture = false) {
  const result = spawnSync(command, args, {
    cwd,
    windowsHide: true,
    encoding: "utf8",
    maxBuffer: 16 * 1024 * 1024,
    stdio: capture ? "pipe" : "inherit",
  });
  assert.equal(
    result.status,
    0,
    result.stderr || result.error?.message || command,
  );
  return result.stdout;
}
const npm = process.env.npm_execpath;
assert.ok(npm, "Run via npm run prepare:reference or npm run test:reference");
if (existsSync(referenceRoot)) {
  referencePath("dist/server/cli.js");
  console.log(`Verified existing test reference ${referenceRevision}`);
} else {
  assert.equal(
    run(
      "git",
      ["rev-parse", `${referenceRevision}^{commit}`],
      root,
      true,
    ).trim(),
    referenceRevision,
    "Fetch the pinned commit before preparing the test reference (CI uses full history)",
  );
  const entries = run(
    "git",
    ["ls-tree", "-rz", "--full-tree", referenceRevision],
    root,
    true,
  )
    .split("\0")
    .filter(Boolean)
    .map((line) => {
      const match = line.match(/^(100644|100755) blob ([a-f0-9]{40})\t(.+)$/);
      assert.ok(match, "Reference must contain only regular Git files");
      return { path: match[3], algorithm: "sha1", hash: match[2] };
    });
  await mkdir(dirname(referenceRoot), { recursive: true });
  const temporary = await mkdtemp(join(dirname(referenceRoot), "prepare-"));
  const archive = join(temporary, "original.tar");
  run("git", [
    "-c",
    "core.autocrlf=false",
    "-c",
    "core.eol=lf",
    "archive",
    "--format=tar",
    `--output=${archive}`,
    referenceRevision,
  ]);
  run("tar", ["-xf", archive, "-C", temporary]);
  await rm(archive);
  for (const entry of entries) {
    const bytes = await readFile(join(temporary, entry.path));
    assert.equal(
      createHash("sha1")
        .update(`blob ${bytes.length}\0`)
        .update(bytes)
        .digest("hex"),
      entry.hash,
      entry.path,
    );
  }
  // Build with the original lockfile and scripts, never the port's npm graph.
  run(process.execPath, [npm, "ci", "--include=dev"], temporary);
  run(process.execPath, [npm, "run", "build"], temporary);
  async function outputs(directory, prefix = "dist") {
    for (const entry of await readdir(directory, { withFileTypes: true })) {
      const path = `${prefix}/${entry.name}`,
        file = join(directory, entry.name);
      if (entry.isDirectory()) await outputs(file, path);
      else {
        assert.ok(entry.isFile(), "No symlinks in compiled reference");
        entries.push({
          path,
          algorithm: "sha256",
          hash: createHash("sha256")
            .update(await readFile(file))
            .digest("hex"),
        });
      }
    }
  }
  await outputs(join(temporary, "dist"));
  await writeFile(
    join(temporary, "reference.json"),
    JSON.stringify({ revision: referenceRevision, files: entries }, null, 2) +
      "\n",
  );
  await rename(temporary, referenceRoot);
  referencePath("dist/server/cli.js");
  console.log(`Prepared isolated test reference ${referenceRevision}`);
}
if (process.argv.includes("--test")) {
  const tests = (await readdir(join(referenceRoot, "tests")))
    .filter((f) => f.endsWith(".test.ts"))
    .sort();
  run(
    process.execPath,
    [
      "--import",
      "tsx",
      "--test",
      "--test-concurrency=1",
      "--test-force-exit",
      "--test-timeout=90000",
      ...tests.map((f) => `tests/${f}`),
    ],
    referenceRoot,
  );
}
