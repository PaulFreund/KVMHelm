import WebSocket from "ws";
import { createHash } from "node:crypto";
import sharp from "sharp";
import {
  type KvmDriver,
  type Device,
  type Capabilities,
  type DriverFrame,
  type ComputerAction,
  type ActionContext,
  GatewayError,
} from "../../shared/contracts.js";
import { PiAudio } from "../audio.js";
import type { AudioChunk } from "../../shared/contracts.js";
import { localLookup, PinnedAgent, isLocalAddress } from "../network.js";
import { NetworkHidDriver } from "./network-hid.js";
export class PiKvmDriver extends NetworkHidDriver {
  private lastPong = Date.now();
  private ws?: WebSocket;
  private heartbeat?: NodeJS.Timeout;
  audio?: PiAudio;
  async connect(signal: AbortSignal) {
    await this.prepareTransport();
    this.lastPong = Date.now();
    await this.json("/api/auth/check", signal);
    const hid = await this.json("/api/hid", signal);
    this.caps.mouse.absolute = hid.mouse?.absolute === true;
    this.caps.mouse.relative = !this.caps.mouse.absolute;
    this.caps.keyboard.hid = hid.keyboard?.online === true;
    const url = new URL("/api/ws", this.device.address!);
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    const opts = await this.tlsOptions();
    const ws = new WebSocket(url, {
      headers: this.headers,
      ...opts,
      lookup: localLookup,
      agent: this.device.tls.fingerprint
        ? new PinnedAgent(this.device.tls.fingerprint)
        : undefined,
      handshakeTimeout: 5000,
    });
    this.ws = ws;
    await new Promise<void>((resolve, reject) => {
      ws.once("open", () => {
        if (this.device.tls.fingerprint) {
          const socket = (ws as any)._socket;
          const cert = socket.getPeerCertificate();
          if (
            createHash("sha256").update(cert.raw).digest("hex") !==
            this.device.tls.fingerprint.replaceAll(":", "").toLowerCase()
          ) {
            ws.terminate();
            reject(new GatewayError("AUTH_FAILED"));
            return;
          }
        }
        resolve();
      });
      ws.once("error", reject);
    });
    ws.on("error", () => {});
    ws.on("close", () => {
      if (!this.closing) this.onDisconnected?.();
    });
    ws.on("message", (data) => {
      try {
        const m = JSON.parse(data.toString());
        if (m.event_type === "pong") this.lastPong = Date.now();
        if (m.event_type === "hid") {
          this.caps.keyboard.hid = m.event.keyboard?.online === true;
          this.caps.mouse.absolute = m.event.mouse?.absolute === true;
        }
        if (m.event_type === "streamer") {
          const online = m.event.streamer?.source?.online;
          if (typeof online === "boolean")
            this.caps.video.signal = online ? "present" : "absent";
        }
      } catch {}
    });
    this.heartbeat = setInterval(() => {
      if (Date.now() - this.lastPong > 30000) {
        ws.terminate();
        return;
      }
      if (ws.readyState === WebSocket.OPEN)
        ws.send(JSON.stringify({ event_type: "ping", event: {} }));
    }, 10000);
    await this.releaseAllInputs();
    const audio = (this.audio = new PiAudio({
      device_id: this.device.device_id,
      address: this.device.address,
      headers: this.headers,
      ca: this.ca,
      fingerprint: this.device.tls.fingerprint,
      insecure: this.device.tls.insecure,
    }));
    void audio.probe().then((caps) => {
      if (this.audio === audio) this.caps.audio = caps;
    });
  }
  async disconnect() {
    this.closing = true;
    await this.audio?.close();
    clearInterval(this.heartbeat);
    this.ws?.close();
    this.ws = undefined;
    await this.agent.close();
  }
  async capabilities() {
    return this.caps;
  }
  async *subscribeAudio(signal: AbortSignal): AsyncIterable<AudioChunk> {
    if (!this.audio) throw new GatewayError("UNSUPPORTED_ACTION");
    yield* this.audio.subscribe(signal);
  }
  async sendAudio(chunk: AudioChunk, context: ActionContext) {
    context.assertControl();
    if (!this.audio) throw new GatewayError("UNSUPPORTED_ACTION");
    this.audio.send(chunk.data);
  }
  async setMicrophone(enabled: boolean) {
    if (!this.audio) {
      if (enabled) throw new GatewayError("UNSUPPORTED_ACTION");
      return;
    }
    await this.audio.setMicrophone(enabled);
  }
  async snapshot(signal: AbortSignal): Promise<DriverFrame> {
    const r = await this.http("/api/streamer/snapshot", signal);
    const parts: Buffer[] = [];
    let size = 0;
    for await (const part of r.body) {
      size += part.length;
      if (size > 8 * 1024 * 1024) {
        r.body.destroy();
        throw new GatewayError("RESOURCE_LIMIT");
      }
      parts.push(Buffer.from(part));
    }
    const data = Buffer.concat(parts);
    const meta = await sharp(data, { limitInputPixels: 16777216 }).metadata();
    if (
      !meta.width ||
      !meta.height ||
      !["jpeg", "png"].includes(meta.format ?? "")
    )
      throw new GatewayError("FRAME_TIMEOUT", "Invalid snapshot");
    return {
      data,
      width: meta.width,
      height: meta.height,
      mime_type: meta.format === "png" ? "image/png" : "image/jpeg",
      signal: this.caps.video.signal,
    };
  }
  async *subscribeVideo(signal: AbortSignal): AsyncIterable<DriverFrame> {
    const r = await this.http("/streamer/stream", signal);
    let buffer = Buffer.alloc(0);
    for await (const raw of r.body) {
      buffer = Buffer.concat([buffer, raw]);
      if (buffer.length > 12 * 1024 * 1024)
        throw new GatewayError("RESOURCE_LIMIT", "MJPEG buffer limit");
      for (;;) {
        const start = buffer.indexOf(Buffer.from([255, 216]));
        if (start < 0) {
          if (buffer.length > 65536) buffer = buffer.subarray(-2);
          break;
        }
        const end = buffer.indexOf(Buffer.from([255, 217]), start + 2);
        if (end < 0) {
          buffer = buffer.subarray(start);
          break;
        }
        const data = Buffer.from(buffer.subarray(start, end + 2));
        buffer = buffer.subarray(end + 2);
        const m = await sharp(data, { limitInputPixels: 16777216 }).metadata();
        yield {
          data,
          width: m.width!,
          height: m.height!,
          mime_type: "image/jpeg",
          signal: this.caps.video.signal,
        };
      }
    }
  }
  protected async event(type: string, event: unknown, context?: ActionContext) {
    context?.signal.throwIfAborted();
    context?.assertControl();
    if (this.ws?.readyState !== WebSocket.OPEN)
      throw new GatewayError("DEVICE_OFFLINE");
    if (this.ws.bufferedAmount > 65536)
      throw new GatewayError("RESOURCE_LIMIT", "HID transport congested");
    await new Promise<void>((resolve, reject) =>
      this.ws!.send(JSON.stringify({ event_type: type, event }), (e) =>
        e ? reject(e) : resolve(),
      ),
    );
  }
  async releaseAllInputs() {
    await this.audio?.setMicrophone(false);
    if (this.ws?.readyState !== WebSocket.OPEN) {
      if (this.keys.size || this.buttons.size)
        throw new GatewayError(
          "EXECUTION_UNKNOWN",
          "Input release could not be confirmed",
        );
      return;
    }
    for (const key of this.keys) await this.event("key", { key, state: false });
    for (const button of this.buttons)
      await this.event("mouse_button", { button, state: false });
    await this.event("key", { key: "ControlLeft", state: false });
    this.keys.clear();
    this.buttons.clear();
  }
}
