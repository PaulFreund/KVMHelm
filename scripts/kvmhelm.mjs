#!/usr/bin/env node
// npm is a source-checkout convenience. Deployed packages invoke kvmhelm itself.
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";

const binary = fileURLToPath(
  new URL(
    `../target/release/kvmhelm${process.platform === "win32" ? ".exe" : ""}`,
    import.meta.url,
  ),
);
if (!existsSync(binary)) {
  console.error(
    "Native executable missing. Run npm run build and npm run build:native first, or use an extracted native package.",
  );
  process.exitCode = 1;
} else {
  const child = spawn(binary, process.argv.slice(2), {
    stdio: "inherit",
  });
  // Windows Ctrl+C reaches the inherited console of both processes. Sending
  // child.kill there would terminate the daemon before its shutdown handler.
  const interrupt = () => {
    if (process.platform !== "win32" && !child.killed) child.kill("SIGINT");
  };
  const terminate = () => {
    if (!child.killed) child.kill("SIGTERM");
  };
  process.on("SIGINT", interrupt);
  process.on("SIGTERM", terminate);
  child.once("error", (error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
  child.once("exit", (code, signal) => {
    process.removeListener("SIGINT", interrupt);
    process.removeListener("SIGTERM", terminate);
    process.exitCode = code ?? (signal === "SIGINT" ? 130 : 143);
  });
}
