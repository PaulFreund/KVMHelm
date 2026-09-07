#!/usr/bin/env node
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { resolve, join } from "node:path";
import { homedir } from "node:os";
import { parseArgs } from "node:util";
import { randomUUID } from "node:crypto";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import {
  ListToolsRequestSchema,
  CallToolRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import { Store, Secrets } from "./store.js";
import { Auth } from "./auth.js";
import { Core } from "./core.js";
import { PluginHost } from "./plugins.js";
import { serve } from "./server.js";
import { scopes } from "../shared/contracts.js";
import { McpUpstream } from "./mcp-upstream.js";
const { values: v, positionals: a } = parseArgs({
  allowPositionals: true,
  options: {
    data: { type: "string" },
    secrets: { type: "string" },
    host: { type: "string" },
    "lan-host": { type: "string" },
    "lan-port": { type: "string" },
    port: { type: "string" },
    cert: { type: "string" },
    key: { type: "string" },
    origin: { type: "string", multiple: true },
    dev: { type: "boolean" },
    url: { type: "string" },
    "pat-file": { type: "string" },
    name: { type: "string" },
    scope: { type: "string", multiple: true },
    device: { type: "string", multiple: true },
    file: { type: "string" },
    transport: { type: "string" },
    out: { type: "string" },
  },
});
const dir = resolve(
  v.data ?? process.env.KVMHELM_DATA ?? join(homedir(), ".kvmhelm", "data"),
);
const secretDir = resolve(
  v.secrets ??
    process.env.KVMHELM_SECRETS ??
    join(homedir(), ".kvmhelm", "secrets"),
);
const patFile = resolve(v["pat-file"] ?? join(secretDir, "cli.pat"));
const url = v.url ?? process.env.KVMHELM_URL ?? "http://127.0.0.1:8765";
async function pat() {
  return (process.env.KVMHELM_PAT ?? (await readFile(patFile, "utf8"))).trim();
}
async function api(path: string, method = "GET", body?: unknown) {
  const r = await fetch(url + "/api/v1" + path, {
    method,
    headers: {
      Authorization: "Bearer " + (await pat()),
      "Content-Type": "application/json",
      "X-KVMHelm-Client": "cli",
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const b = await r.json();
  if (!r.ok) throw Error(JSON.stringify(b));
  return b;
}
async function main() {
  const command = a[0] ?? "help";
  if (command === "mcp") {
    if (v.transport && v.transport !== "stdio")
      throw Error("Use --transport stdio");
    const upstream = new McpUpstream(new URL("/mcp", url), pat);
    const { client } = await upstream.connect();
    const server = new Server(
      { name: "KVMHelm", version: "0.1.0" },
      { capabilities: { tools: {} }, instructions: client.getInstructions() },
    );
    server.setRequestHandler(ListToolsRequestSchema, () =>
      upstream.listTools(),
    );
    server.setRequestHandler(
      CallToolRequestSchema,
      async (req) => (await upstream.callTool(req.params)) as any,
    );
    const stdio = new StdioServerTransport();
    let ending = false;
    const close = async () => {
      if (ending) return;
      ending = true;
      await upstream.close();
      await server.close();
    };
    process.stdin.on("end", () => void close());
    process.on("SIGINT", () => void close());
    await server.connect(stdio);
    return;
  }
  if (command === "doctor") {
    console.log(
      JSON.stringify(
        {
          node: process.version,
          platform: process.platform,
          arch: process.arch,
          url,
          health: await fetch(url + "/healthz").then((r) => r.json()),
          diagnostics: await api("/diagnostics"),
        },
        null,
        2,
      ),
    );
    return;
  }
  if (command === "token" && a[1] !== "bootstrap") {
    if (a[1] === "list")
      console.log(JSON.stringify(await api("/tokens"), null, 2));
    else if (a[1] === "revoke")
      console.log(await api("/tokens/" + a[2], "DELETE"));
    else if (a[1] === "create") {
      const result: any = await api("/tokens", "POST", {
        name: v.name ?? "client",
        scopes: v.scope ?? ["devices:read", "video:read", "input:write"],
        devices: v.device ?? ["*"],
      });
      if (v.out) {
        await writeFile(resolve(v.out), result.token + "\n", {
          mode: 0o600,
          flag: "wx",
        });
        delete result.token;
        console.log(JSON.stringify(result));
      } else console.log(JSON.stringify(result, null, 2));
    } else throw Error("token create|list|revoke|bootstrap");
    return;
  }
  if (command === "device") {
    if (a[1] === "list")
      console.log(JSON.stringify(await api("/devices"), null, 2));
    else if (a[1] === "add") {
      if (!v.file) throw Error("--file configuration.json required");
      console.log(
        await api(
          "/devices",
          "POST",
          JSON.parse(await readFile(v.file, "utf8")),
        ),
      );
    } else if (
      a[1] === "stop" ||
      a[1] === "resume" ||
      a[1] === "test" ||
      a[1] === "benchmark"
    )
      console.log(await api(`/devices/${a[2]}/${a[1]}`, "POST", {}));
    else throw Error("device list|add|test|benchmark|stop|resume");
    return;
  }
  if (command === "plugin") {
    if (a[1] === "list") console.log(await api("/plugins"));
    else if (a[1] === "install")
      console.log(
        await api("/plugins", "POST", {
          path: resolve(a[2]),
          devices: v.device ?? [],
        }),
      );
    else console.log(await api(`/plugins/${a[2]}/${a[1]}`, "POST", {}));
    return;
  }
  if (!["serve", "demo", "token", "secrets"].includes(command)) {
    console.log(
      "KVMHelm\n  serve [--host 127.0.0.1 --port 8765 --cert cert.pem --key key.pem]\n  token bootstrap|create|list|revoke\n  demo\n  mcp --transport stdio [--pat-file protected-file]\n  device list|add --file config.json|test|benchmark|stop|resume\n  plugin list|install <directory>|enable|disable|uninstall\n  doctor\nUse --data and --secrets for separate protected storage locations.",
    );
    return;
  }
  await mkdir(dir, { recursive: true, mode: 0o700 });
  const store = new Store(dir);
  const secrets = await Secrets.open(secretDir),
    auth = new Auth(store);
  await auth.load();
  if (command === "secrets") {
    try {
      if (a[1] !== "cleanup")
        throw Error("Use secrets cleanup with the daemon stopped");
      const references = new Set<string>();
      for (const device of await store.list("devices"))
        for (const ref of [device.secret_ref, device.tls.ca_ref])
          if (ref) references.add(ref);
      console.log(JSON.stringify({ deleted: await secrets.prune(references) }));
    } finally {
      await store.close();
    }
    return;
  }
  if (command === "token") {
    if (a[1] !== "bootstrap") throw Error("Unknown command");
    const token = await auth.create({
      name: v.name ?? "local owner",
      scopes: [...scopes],
      devices: ["*"],
    });
    await writeFile(patFile, token.token + "\n", { mode: 0o600, flag: "wx" });
    console.log(
      `Bootstrap PAT written once to ${patFile}. Protect this file; existing daemons must restart to load locally bootstrapped tokens.`,
    );
    await store.close();
    return;
  }
  const core = new Core(store, secrets, auth);
  await core.load();
  if (command === "demo") {
    if (!auth.tokens.size) throw Error("Run token bootstrap first");
    const identity = auth.authenticate(await pat(), "cli");
    for (let n = 1; n <= 4; n++)
      await core.saveDevice(identity, {
        name: `Lab ${n.toString().padStart(2, "0")} · Simulator`,
        driver_id: "simulator",
        model: "Synthetic KVM",
        transport: "simulator",
        tags: ["demo"],
      });
    await core.shutdown();
    await store.close();
    console.log("Added four explicitly simulated targets.");
    return;
  }
  const plugins = new PluginHost(core, join(dir, "plugins"));
  await plugins.load();
  const instance = await serve(core, plugins, {
    host: v.host ?? "127.0.0.1",
    port: Number(v.port ?? 8765),
    cert: v["lan-host"] ? undefined : v.cert,
    key: v["lan-host"] ? undefined : v.key,
    origins: v.origin ?? [],
    dev: v.dev,
  });
  const lanInstance = v["lan-host"]
    ? await serve(core, plugins, {
        host: v["lan-host"],
        port: Number(v["lan-port"] ?? 8766),
        cert: v.cert,
        key: v.key,
        origins: v.origin ?? [],
      })
    : undefined;
  console.error(
    `KVMHelm listening on ${v.cert && !v["lan-host"] ? "https" : "http"}://${v.host ?? "127.0.0.1"}:${v.port ?? 8765}`,
  );
  if (lanInstance)
    console.error(
      `KVMHelm LAN listening on https://${v["lan-host"]}:${v["lan-port"] ?? 8766}`,
    );
  let stopping = false;
  const stop = async () => {
    if (stopping) return;
    stopping = true;
    await instance.close();
    await lanInstance?.close();
    await plugins.shutdown();
    await core.shutdown();
    await store.close();
  };
  process.on("SIGINT", () => void stop());
  process.on("SIGTERM", () => void stop());
}
main().catch((e) => {
  console.error(e instanceof Error ? e.message : "KVMHelm failed");
  process.exitCode = 1;
});
