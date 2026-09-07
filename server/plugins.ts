import { fork, type ChildProcess } from "node:child_process";
import { readFile, mkdir } from "node:fs/promises";
import { resolve, join } from "node:path";
import { z } from "zod";
import { Core } from "./core.js";
import { GatewayError } from "../shared/contracts.js";
export const manifestSchema = z
  .object({
    id: z.string().regex(/^[a-zA-Z0-9._-]+$/),
    version: z.string().min(1),
    apiVersion: z.literal("kvmhelm.plugin/v1"),
    kind: z.enum(["feature", "driver", "media"]),
    runtime: z.literal("external-process"),
    entry: z.string(),
    permissions: z.array(
      z.enum([
        "audio:read:from_target",
        "video:read",
        "ui:panel",
        "notifications:publish",
      ]),
    ),
    outboundHosts: z.array(z.string()).default([]),
    secretRefs: z.array(z.string()).default([]),
    uiSlots: z
      .array(z.enum(["kvm.sidepanel", "settings.plugins", "overview.badge"]))
      .default([]),
  })
  .strict();
interface Loaded {
  id: string;
  path: string;
  manifest: z.infer<typeof manifestSchema>;
  devices: string[];
  enabled: boolean;
  revision: number;
  status: string;
  process?: ChildProcess;
  subscriptions: Map<string, AbortController>;
  panels: unknown[];
  lastHealth: number;
}
export class PluginHost {
  private eventBudget = new Map<string, { at: number; count: number }>();
  entries = new Map<string, Loaded>();
  private health: NodeJS.Timeout;
  constructor(
    private core: Core,
    private data: string,
  ) {
    this.health = setInterval(() => {
      for (const p of this.entries.values()) {
        if (p.process) {
          if (Date.now() - p.lastHealth > 30000) {
            void this.stop(p);
            p.status = "error";
          } else p.process.send({ type: "health" });
        }
      }
    }, 10000);
  }
  async load() {
    for (const p of await this.core.store.list("plugins")) {
      const entry = {
        ...p,
        status: "disabled",
        subscriptions: new Map(),
        panels: [],
        lastHealth: 0,
      };
      this.entries.set(p.id, entry);
      if (p.enabled)
        try {
          await this.start(entry);
        } catch {
          entry.status = "error";
        }
    }
  }
  list() {
    return [...this.entries.values()].map((p) => ({
      id: p.id,
      version: p.manifest.version,
      manifest: p.manifest,
      devices: p.devices,
      enabled: p.enabled,
      status: p.status,
      panels: p.panels,
      revision: p.revision,
    }));
  }
  async install(input: { path: string; devices: string[] }) {
    const path = resolve(input.path),
      manifest = manifestSchema.parse(
        JSON.parse(await readFile(join(path, "kvmhelm.plugin.json"), "utf8")),
      );
    const old = this.entries.get(manifest.id);
    if (old) await this.stop(old);
    const saved = await this.core.store.put(
      "plugins",
      manifest.id,
      {
        id: manifest.id,
        path,
        manifest,
        devices: input.devices,
        enabled: true,
      },
      old?.revision ?? 0,
    );
    const p: Loaded = {
      ...saved,
      status: "starting",
      subscriptions: new Map(),
      panels: [],
      lastHealth: Date.now(),
    };
    this.entries.set(p.id, p);
    try {
      await this.start(p);
    } catch {
      p.status = "error";
    }
    return this.list().find((v) => v.id === p.id);
  }
  private async start(p: Loaded) {
    const own = join(this.data, p.id);
    await mkdir(own, { recursive: true, mode: 0o700 });
    const child = fork(resolve(p.path, p.manifest.entry), [], {
      cwd: p.path,
      serialization: "advanced",
      stdio: ["ignore", "ignore", "ignore", "ipc"],
      env: {
        PATH: process.env.PATH,
        SystemRoot: process.env.SystemRoot,
        TEMP: process.env.TEMP,
        HOME: process.env.HOME,
      },
      execArgv: [],
    });
    p.process = child;
    p.lastHealth = Date.now();
    p.status = "starting";
    child.on("error", () => {
      p.status = "error";
    });
    child.on("exit", () => {
      p.process = undefined;
      for (const c of p.subscriptions.values()) c.abort();
      p.subscriptions.clear();
      if (p.status !== "disabled") p.status = "error";
      this.core.event("plugin.status", undefined, {
        plugin_id: p.id,
        status: p.status,
      });
    });
    child.on(
      "message",
      (m) =>
        void this.message(p, m).catch(() => {
          child.send({ type: "error", code: "PLUGIN_REQUEST_REJECTED" });
        }),
    );
    child.send({
      type: "initialize",
      apiVersion: "kvmhelm.plugin/v1",
      plugin_id: p.id,
      permissions: p.manifest.permissions,
      devices: p.devices,
      data_path: own,
      secret_refs: p.manifest.secretRefs,
      budgets: {
        max_buffer_bytes: 1024 * 1024,
        max_chunk_bytes: 262144,
        max_events_per_second: 30,
      },
    });
    child.send({ type: "start" });
  }
  private async message(p: Loaded, m: any) {
    if (!m || typeof m !== "object") return;
    if (m.type === "health" || m.type === "ready") {
      p.lastHealth = Date.now();
      p.status = "running";
      return;
    }
    if (!p.enabled) throw new GatewayError("FORBIDDEN");
    const now = Date.now();
    let budget = this.eventBudget.get(p.id);
    if (!budget || now - budget.at > 1000) {
      budget = { at: now, count: 0 };
      this.eventBudget.set(p.id, budget);
    }
    if (++budget.count > 30)
      throw new GatewayError("RESOURCE_LIMIT", "Plugin event rate exceeded");
    const allowed = (scope: string, id?: string) => {
      if (
        !p.manifest.permissions.includes(scope as any) ||
        (id && !p.devices.includes(id))
      )
        throw new GatewayError("FORBIDDEN");
    };
    if (m.type === "devices") {
      p.process?.send({
        type: "devices",
        devices: p.devices.map((id) => {
          const d = this.core.get(id);
          return { device_id: id, name: d.device.name, capabilities: d.caps };
        }),
      });
      return;
    }
    if (m.type === "panel") {
      const panel = z
        .object({
          id: z.string().max(100),
          title: z.string().max(100),
          text: z.string().max(16000),
          device_id: z.string().optional(),
          slot: z.enum(["kvm.sidepanel", "settings.plugins", "overview.badge"]),
        })
        .strict()
        .parse(m.panel);
      allowed("ui:panel", panel.device_id);
      if (!p.manifest.uiSlots.includes(panel.slot))
        throw new GatewayError("FORBIDDEN");
      p.panels = [
        ...p.panels.filter((x: any) => x.id !== panel.id),
        panel,
      ].slice(-32);
      this.core.event("plugin.panel", panel.device_id, {
        plugin_id: p.id,
        panel,
      });
      return;
    }
    if (m.type === "notification") {
      allowed("notifications:publish", m.device_id);
      if (!m.device_id) throw new GatewayError("FORBIDDEN");
      const text = z.string().max(2000).parse(m.text);
      this.core.event("plugin.notification", m.device_id, {
        plugin_id: p.id,
        text,
      });
      return;
    }
    if (m.type === "unsubscribe") {
      p.subscriptions.get(m.id)?.abort();
      p.subscriptions.delete(m.id);
      return;
    }
    if (m.type === "subscribe") {
      if (!["audio", "video"].includes(m.source))
        throw new GatewayError("INVALID_ARGUMENT");
      const id = z.string().max(100).parse(m.id),
        device_id = z.string().parse(m.device_id);
      allowed(
        m.source === "audio" ? "audio:read:from_target" : "video:read",
        device_id,
      );
      if (p.subscriptions.size >= 64) throw new GatewayError("RESOURCE_LIMIT");
      const d = this.core.get(device_id);
      await this.core.connect(d);
      const controller = new AbortController();
      p.subscriptions.set(id, controller);
      const consumer = `plugin:${p.id}:${id}`;
      d.media.retain(consumer, "warm");
      const send = (chunk: any) => {
        if (!p.enabled || controller.signal.aborted) return false;
        return (
          p.process?.send({ type: "media", id, device_id, chunk }) ?? false
        );
      };
      void (async () => {
        try {
          if (m.source === "audio") {
            if (!d.driver.subscribeAudio)
              throw new GatewayError(
                "UNSUPPORTED_ACTION",
                "Audio source unavailable",
              );
            for await (const chunk of d.driver.subscribeAudio(
              controller.signal,
            )) {
              if (!send(chunk)) {
                controller.abort();
                throw new GatewayError(
                  "RESOURCE_LIMIT",
                  "Plugin receiver is too slow",
                );
              }
            }
          } else {
            while (!controller.signal.aborted) {
              const frame = await d.media.screenshot(250, 3000);
              if (!send({ data: frame.data, ...frame.info })) {
                controller.abort();
                throw new GatewayError("RESOURCE_LIMIT");
              }
              await new Promise((r) => setTimeout(r, 250));
            }
          }
        } catch (e) {
          p.process?.send({
            type: "gap",
            id,
            code: e instanceof GatewayError ? e.code : "SOURCE_UNAVAILABLE",
          });
        } finally {
          d.media.release(consumer);
          p.subscriptions.delete(id);
        }
      })();
      return;
    }
  }
  private async stop(p: Loaded) {
    p.status = "disabled";
    for (const c of p.subscriptions.values()) c.abort();
    p.subscriptions.clear();
    const child = p.process;
    if (child) {
      child.send({ type: "stop" });
      child.send({ type: "dispose" });
      await new Promise<void>((r) => {
        const timer = setTimeout(() => {
          child.kill();
          r();
        }, 2000);
        child.once("exit", () => {
          clearTimeout(timer);
          r();
        });
      });
      p.process = undefined;
    }
    p.panels = [];
  }
  async operation(id: string, op: string) {
    const p = this.entries.get(id);
    if (!p) throw new GatewayError("NOT_FOUND");
    if (!["enable", "disable", "uninstall", "restart"].includes(op))
      throw new GatewayError("INVALID_ARGUMENT");
    await this.stop(p);
    if (op === "uninstall") {
      await this.core.store.delete("plugins", id, p.revision);
      this.entries.delete(id);
      return { ok: true };
    }
    p.enabled = op !== "disable";
    const saved = await this.core.store.put(
      "plugins",
      id,
      {
        id: p.id,
        path: p.path,
        manifest: p.manifest,
        devices: p.devices,
        enabled: p.enabled,
      },
      p.revision,
    );
    p.revision = saved.revision;
    if (p.enabled) await this.start(p);
    return { ok: true };
  }
  async shutdown() {
    clearInterval(this.health);
    for (const p of this.entries.values()) await this.stop(p);
  }
}
