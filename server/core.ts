import { randomUUID, createHash } from "node:crypto";
import { EventEmitter } from "node:events";
import { setTimeout as delay } from "node:timers/promises";
import {
  type Device,
  type KvmDriver,
  type Frame,
  type ToolName,
  type Capabilities,
  deviceSchema,
  toolSchemas,
  GatewayError,
} from "../shared/contracts.js";
import { type Identity, Auth } from "./auth.js";
import { Store, Secrets } from "./store.js";
import { PiKvmDriver } from "./drivers/pikvm.js";
import { JetKvmDriver } from "./drivers/jetkvm.js";
import { CometDriver } from "./drivers/comet.js";
import { Simulator } from "./drivers/simulator.js";
import { Media } from "./media.js";
export interface Session {
  session_id: string;
  owner_id: "owner";
  client_id: string;
  token_id: string;
  ui_id?: string;
  device_id: string;
  mode: "observe" | "control";
  expires_at: number;
  connection_generation: number;
  last_view?: Frame["info"];
  references: Map<string, Frame["info"]>;
  journal: Map<string, { hash: string; result: Promise<ToolResult> }>;
}
interface Lease {
  session_id: string;
  client_id: string;
  expires_at: number;
  generation: number;
}
export interface Runtime {
  device: Device;
  driver: KvmDriver;
  media: Media;
  status: string;
  error?: string;
  caps?: Capabilities;
  lease?: Lease;
  suspended: boolean;
  controlGeneration: number;
  tail: Promise<unknown>;
  pending: number;
  connecting?: Promise<void>;
  abort?: AbortController;
  retryAt: number;
  failures: number;
  releasing?: Promise<void>;
  lastInputCompletedAt?: number;
}
export interface ToolResult {
  ok: boolean;
  [key: string]: unknown;
  images?: Frame[];
}
const stable = (v: any): string =>
  v === null || typeof v !== "object"
    ? JSON.stringify(v)
    : Array.isArray(v)
      ? "[" + v.map(stable).join(",") + "]"
      : "{" +
        Object.keys(v)
          .sort()
          .map((k) => JSON.stringify(k) + ":" + stable(v[k]))
          .join(",") +
        "}";
export class Core extends EventEmitter {
  devices = new Map<string, Runtime>();
  sessions = new Map<string, Session>();
  factories = new Map<string, (d: Device, s: any, ca?: string) => KvmDriver>();
  events: any[] = [];
  sequence = 0;
  private opens = new Map<
    string,
    Map<string, { hash: string; result: Promise<ToolResult> }>
  >();
  private sweep: NodeJS.Timeout;
  private replayImages: { result: ToolResult; bytes: number }[] = [];
  private replayBytes = 0;
  readonly limits = {
    maxSessions: Number(process.env.KVMHELM_MAX_SESSIONS ?? 128),
    replayBytes: Number(process.env.KVMHELM_REPLAY_MB ?? 64) * 1024 * 1024,
  };
  metrics = { dispatch_ms: [] as number[], batches: 0 };
  constructor(
    public store: Store,
    public secrets: Secrets,
    public auth: Auth,
    public leaseMs = 120000,
  ) {
    super();
    this.factories.set("pikvm-v4", (d, s, ca) => new PiKvmDriver(d, s, ca));
    this.factories.set("jetkvm", (d, s, ca) => new JetKvmDriver(d, s, ca));
    this.factories.set("glinet-comet", (d, s, ca) => new CometDriver(d, s, ca));
    this.factories.set("simulator", (d) => new Simulator(d));
    this.sweep = setInterval(() => void this.tick(), 1000);
    auth.onRevoke = async (id) => {
      for (const s of [...this.sessions.values()])
        if (s.token_id === id) await this.close(s);
      this.event("token.revoked", undefined, { token_id: id });
    };
  }
  event(type: string, device_id?: string, data: unknown = {}) {
    const e = {
      event_id: ++this.sequence,
      type,
      device_id,
      at: Date.now(),
      data,
    };
    this.events.push(e);
    if (this.events.length > 256) this.events.shift();
    this.emit("event", e);
  }
  async load() {
    for (const d of await this.store.list("devices")) await this.mount(d);
  }
  async mount(d: Device) {
    d = { ...d, minimum_action_latency_ms: d.minimum_action_latency_ms ?? 30 };
    const factory = this.factories.get(d.driver_id);
    if (!factory) throw new GatewayError("UNSUPPORTED_DRIVER");
    const secret = await this.secrets.get(d.secret_ref);
    const ca = d.tls.ca_ref
      ? (await this.secrets.get(d.tls.ca_ref)).pem
      : undefined;
    const driver = factory(d, secret, ca);
    const media = new Media(d.device_id, driver);
    const r: Runtime = {
      device: d,
      driver,
      media,
      status: d.enabled ? "disconnected" : "disabled",
      suspended: false,
      controlGeneration: 0,
      tail: Promise.resolve(),
      pending: 0,
      retryAt: 0,
      failures: 0,
    };
    driver.onDisconnected = () => {
      r.status = "reconnecting";
      r.error = "DEVICE_OFFLINE";
      r.abort?.abort();
      void this.revoke(r);
    };
    media.on("fault", (e) => {
      r.status = "reconnecting";
      r.error = e instanceof GatewayError ? e.code : "DEVICE_OFFLINE";
      r.abort?.abort();
      void this.revoke(r);
    });
    media.on("frame", () => {
      if (r.status === "ready" && r.media.latest?.info.signal === "absent")
        r.status = "no_signal";
      else if (r.status === "no_signal") r.status = "ready";
    });
    this.devices.set(d.device_id, r);
  }
  async connect(r: Runtime) {
    if (!r.device.enabled)
      throw new GatewayError("DEVICE_OFFLINE", "Device disabled");
    if (r.status === "ready" || r.status === "no_signal") return;
    if (r.connecting) return r.connecting;
    if (performance.now() < r.retryAt)
      throw new GatewayError(
        r.error ?? "DEVICE_OFFLINE",
        "Reconnect backoff active",
      );
    r.status = "connecting";
    r.connecting = (async () => {
      try {
        await r.media.pause();
        if (r.media.generation) await r.driver.disconnect();
        await r.driver.connect(AbortSignal.timeout(7000));
        r.caps = await r.driver.capabilities();
        r.media.reconnect();
        r.media.resume();
        r.status = "ready";
        r.error = undefined;
        r.failures = 0;
        this.event("device.connected", r.device.device_id);
        if (
          r.device.media_profile === "auto" &&
          r.device.driver_id !== "simulator"
        ) {
          void r.media
            .benchmark()
            .then((result) =>
              this.event("media.strategy", r.device.device_id, result),
            )
            .catch(() => {});
        }
      } catch (e) {
        r.status = "error";
        r.error = e instanceof GatewayError ? e.code : "DEVICE_OFFLINE";
        for (let cause: any = e, depth = 0; cause && depth < 5; cause = cause.cause, depth++) {
          if (["DEPTH_ZERO_SELF_SIGNED_CERT", "SELF_SIGNED_CERT_IN_CHAIN", "UNABLE_TO_VERIFY_LEAF_SIGNATURE", "UNABLE_TO_GET_ISSUER_CERT_LOCALLY", "ERR_TLS_CERT_ALTNAME_INVALID", "CERT_HAS_EXPIRED"].includes(cause.code))
            r.error = "TLS_CERTIFICATE_ERROR";
        }
        r.retryAt =
          performance.now() +
          (r.error === "AUTH_FAILED"
            ? 30000
            : Math.min(30000, 500 * 2 ** r.failures++));
        throw new GatewayError(r.error);
      } finally {
        r.connecting = undefined;
      }
    })();
    return r.connecting;
  }
  get(id: string) {
    const r = this.devices.get(id);
    if (!r) throw new GatewayError("DEVICE_OFFLINE", "Unknown device");
    return r;
  }
  describe(r: Runtime) {
    return {
      ...r.device,
      status: r.status,
      error: r.error,
      capabilities: r.caps ?? null,
      lease: r.lease
        ? {
            client_id: r.lease.client_id,
            session_id: r.lease.session_id,
            expires_at: r.lease.expires_at,
          }
        : null,
      control_suspended: r.suspended,
      consumers: [...r.media.consumers].map(([id, profile]) => ({
        id,
        profile,
      })),
      media: {
        strategy: r.media.strategy,
        metrics: r.media.metrics,
        frame_age_ms: r.media.latest
          ? performance.now() - r.media.latest.info.received_at_monotonic_ms
          : null,
      },
    };
  }
  list(i: Identity) {
    this.auth.check(i, "devices:read");
    return [...this.devices.values()]
      .filter(
        (r) =>
          i.token.devices.includes("*") ||
          i.token.devices.includes(r.device.device_id),
      )
      .map((r) => this.describe(r));
  }
  async saveDevice(
    i: Identity,
    input: unknown,
    secret?: unknown,
    ca?: string | null,
  ) {
    this.auth.check(i, "devices:manage");
    const parsed = deviceSchema.parse(input),
      id = parsed.device_id ?? randomUUID();
    this.auth.check(i, "devices:manage", id);
    if (parsed.transport === "network") {
      const u = new URL(parsed.address ?? "");
      if (
        !["http:", "https:"].includes(u.protocol) ||
        u.username ||
        u.password ||
        u.pathname !== "/" ||
        u.search ||
        u.hash
      )
        throw new GatewayError(
          "INVALID_ADDRESS",
          "Use an HTTP(S) origin without credentials or path",
        );
    }
    if (!this.factories.has(parsed.driver_id))
      throw new GatewayError("UNSUPPORTED_DRIVER");
    const existing = this.devices.get(id)?.device;
    if ((existing?.revision ?? 0) !== parsed.revision)
      throw new GatewayError("REVISION_CONFLICT");
    if (parsed.secret_ref && parsed.secret_ref !== existing?.secret_ref)
      throw new GatewayError(
        "FORBIDDEN",
        "Secret references cannot be reassigned between devices",
      );
    if (parsed.tls.ca_ref && parsed.tls.ca_ref !== existing?.tls.ca_ref)
      throw new GatewayError(
        "FORBIDDEN",
        "CA references cannot be reassigned between devices",
      );
    if (secret === null) delete parsed.secret_ref;
    if (ca === null) delete parsed.tls.ca_ref;
    if (ca) {
      parsed.tls.ca_ref = `ca-${randomUUID()}`;
      await this.secrets.set(parsed.tls.ca_ref, { pem: ca });
    }
    if (secret) {
      parsed.secret_ref = `${id}-${randomUUID()}`;
      await this.secrets.set(parsed.secret_ref, secret);
    }
    const d = await this.store.put(
      "devices",
      id,
      { ...parsed, device_id: id },
      parsed.revision,
    );
    const old = this.devices.get(id);
    if (old) await this.unmount(old);
    await this.mount(d);
    if (existing?.secret_ref && existing.secret_ref !== d.secret_ref)
      await this.secrets.delete(existing.secret_ref);
    if (existing?.tls.ca_ref && existing.tls.ca_ref !== d.tls.ca_ref)
      await this.secrets.delete(existing.tls.ca_ref);
    this.event("device.updated", id);
    return this.describe(this.get(id));
  }
  async unmount(r: Runtime) {
    for (const s of [...this.sessions.values()])
      if (s.device_id === r.device.device_id) await this.close(s);
    await r.media.stop();
    await r.driver.disconnect();
    this.devices.delete(r.device.device_id);
  }
  async deleteDevice(
    i: Identity,
    id: string,
    revision: number,
    confirmed: boolean,
  ) {
    this.auth.check(i, "devices:manage", id);
    const r = this.get(id);
    if ((r.media.consumers.size || r.lease) && !confirmed)
      throw new GatewayError(
        "CONFIRM_REQUIRED",
        "Confirm deletion of this active device",
      );
    await this.store.delete("devices", id, revision);
    await this.unmount(r);
    this.event("device.deleted", id);
  }
  session(i: Identity, id: string) {
    this.auth.check(i);
    const s = this.sessions.get(id);
    if (!s || s.client_id !== i.client_id || s.token_id !== i.token.id)
      throw new GatewayError(
        "FORBIDDEN",
        "Session does not belong to this client",
      );
    if (s.expires_at < Date.now())
      throw new GatewayError("LEASE_EXPIRED", "Session expired");
    this.auth.check(i, undefined, s.device_id);
    s.expires_at = Date.now() + 3600000;
    return s;
  }
  private async serialized<T>(r: Runtime, fn: () => Promise<T>): Promise<T> {
    if (r.pending >= 64)
      throw new GatewayError("RESOURCE_LIMIT", "Device command queue full");
    r.pending++;
    const result = r.tail.then(fn, fn);
    r.tail = result.catch(() => {});
    try {
      return await result;
    } finally {
      r.pending--;
    }
  }
  async revoke(r: Runtime) {
    if (r.releasing) return r.releasing;
    r.controlGeneration++;
    r.media.revision++;
    r.abort?.abort();
    r.lease = undefined;
    r.releasing = (async () => {
      try {
        await r.driver.releaseAllInputs();
      } catch {
        r.suspended = true;
        r.error = "EXECUTION_UNKNOWN";
      }
      this.event("control.changed", r.device.device_id);
    })();
    try {
      await r.releasing;
    } finally {
      r.releasing = undefined;
    }
  }
  private async acquire(i: Identity, s: Session, r: Runtime) {
    await r.releasing;
    this.auth.check(i, "input:write", s.device_id);
    if (r.suspended) throw new GatewayError("CONTROL_SUSPENDED");
    if (r.lease && r.lease.expires_at <= Date.now()) await this.revoke(r);
    if (r.lease && r.lease.session_id !== s.session_id)
      throw new GatewayError("CONTROL_BUSY", "Another client holds control", {
        client_id: r.lease.client_id,
        expires_at: r.lease.expires_at,
      });
    if (!r.lease) {
      r.controlGeneration++;
      r.media.revision++;
      r.lease = {
        session_id: s.session_id,
        client_id: i.client_id,
        generation: r.controlGeneration,
        expires_at: Date.now() + this.leaseMs,
      };
    } else r.lease.expires_at = Date.now() + this.leaseMs;
    s.mode = "control";
    s.last_view = undefined;
    this.event("control.changed", s.device_id);
    return r.lease;
  }
  assertControl(
    i: Identity,
    s: Session,
    r: Runtime,
    generation = r.controlGeneration,
  ) {
    this.auth.check(i, "input:write", s.device_id);
    if (!this.sessions.has(s.session_id) || s.expires_at <= Date.now())
      throw new GatewayError("LEASE_EXPIRED");
    if (r.suspended) throw new GatewayError("CONTROL_SUSPENDED");
    if (
      !r.lease ||
      r.lease.session_id !== s.session_id ||
      generation !== r.controlGeneration
    )
      throw new GatewayError("LEASE_REVOKED");
    if (r.lease.expires_at <= Date.now())
      throw new GatewayError("LEASE_EXPIRED");
  }
  async observe(
    i: Identity,
    s: Session,
    maxAge = 0,
    timeout = 1500,
    after = false,
  ) {
    this.auth.check(i, "video:read", s.device_id);
    const r = this.get(s.device_id);
    await this.connect(r);
    // A fixed deadline follows the latest input; observations never restart it.
    if (r.lastInputCompletedAt !== undefined) {
      const deadline = r.lastInputCompletedAt + (r.device.minimum_action_latency_ms ?? 30);
      while (performance.now() < deadline)
        await delay(Math.ceil(deadline - performance.now()), undefined, { signal: r.abort?.signal });
      // Cached frames received before the deadline cannot represent this observation.
      maxAge = Math.min(maxAge, Math.max(0, performance.now() - deadline));
      after ||= !r.media.latest || r.media.latest.info.received_at_monotonic_ms < deadline;
    }
    const f = await r.media.screenshot(maxAge, timeout, after);
    this.auth.check(i, "video:read", s.device_id);
    s.connection_generation = r.media.generation;
    s.last_view = f.info;
    s.references.set(f.info.frame_id, f.info);
    while (s.references.size > 64)
      s.references.delete(s.references.keys().next().value!);
    return f;
  }
  async close(s: Session) {
    const r = this.devices.get(s.device_id);
    this.sessions.delete(s.session_id);
    if (r?.lease?.session_id === s.session_id) await this.revoke(r);
    r?.media.release(s.session_id);
  }
  async disconnectClient(client: string) {
    for (const s of [...this.sessions.values()])
      if (s.client_id === client) await this.close(s);
    this.opens.delete(client);
  }
  private dedupe(
    journal: Map<string, { hash: string; result: Promise<ToolResult> }>,
    id: string,
    input: unknown,
    fn: () => Promise<ToolResult>,
  ) {
    const hash = createHash("sha256").update(stable(input)).digest("hex");
    const old = journal.get(id);
    if (old) {
      if (old.hash !== hash) throw new GatewayError("REQUEST_CONFLICT");
      return old.result;
    }
    if (journal.size >= 256)
      throw new GatewayError(
        "RESOURCE_LIMIT",
        "Request journal full; open a new session",
      );
    const result = fn()
      .catch((e) => this.error(e))
      .then((value) => {
        const bytes =
          value.images?.reduce((sum, f) => sum + f.data.length, 0) ?? 0;
        if (bytes) {
          this.replayImages.push({ result: value, bytes });
          this.replayBytes += bytes;
          while (
            this.replayBytes > this.limits.replayBytes &&
            this.replayImages.length > 1
          ) {
            const old = this.replayImages.shift()!;
            old.result.frames = old.result.images?.map((f) => f.info);
            old.result.images = [];
            old.result.replay_image_evicted = true;
            this.replayBytes -= old.bytes;
          }
        }
        return value;
      });
    journal.set(id, { hash, result });
    return result;
  }
  error(e: unknown): ToolResult {
    return {
      ok: false,
      error: {
        code: e instanceof GatewayError ? e.code : "EXECUTION_UNKNOWN",
        message:
          e instanceof GatewayError
            ? e.message
            : "Operation failed; observe before retrying",
        ...(e instanceof GatewayError ? e.details : {}),
      },
    };
  }
  async tool(i: Identity, name: ToolName, input: unknown): Promise<ToolResult> {
    try {
      this.auth.check(i);
      const a: any = toolSchemas[name].parse(input);
      if (name === "list_computers")
        return {
          ok: true,
          computers: this.list(i).filter(
            (d) =>
              !a.tags?.length ||
              a.tags.every((t: string) => d.tags.includes(t)),
          ),
        };
      if (name === "open_computer") {
        if (a.mode === "control" && !a.request_id)
          throw new GatewayError(
            "INVALID_ARGUMENT",
            "Control requires request_id",
          );
        const open = async (): Promise<ToolResult> => {
          this.auth.check(i, "video:read", a.computer_id);
          if (this.sessions.size >= this.limits.maxSessions)
            throw new GatewayError("RESOURCE_LIMIT");
          const r = this.get(a.computer_id);
          await this.connect(r);
          const s: Session = {
            session_id: randomUUID(),
            owner_id: "owner",
            client_id: i.client_id,
            token_id: i.token.id,
            ui_id: i.ui_id,
            device_id: a.computer_id,
            mode: a.mode,
            expires_at: Date.now() + 3600000,
            connection_generation: r.media.generation,
            references: new Map(),
            journal: new Map(),
          };
          this.sessions.set(s.session_id, s);
          try {
            if (a.mode === "control")
              await this.serialized(r, () => this.acquire(i, s, r));
            r.media.retain(
              s.session_id,
              a.profile ??
                (r.device.media_profile === "auto"
                  ? "active"
                  : r.device.media_profile),
            );
            const image = await this.observe(i, s);
            return {
              ok: true,
              session: this.publicSession(s),
              lease: r.lease ?? null,
              images: [image],
            };
          } catch (e) {
            await this.close(s);
            throw e;
          }
        };
        if (!a.request_id) return await open();
        if (!this.opens.has(i.client_id))
          this.opens.set(i.client_id, new Map());
        return await this.dedupe(
          this.opens.get(i.client_id)!,
          a.request_id,
          a,
          open,
        );
      }
      const s = this.session(i, a.session_id),
        r = this.get(s.device_id);
      if (name === "close_computer") {
        await this.close(s);
        return { ok: true };
      }
      if (name === "computer_screenshot")
        return {
          ok: true,
          images: [await this.observe(i, s, a.max_age_ms, a.timeout_ms)],
        };
      return await this.dedupe(s.journal, a.request_id, { name, ...a }, () =>
        this.serialized(r, async () => {
          this.session(i, s.session_id);
          if (name === "computer_control") {
            if (a.operation === "acquire") await this.acquire(i, s, r);
            else if (a.operation === "renew") {
              this.assertControl(i, s, r);
              r.lease!.expires_at = Date.now() + this.leaseMs;
            } else if (r.lease?.session_id === s.session_id) {
              this.auth.check(i, "input:write", s.device_id);
              await this.revoke(r);
              s.mode = "observe";
            }
            return { ok: true, lease: r.lease ?? null };
          }
          this.assertControl(i, s, r);
          await this.connect(r);
          const ref = s.references.get(a.reference.frame_id);
          if (
            !ref ||
            ref.view_id !== a.reference.view_id ||
            ref.view_id !== r.media.view ||
            ref.connection_generation !== r.media.generation
          )
            throw new GatewayError("VIEW_CHANGED");
          if (
            a.reference.input_revision !== r.media.revision ||
            ref.input_revision !== r.media.revision
          )
            throw new GatewayError("STALE_REFERENCE");
          if (performance.now() - ref.received_at_monotonic_ms > 30000)
            throw new GatewayError(
              "STALE_REFERENCE",
              "Observe again before input",
            );
          r.driver.validate(a.actions, ref.width, ref.height);
          const count =
            a.actions.filter((x: any) => x.type === "screenshot").length +
            (a.observation?.mode === "none" ? 0 : 1);
          if (
            count > 4 ||
            a.actions.reduce(
              (n: number, x: any) =>
                n + (x.type === "wait" ? x.duration_ms : 0),
              0,
            ) > 25000
          )
            throw new GatewayError("RESOURCE_LIMIT");
          if (count) this.auth.check(i, "video:read", s.device_id);
          const statuses = a.actions.map((x: any) => ({
            type: x.type,
            status: "not_started",
          }));
          const images: Frame[] = [];
          const controller = new AbortController();
          r.abort = controller;
          const signal = AbortSignal.any([
            controller.signal,
            AbortSignal.timeout(30000),
          ]);
          const generation = r.controlGeneration;
          let error: unknown;
          const context = {
            signal,
            width: ref.width,
            height: ref.height,
            assertControl: () => {
              this.assertControl(i, s, r, generation);
              if (r.media.view !== ref.view_id)
                throw new GatewayError("VIEW_CHANGED");
            },
          };
          for (let n = 0; n < a.actions.length; n++) {
            try {
              context.assertControl();
              signal.throwIfAborted();
              const action = a.actions[n];
              if (action.type === "wait") {
                await delay(action.duration_ms, undefined, { signal });
                statuses[n].status = "acknowledged";
              } else if (action.type === "screenshot") {
                images.push(
                  await this.observe(
                    i,
                    s,
                    0,
                    a.observation?.timeout_ms ?? 1500,
                    true,
                  ),
                );
                statuses[n].status = "acknowledged";
              } else {
                statuses[n].status = "unknown";
                r.media.revision++;
                const start = performance.now();
                try {
                  statuses[n].status = await r.driver.execute(action, context);
                } finally {
                  r.lastInputCompletedAt = performance.now();
                }
                this.metrics.dispatch_ms.push(performance.now() - start);
                if (this.metrics.dispatch_ms.length > 512)
                  this.metrics.dispatch_ms.shift();
              }
              context.assertControl();
              signal.throwIfAborted();
              r.lease!.expires_at = Date.now() + this.leaseMs;
            } catch (e) {
              error = e;
              break;
            }
          }
          if (error) {
            try {
              await r.driver.releaseAllInputs();
            } catch {
              r.suspended = true;
            }
            this.event("action.failed", s.device_id, { statuses });
          } else if (a.observation?.mode !== "none") {
            try {
              images.push(
                await this.observe(
                  i,
                  s,
                  0,
                  a.observation?.timeout_ms ?? 1500,
                  true,
                ),
              );
            } catch (e) {
              error = e;
            }
          }
          r.abort = undefined;
          this.metrics.batches++;
          const result: ToolResult = {
            ok: !error,
            actions: statuses,
            input_revision: r.media.revision,
            images,
          };
          if (error) result.error = this.error(error).error;
          return result;
        }),
      );
    } catch (e) {
      if ((e as any)?.name === "ZodError")
        return {
          ok: false,
          error: {
            code: "INVALID_ARGUMENT",
            message: "Request does not match the strict input schema",
          },
        };
      return this.error(e);
    }
  }
  publicSession(s: Session) {
    return {
      session_id: s.session_id,
      owner_id: s.owner_id,
      client_id: s.client_id,
      device_id: s.device_id,
      mode: s.mode,
      expires_at: s.expires_at,
      connection_generation: s.connection_generation,
    };
  }
  async stopControl(i: Identity, id: string, resume = false) {
    this.auth.check(i, "control:stop", id);
    const r = this.get(id);
    if (resume) {
      await r.tail;
      await r.driver.releaseAllInputs();
      r.suspended = false;
      this.event("control.resumed", id);
    } else {
      r.suspended = true;
      await this.revoke(r);
      this.event("control.suspended", id);
    }
    return { ok: true, control_suspended: r.suspended };
  }
  private async tick() {
    for (const s of [...this.sessions.values()]) {
      try {
        this.auth.check({
          token: this.auth.tokens.get(s.token_id)!,
          client_id: s.client_id,
          ui_id: s.ui_id,
        });
        if (s.expires_at <= Date.now()) await this.close(s);
      } catch {
        await this.close(s);
      }
    }
    for (const r of this.devices.values()) {
      if (r.lease && r.lease.expires_at <= Date.now()) await this.revoke(r);
      if (
        r.device.enabled &&
        r.media.consumers.size &&
        ["error", "reconnecting", "disconnected"].includes(r.status)
      )
        void this.connect(r).catch(() => {});
    }
  }
  async shutdown() {
    clearInterval(this.sweep);
    for (const r of [...this.devices.values()]) await this.unmount(r);
  }
}
