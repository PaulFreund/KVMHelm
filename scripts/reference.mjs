// Test-only access to the actual, pinned pre-port implementation. Preparation
// is explicit; importing a fixture never installs or starts another backend.
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { resolve, sep } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

export const referenceRevision = "b40b2dde246533a25a18148a6b59faced676d9ba";
export const referenceRoot = resolve(
  fileURLToPath(
    new URL(`../target/reference/${referenceRevision}/`, import.meta.url),
  ),
);
let verified = false;
function verify() {
  if (verified) return;
  try {
    const manifest = JSON.parse(
      readFileSync(resolve(referenceRoot, "reference.json"), "utf8"),
    );
    if (manifest.revision !== referenceRevision || !manifest.files?.length)
      throw Error("Wrong reference revision");
    for (const { path, algorithm, hash } of manifest.files) {
      const file = resolve(referenceRoot, path);
      if (!file.startsWith(referenceRoot + sep)) throw Error("Invalid path");
      const bytes = readFileSync(file);
      const digest = createHash(algorithm);
      if (algorithm === "sha1") digest.update(`blob ${bytes.length}\0`);
      if (digest.update(bytes).digest("hex") !== hash)
        throw Error(`Modified reference file: ${path}`);
    }
    verified = true;
  } catch (cause) {
    throw new Error(
      "Pinned test reference unavailable or modified. Run npm run prepare:reference in a checkout containing " +
        referenceRevision +
        ".",
      { cause },
    );
  }
}
export function referencePath(path) {
  verify();
  const file = resolve(referenceRoot, path);
  if (!file.startsWith(referenceRoot + sep))
    throw Error("Invalid reference path");
  return file;
}
export function referenceImport(path) {
  return import(pathToFileURL(referencePath(path)).href);
}
export function referenceDependency(name) {
  return createRequire(referencePath("package.json"))(name);
}
