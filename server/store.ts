import { Worker } from "node:worker_threads";
import { mkdir, readFile, writeFile, chmod, unlink } from "node:fs/promises";
import { join, resolve } from "node:path";
import { randomBytes, createCipheriv, createDecipheriv } from "node:crypto";
import { GatewayError } from "../shared/contracts.js";
import { existsSync } from "node:fs";
export class Store {
  private worker: Worker;
  private pending = new Map<
    number,
    { resolve: (v: any) => void; reject: (e: Error) => void }
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
      const p = this.pending.get(m.id);
      this.pending.delete(m.id);
      m.error ? p?.reject(new GatewayError(m.error)) : p?.resolve(m.result);
    });
    this.worker.on("error", (e) => {
      for (const p of this.pending.values()) p.reject(e as Error);
      this.pending.clear();
    });
  }
  call(
    op: string,
    kind: string,
    key?: string,
    value?: unknown,
    revision = 0,
  ): Promise<any> {
    return new Promise((resolve, reject) => {
      const id = ++this.seq;
      this.pending.set(id, { resolve, reject });
      this.worker.postMessage({ id, op, kind, key, value, revision });
    });
  }
  list(kind: string) {
    return this.call("list", kind);
  }
  get(kind: string, key: string) {
    return this.call("get", kind, key);
  }
  put(kind: string, key: string, value: unknown, revision = 0) {
    return this.call("put", kind, key, value, revision);
  }
  delete(kind: string, key: string, revision: number) {
    return this.call("delete", kind, key, undefined, revision);
  }
  async close() {
    await this.worker.terminate();
  }
}
export class Secrets {
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
  async get(ref?: string): Promise<any> {
    if (!ref) return {};
    const raw = await readFile(this.path(ref));
    const d = createDecipheriv("aes-256-gcm", this.key, raw.subarray(0, 12));
    d.setAuthTag(raw.subarray(12, 28));
    return JSON.parse(
      Buffer.concat([d.update(raw.subarray(28)), d.final()]).toString(),
    );
  }
}
