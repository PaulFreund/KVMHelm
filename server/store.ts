import { Worker } from "node:worker_threads";
import {
  mkdir,
  readFile,
  writeFile,
  chmod,
  unlink,
  readdir,
  stat,
} from "node:fs/promises";
import { join, resolve } from "node:path";
import { randomBytes, createCipheriv, createDecipheriv } from "node:crypto";
import { GatewayError } from "../shared/contracts.js";
import { existsSync } from "node:fs";
import type { Device, Preferences } from "../shared/contracts.js";
import type { Token } from "./auth.js";
import type { PluginRecord } from "./plugins.js";
interface Documents {
  devices: Device;
  tokens: Token;
  preferences: Preferences;
  plugins: PluginRecord;
}
export class Store {
  state: "starting" | "ready" | "failed" | "closed" = "starting";
  private startup: NodeJS.Timeout;
  private worker: Worker;
  private pending = new Map<
    number,
    {
      resolve: (v: unknown) => void;
      reject: (e: Error) => void;
      timer: NodeJS.Timeout;
    }
  >();
  private seq = 0;
  constructor(public dir: string) {
    const compiled = new URL("./store-worker.js", import.meta.url);
    this.worker = new Worker(
      existsSync(compiled)
        ? compiled
        : new URL("./store-worker.ts", import.meta.url),
      {
        workerData: { path: join(dir, "config.sqlite") },
      },
    );
    this.worker.on("message", (m) => {
      if (m.ready) {
        if (this.state === "starting") this.state = "ready";
        clearTimeout(this.startup);
        return;
      }
      const p = this.pending.get(m.id);
      this.pending.delete(m.id);
      if (p) clearTimeout(p.timer);
      m.error ? p?.reject(new GatewayError(m.error)) : p?.resolve(m.result);
    });
    this.worker.on("error", () => this.fail());
    this.worker.on("exit", () => {
      if (this.state !== "closed") this.fail();
    });
    this.startup = setTimeout(() => this.fail(), 10000);
  }
  private fail() {
    if (this.state !== "closed") this.state = "failed";
    clearTimeout(this.startup);
    for (const p of this.pending.values()) {
      clearTimeout(p.timer);
      p.reject(
        new GatewayError("STORAGE_ERROR", "Configuration store unavailable"),
      );
    }
    this.pending.clear();
    void this.worker.terminate();
  }
  async health() {
    if (this.state !== "ready") throw new GatewayError("STORAGE_ERROR");
    await this.call("ping", "");
    return { state: this.state, pending: this.pending.size };
  }
  call<T = unknown>(
    op: string,
    kind: string,
    key?: string,
    value?: unknown,
    revision = 0,
  ): Promise<T> {
    if (this.state === "failed" || this.state === "closed")
      return Promise.reject(new GatewayError("STORAGE_ERROR"));
    if (this.pending.size >= 256)
      return Promise.reject(new GatewayError("RESOURCE_LIMIT"));
    return new Promise((resolve, reject) => {
      const id = ++this.seq;
      const timer = setTimeout(() => this.fail(), 10000);
      this.pending.set(id, {
        resolve: (value) => resolve(value as T),
        reject,
        timer,
      });
      try {
        this.worker.postMessage({ id, op, kind, key, value, revision });
      } catch {
        this.fail();
      }
    });
  }
  list<K extends keyof Documents>(kind: K) {
    return this.call<Documents[K][]>("list", kind);
  }
  get<K extends keyof Documents>(kind: K, key: string) {
    return this.call<Documents[K] | null>("get", kind, key);
  }
  put<T extends object>(
    kind: keyof Documents,
    key: string,
    value: T,
    revision = 0,
  ) {
    return this.call<T & { revision: number }>(
      "put",
      kind,
      key,
      value,
      revision,
    );
  }
  delete(kind: string, key: string, revision: number) {
    return this.call("delete", kind, key, undefined, revision);
  }
  async close() {
    this.state = "closed";
    this.fail();
    await this.worker.terminate();
  }
}
export class Secrets {
  /** Only gateway-generated, unreferenced files older than a day are eligible. */
  async prune(references: Set<string>) {
    const deleted: string[] = [];
    for (const file of await readdir(this.dir)) {
      if (!/^(?:ca-|[a-f0-9-]{36}-)[a-f0-9-]{36}\.secret$/.test(file)) continue;
      const ref = file.slice(0, -7);
      if (references.has(ref)) continue;
      const info = await stat(this.path(ref));
      if (Date.now() - info.mtimeMs < 86400000) continue;
      await this.delete(ref);
      deleted.push(ref);
    }
    return deleted;
  }
  async delete(ref: string) {
    await unlink(this.path(ref)).catch((e) => {
      if (e.code !== "ENOENT") throw e;
    });
  }
  constructor(
    public dir: string,
    private key: Buffer,
  ) {}
  static async open(dir: string) {
    await mkdir(dir, { recursive: true, mode: 0o700 });
    const path = join(dir, "master.key");
    let key: Buffer;
    try {
      key = await readFile(path);
    } catch (e) {
      if ((e as NodeJS.ErrnoException).code !== "ENOENT") throw e;
      key = randomBytes(32);
      await writeFile(path, key, { mode: 0o600, flag: "wx" });
    }
    if (key.length !== 32) throw Error("Invalid secret key");
    return new Secrets(dir, key);
  }
  path(ref: string) {
    if (!/^[a-zA-Z0-9_-]{1,100}$/.test(ref))
      throw new GatewayError("INVALID_SECRET_REF");
    return resolve(this.dir, ref + ".secret");
  }
  async set(ref: string, value: unknown) {
    const iv = randomBytes(12),
      cipher = createCipheriv("aes-256-gcm", this.key, iv);
    const data = Buffer.concat([
      cipher.update(JSON.stringify(value)),
      cipher.final(),
    ]);
    await writeFile(
      this.path(ref),
      Buffer.concat([iv, cipher.getAuthTag(), data]),
      { mode: 0o600 },
    );
    await chmod(this.path(ref), 0o600);
  }
  async get<T = Record<string, unknown>>(ref?: string): Promise<T> {
    if (!ref) return {} as T;
    const raw = await readFile(this.path(ref));
    const d = createDecipheriv("aes-256-gcm", this.key, raw.subarray(0, 12));
    d.setAuthTag(raw.subarray(12, 28));
    return JSON.parse(
      Buffer.concat([d.update(raw.subarray(28)), d.final()]).toString(),
    );
  }
}
