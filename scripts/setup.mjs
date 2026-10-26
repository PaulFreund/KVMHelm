import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { resolve, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../", import.meta.url));
const [major, minor] = process.versions.node.split(".").map(Number);
function run(command, args, options = {}) {
  return new Promise((accept, reject) => {
    const child = spawn(command, args, {
      cwd: root,
      stdio: "inherit",
      windowsHide: true,
      shell: false,
      ...options,
    });
    child.once("error", reject);
    child.once("exit", (code) =>
      code === 0
        ? accept()
        : reject(new Error(`${command} exited with ${code}`)),
    );
  });
}
try {
  if (major !== 24 || minor < 18)
    throw new Error(
      "Install Node.js 24.18 or a newer Node.js 24 release first.",
    );
  const npm = process.env.npm_execpath;
  if (!npm) throw new Error("Run this installer with npm run setup.");
  await run(process.execPath, [npm, "ci", "--include=dev"]);
  await run(process.execPath, [npm, "run", "build"]);
  await run(process.execPath, [npm, "run", "build:native"]);
  const secrets = resolve(
    process.env.KVMHELM_SECRETS || join(homedir(), ".kvmhelm", "secrets"),
  );
  const pat = join(secrets, "cli.pat");
  if (!existsSync(pat))
    await run(process.execPath, ["scripts/kvmhelm.mjs", "token", "bootstrap"]);
  console.log(
    `\nKVMHelm native is ready. Start with: npm start\nOpen: http://127.0.0.1:8765\nLogin token file: ${pat}\nJetKVM video needs FFmpeg; plugins need Node. Use a native media/full package or configure KVMHELM_FFMPEG/KVMHELM_NODE.\nExisting settings and tokens are preserved.`,
  );
} catch (error) {
  console.error(`Setup failed: ${error.message}`);
  process.exitCode = 1;
}
