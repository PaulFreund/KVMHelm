import { Worker } from "node:worker_threads";
import { EventEmitter } from "node:events";
import { randomBytes } from "node:crypto";
import { WebSocketServer, WebSocket } from "ws";
import type { Server as HttpServer, IncomingMessage } from "node:http";
import { type AudioChunk, GatewayError } from "../shared/contracts.js";
import type { Core, Session } from "./core.js";
import type { Identity } from "./auth.js";
export class PiAudio extends EventEmitter {
  private worker?: Worker;
  private consumers = 0;
  private microphone = false;
  private available = {
    from_target: false,
    to_target: false,
    formats: [] as string[],
    reason: "Audio unavailable",
  };
  private sequence = 0;
  private closed = false;
  private retries = 0;
  private retry?: NodeJS.Timeout;
  private failed(code: string) {
    Object.assign(this.available, {
      from_target: false,
      to_target: false,
      reason: code,
    });
    this.microphone = false;
    this.emit("fault", new GatewayError(code));
    const worker = this.worker;
    this.worker = undefined;
    void worker?.terminate();
    if (!this.closed && this.retries < 3) {
      clearTimeout(this.retry);
      this.retry = setTimeout(
        () => {
          void this.probe();
        },
        1000 * 2 ** this.retries++,
      );
    }
  }
  constructor(private config: any) {
    super();
  }
  async probe() {
    if (this.worker) return this.available;
    const worker = new Worker(new URL("./audio-worker.js", import.meta.url), {
      workerData: this.config,
    });
    this.worker = worker;
    worker.on("message", (m) => {
      if (m.type === "features") {
        Object.assign(this.available, {
          from_target: m.audio,
          to_target: m.mic,
          formats: m.audio ? ["pcm_s16le/48000/2"] : [],
          reason: m.audio ? "" : "PiKVM reports no audio source",
        });
        this.emit("features");
      }
      if (m.type === "chunk") {
        const chunk = {
          ...m.chunk,
          sequence: this.sequence++,
          data: Buffer.from(m.chunk.data),
        };
        this.emit("chunk", chunk);
        worker.postMessage({ type: "credit" });
      }
      if (m.type === "error") {
        this.failed(m.code);
      }
    });
    worker.on("error", () => {
      this.failed("AUDIO_WORKER_FAILED");
    });
    return await new Promise<typeof this.available>((resolve) => {
      const timer = setTimeout(() => {
        this.off("features", done);
        resolve(this.available);
      }, 5500);
      const done = () => {
        clearTimeout(timer);
        resolve(this.available);
      };
      this.once("features", done);
    });
  }
  private watch() {
    this.worker?.postMessage({
      type: "watch",
      active: this.consumers > 0 || this.microphone,
      microphone: this.microphone,
    });
  }
  async *subscribe(signal: AbortSignal): AsyncIterable<AudioChunk> {
    if (!(await this.probe()).from_target)
      throw new GatewayError("UNSUPPORTED_ACTION", this.available.reason);
    const queue: AudioChunk[] = [];
    let wake: (() => void) | undefined;
    let fault: Error | undefined;
    let gap = false;
    const chunk = (c: AudioChunk) => {
      if (queue.length >= 10) {
        queue.shift();
        gap = true;
      }
      queue.push(gap ? { ...c, discontinuity: true } : c);
      gap = false;
      wake?.();
    };
    const fail = (e: Error) => {
      fault = e;
      wake?.();
    };
    const abort = () => wake?.();
    this.on("chunk", chunk);
    this.on("fault", fail);
    signal.addEventListener("abort", abort);
    this.consumers++;
    if (this.consumers === 1) this.watch();
    try {
      while (!signal.aborted) {
        if (fault) throw fault;
        if (queue.length) yield queue.shift()!;
        else await new Promise<void>((r) => (wake = r));
      }
    } finally {
      this.off("chunk", chunk);
      this.off("fault", fail);
      signal.removeEventListener("abort", abort);
      this.consumers--;
      if (!this.consumers) this.watch();
    }
  }
  async setMicrophone(enabled: boolean) {
    if (enabled && !(await this.probe()).to_target)
      throw new GatewayError(
        "UNSUPPORTED_ACTION",
        "PiKVM microphone not available",
      );
    if (this.microphone !== enabled) {
      this.microphone = enabled;
      this.watch();
    }
  }
  send(data: Buffer) {
    if (!this.microphone) throw new GatewayError("FORBIDDEN");
    if (data.length > 16384) throw new GatewayError("RESOURCE_LIMIT");
    this.worker?.postMessage({ type: "pcm", data });
  }
  async close() {
    this.closed = true;
    clearTimeout(this.retry);
    this.worker?.postMessage({ type: "stop" });
    const worker = this.worker;
    this.worker = undefined;
    if (worker) await worker.terminate();
  }
}
export function audioServer(
  server: HttpServer,
  core: Core,
  origins: Set<string>,
) {
  const tickets = new Map<
    string,
    {
      identity: Identity;
      session: Session;
      microphone: boolean;
      expires: number;
    }
  >();
  const sockets = new Set<WebSocket>();
  const senders = new Map<string, WebSocket>();
  const wss = new WebSocketServer({ noServer: true, maxPayload: 16384 });
  const authorize = (i: Identity, s: Session, mic: boolean) => {
    core.session(i, s.session_id);
    core.auth.check(i, "audio:read", s.device_id);
    if (mic) {
      core.auth.check(i, "audio:write", s.device_id);
      core.assertControl(i, s, core.get(s.device_id));
    }
  };
  const ticket = (
    identity: Identity,
    session: Session,
    microphone: boolean,
  ) => {
    authorize(identity, session, microphone);
    const d = core.get(session.device_id);
    if (!d.caps?.audio.from_target || (microphone && !d.caps.audio.to_target))
      throw new GatewayError(
        "UNSUPPORTED_ACTION",
        "Requested audio direction unavailable",
      );
    if (tickets.size >= 256) throw new GatewayError("RESOURCE_LIMIT");
    const id = randomBytes(32).toString("base64url");
    tickets.set(id, {
      identity,
      session,
      microphone,
      expires: Date.now() + 10000,
    });
    return id;
  };
  server.on("upgrade", (req, socket, head) => {
    if (
      req.url !== "/api/v1/audio" ||
      !req.headers.origin ||
      !origins.has(req.headers.origin)
    ) {
      socket.destroy();
      return;
    }
    wss.handleUpgrade(req, socket, head, (ws) =>
      wss.emit("connection", ws, req),
    );
  });
  wss.on("connection", (ws, req: IncomingMessage) => {
    sockets.add(ws);
    let entry: ReturnType<typeof tickets.get>, timer: NodeJS.Timeout;
    const abort = new AbortController();
    let micWindow = Date.now(),
      micBytes = 0;
    const unauth = setTimeout(() => ws.close(1008, "Authenticate"), 5000);
    ws.on("error", () => abort.abort());
    ws.on("close", () => {
      clearTimeout(unauth);
      clearInterval(timer);
      abort.abort();
      sockets.delete(ws);
      if (entry && senders.get(entry.session.device_id) === ws) {
        senders.delete(entry.session.device_id);
        const device = core.devices.get(entry.session.device_id);
        void (device?.driver as any)?.audio?.setMicrophone(false);
      }
    });
    ws.on("message", async (raw, binary) => {
      try {
        if (!entry) {
          if (binary) throw new GatewayError("FORBIDDEN");
          const m = JSON.parse(raw.toString()),
            candidate = tickets.get(m.ticket);
          tickets.delete(m.ticket);
          if (!candidate || candidate.expires < Date.now())
            throw new GatewayError("AUTH_FAILED");
          entry = candidate;
          authorize(entry.identity, entry.session, entry.microphone);
          if (entry.identity.ui_id) {
            const cookie = String(req.headers.cookie ?? "");
            if (
              !cookie
                .split(";")
                .some((c) => c.trim() === "kvmhelm=" + entry!.identity.ui_id)
            )
              throw new GatewayError("AUTH_FAILED");
          }
          clearTimeout(unauth);
          const { identity, session, microphone } = entry;
          const d = core.get(session.device_id);
          if (microphone) {
            if (senders.has(session.device_id))
              throw new GatewayError(
                "CONTROL_BUSY",
                "Microphone already active",
              );
            senders.set(session.device_id, ws);
            await (d.driver as any).audio.setMicrophone(true);
          }
          timer = setInterval(() => {
            try {
              authorize(identity, session, microphone);
            } catch {
              ws.close(1008, "Authorization expired");
            }
          }, 250);
          if (!d.driver.subscribeAudio)
            throw new GatewayError("UNSUPPORTED_ACTION");
          d.media.retain(`audio:${identity.client_id}`, "warm");
          void (async () => {
            try {
              for await (const chunk of d.driver.subscribeAudio!(
                abort.signal,
              )) {
                authorize(identity, session, microphone);
                if (ws.readyState !== WebSocket.OPEN) break;
                if (ws.bufferedAmount > 96000) {
                  ws.send(JSON.stringify({ gap: true }));
                  continue;
                }
                ws.send(chunk.data);
              }
            } catch {
              ws.close(1011, "Audio source unavailable");
            } finally {
              d.media.release(`audio:${identity.client_id}`);
            }
          })();
          return;
        }
        if (!binary || !entry.microphone) throw new GatewayError("FORBIDDEN");
        if (Date.now() - micWindow > 1000) {
          micWindow = Date.now();
          micBytes = 0;
        }
        micBytes += Buffer.from(raw as Buffer).length;
        if (micBytes > 128000) throw new GatewayError("RESOURCE_LIMIT");
        authorize(entry.identity, entry.session, true);
        const d = core.get(entry.session.device_id);
        (d.driver as any).audio.send(Buffer.from(raw as Buffer));
      } catch {
        ws.close(1008, "Audio authorization or source failed");
      }
    });
  });
  const sweep = setInterval(() => {
    for (const [k, t] of tickets) if (t.expires < Date.now()) tickets.delete(k);
  }, 5000);
  return {
    ticket,
    close() {
      clearInterval(sweep);
      for (const ws of sockets) ws.terminate();
      wss.close();
    },
  };
}
