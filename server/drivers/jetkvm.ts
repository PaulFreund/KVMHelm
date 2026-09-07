import WebSocket from "ws";
import { EventEmitter } from "node:events";
import { PiKvmDriver } from "./pikvm.js";
import { JetPeer } from "./jet-peer.js";
import { usbKey } from "./usb-key.js";
import { textKeys } from "./keyboard.js";
import { localLookup, PinnedAgent, isLocalAddress } from "../network.js";
import {
  GatewayError,
  type Device,
  type ActionContext,
  type ComputerAction,
  type DriverFrame,
  type AudioChunk,
} from "../../shared/contracts.js";
/** Reuses only LAN/TLS HTTP transport and HID action planning; Jet endpoints are independent. */
export class JetKvmDriver extends PiKvmDriver {
  private peer?: JetPeer;
  private signaling?: WebSocket;
  private pulse?: NodeJS.Timeout;
  private pointer = { x: 0, y: 0, buttons: 0 };
  private reports = new Set<number>();
  private sound = new EventEmitter();
  private audioSequence = 0;
  private audioUsers = 0;
  constructor(
    device: Device,
    private secret: { password?: string },
    ca?: string,
  ) {
    super(device, {}, ca);
    this.headers = { "Content-Type": "application/json" };
    this.caps.video = {
      snapshot: true,
      stream: true,
      formats: ["image/jpeg"],
      signal: "unknown",
    };
    this.caps.audio = {
      from_target: false,
      to_target: false,
      formats: [],
      reason:
        "No incoming JetKVM audio track detected; microphone transmission is unavailable",
    };
    this.caps.extensions = {
      vendor: "JetKVM",
      protocol: "webrtc-jsonrpc",
      decoder: "ffmpeg-h264",
    };
  }
  override async connect(signal: AbortSignal) {
    try {
      await this.prepareTransport();
      this.headers["Content-Type"] = "application/json";
      const status = (await (
        await this.http("/device/status", signal)
      ).body.json()) as any;
      if (status.isSetup === false)
        throw new GatewayError(
          "DEVICE_NOT_CONFIGURED",
          "Complete JetKVM initial setup first",
        );
      let authenticated = false;
      try {
        await (await this.http("/device", signal)).body.dump();
        authenticated = true;
      } catch (e) {
        if (!(e instanceof GatewayError) || e.code !== "AUTH_FAILED") throw e;
      }
      if (!authenticated) {
        const response = await this.http(
          "/auth/login-local",
          signal,
          "POST",
          JSON.stringify({ password: this.secret.password ?? "" }),
        );
        const cookies = response.headers["set-cookie"];
        const cookie = (Array.isArray(cookies) ? cookies : [cookies]).find(
          (x) => x?.startsWith("authToken="),
        );
        await response.body.dump();
        if (!cookie)
          throw new GatewayError(
            "AUTH_FAILED",
            "JetKVM did not return an authentication cookie",
          );
        this.headers.Cookie = cookie.split(";")[0];
      }
      this.peer = new JetPeer();
      this.peer.on("signal", (m) => {
        if (this.signaling?.readyState === WebSocket.OPEN)
          this.signaling.send(JSON.stringify(m));
      });
      this.peer.on("fault", () => {
        if (!this.closing) this.onDisconnected?.();
      });
      this.peer.on("event", (method, value) => {
        if (method === "videoState")
          this.caps.video.signal =
            value?.ready === true
              ? "present"
              : value?.ready === false
                ? "absent"
                : "unknown";
        if (method === "usbState") {
          this.caps.keyboard.hid = value === "configured";
          this.caps.mouse.absolute = this.caps.keyboard.hid;
          this.caps.mouse.relative = this.caps.keyboard.hid;
        }
        if (method === "otherSessionConnected") this.onDisconnected?.();
      });
      this.peer.on("audioAvailable", () => {
        this.caps.audio = {
          from_target: true,
          to_target: false,
          formats: ["pcm_s16le"],
          reason:
            "Incoming WebRTC audio; microphone transmission is unavailable",
        };
      });
      this.peer.on("audio", (data: Buffer, discontinuity: boolean) =>
        this.sound.emit("chunk", {
          device_id: this.device.device_id,
          format: "pcm_s16le",
          sample_rate: 48000,
          channels: 2,
          sequence: ++this.audioSequence,
          timestamp_ms: performance.now(),
          connection_generation: 0,
          discontinuity,
          data,
        } satisfies AudioChunk),
      );
      const url = new URL("/webrtc/signaling/client", this.device.address!);
      url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
      const ws = (this.signaling = new WebSocket(url, {
        headers: this.headers,
        ...(await this.tlsOptions()),
        lookup: localLookup,
        agent: this.device.tls.fingerprint
          ? new PinnedAgent(this.device.tls.fingerprint)
          : undefined,
        handshakeTimeout: 5000,
      }));
      ws.on("error", () => {});
      ws.on("close", () => {
        if (!this.closing) this.onDisconnected?.();
      });
      const abort = () => ws.terminate();
      signal.addEventListener("abort", abort, { once: true });
      try {
        await new Promise<void>((resolve, reject) => {
          ws.once("open", resolve);
          ws.once("error", reject);
          ws.once("close", () => reject(new GatewayError("DEVICE_OFFLINE")));
        });
      } finally {
        signal.removeEventListener("abort", abort);
      }
      const offer = await this.peer.offer();
      let incoming = Promise.resolve();
      ws.on("message", (raw) => {
        incoming = incoming
          .then(async () => {
            const m = JSON.parse(raw.toString());
            if (m.error)
              throw new GatewayError(
                "DEVICE_OFFLINE",
                "JetKVM signaling rejected the offer",
              );
            if (m.type === "answer") {
              const answer = JSON.parse(
                Buffer.from(m.data, "base64").toString(),
              );
              for (const line of String(answer.sdp).split("\n"))
                if (line.startsWith("a=candidate:"))
                  this.checkCandidate(line.slice(2));
            }
            if (m.type === "new-ice-candidate")
              this.checkCandidate(m.data.candidate);
            if (m.type === "answer" || m.type === "new-ice-candidate")
              await this.peer?.signal(m);
          })
          .catch(() => {
            ws.terminate();
          });
      });
      ws.send(
        JSON.stringify({ type: "offer", data: { sd: offer, iceServers: [] } }),
      );
      await this.peer.ready(signal);
      signal.throwIfAborted();
      const video = await this.rpc("getVideoState", {});
      this.caps.video.signal =
        video?.ready === true
          ? "present"
          : video?.ready === false
            ? "absent"
            : "unknown";
      const usb = await this.rpc("getUSBState", {});
      this.caps.keyboard.hid = usb === "configured";
      this.caps.mouse.absolute = this.caps.keyboard.hid;
      this.caps.mouse.relative = this.caps.keyboard.hid;
      let pong = Date.now();
      ws.on("pong", () => {
        pong = Date.now();
      });
      this.pulse = setInterval(() => {
        if (Date.now() - pong > 30000) ws.terminate();
        else if (ws.readyState === WebSocket.OPEN) ws.ping();
      }, 10000);
      await this.releaseAllInputs();
    } catch (error) {
      await this.disconnect().catch(() => {});
      throw error;
    }
  }
  private checkCandidate(candidate: string) {
    const address = candidate.trim().split(/\s+/)[4];
    if (address && !isLocalAddress(address))
      throw new GatewayError(
        "INVALID_ADDRESS",
        "JetKVM ICE must use local/LAN addresses",
      );
  }
  protected async rpc(method: string, params: unknown): Promise<any> {
    if (!this.peer) throw new GatewayError("DEVICE_OFFLINE");
    return this.peer.call(method, params);
  }
  override validate(actions: ComputerAction[], width: number, height: number) {
    super.validate(actions, width, height);
    for (const action of actions) {
      if (action.type === "keypress") {
        const codes = new Set(action.keys.map(usbKey));
        if ([...codes].filter((code) => code < 224).length > 6)
          throw new GatewayError(
            "UNSUPPORTED_ACTION",
            "JetKVM supports six simultaneous non-modifier keys",
          );
      }
      if (action.type === "type")
        for (const keys of textKeys(action.text, this.device.keyboard_layout))
          keys.forEach(usbKey);
    }
  }
  protected override async event(
    type: string,
    event: any,
    context?: ActionContext,
  ) {
    context?.signal.throwIfAborted();
    context?.assertControl();
    if (type === "key") {
      const code = usbKey(event.key);
      if (event.state) this.reports.add(code);
      else this.reports.delete(code);
      let modifier = 0;
      const keys: number[] = [];
      for (const key of this.reports)
        key >= 224 ? (modifier |= 1 << (key - 224)) : keys.push(key);
      if (keys.length > 6)
        throw new GatewayError(
          "UNSUPPORTED_ACTION",
          "JetKVM supports six simultaneous non-modifier keys",
        );
      await this.rpc("keyboardReport", { modifier, keys });
    } else if (type === "mouse_move") {
      this.pointer.x = Math.round(((event.to.x + 32768) * 32767) / 65535);
      this.pointer.y = Math.round(((event.to.y + 32768) * 32767) / 65535);
      await this.rpc("absMouseReport", this.pointer);
    } else if (type === "mouse_button") {
      const bit = ({ left: 1, right: 2, middle: 4 } as Record<string, number>)[
        event.button
      ];
      this.pointer.buttons = event.state
        ? this.pointer.buttons | bit
        : this.pointer.buttons & ~bit;
      await this.rpc("absMouseReport", this.pointer);
    } else if (type === "mouse_wheel")
      await this.rpc("wheelReport", {
        wheelY: event.delta.y,
        wheelX: event.delta.x,
      });
  }
  override async releaseAllInputs() {
    if (!this.peer) {
      if (this.reports.size || this.pointer.buttons)
        throw new GatewayError("EXECUTION_UNKNOWN");
      return;
    }
    await this.rpc("keyboardReport", { modifier: 0, keys: [] });
    await this.rpc("absMouseReport", { ...this.pointer, buttons: 0 });
    this.reports.clear();
    this.pointer.buttons = 0;
    this.keys.clear();
    this.buttons.clear();
  }
  override async snapshot(signal: AbortSignal): Promise<DriverFrame> {
    signal.throwIfAborted();
    if (this.caps.video.signal === "absent")
      throw new GatewayError("NO_SIGNAL");
    if (!this.peer) throw new GatewayError("DEVICE_OFFLINE");
    const frame = await this.peer.frame(signal);
    return { ...frame, signal: this.caps.video.signal };
  }
  override async *subscribeVideo(
    signal: AbortSignal,
  ): AsyncIterable<DriverFrame> {
    while (!signal.aborted) {
      yield await this.snapshot(signal);
    }
  }
  override async *subscribeAudio(
    signal: AbortSignal,
  ): AsyncIterable<AudioChunk> {
    if (!this.caps.audio.from_target || !this.peer)
      throw new GatewayError("UNSUPPORTED_ACTION");
    const queue: AudioChunk[] = [];
    let wake = () => {};
    const receive = (chunk: AudioChunk) => {
      if (queue.length >= 8) {
        queue.shift();
        chunk = { ...chunk, discontinuity: true };
      }
      queue.push(chunk);
      wake();
    };
    const stop = () => wake();
    signal.addEventListener("abort", stop);
    this.sound.on("chunk", receive);
    this.sound.on("end", stop);
    try {
      if (++this.audioUsers === 1) this.peer.audio(true);
      while (!signal.aborted && !this.closing) {
        if (queue.length) yield queue.shift()!;
        else
          await new Promise<void>((r) => {
            wake = r;
          });
      }
    } finally {
      this.sound.off("chunk", receive);
      this.sound.off("end", stop);
      signal.removeEventListener("abort", stop);
      if (--this.audioUsers === 0) this.peer?.audio(false);
    }
  }
  override async disconnect() {
    this.closing = true;
    this.sound.emit("end");
    clearInterval(this.pulse);
    this.signaling?.terminate();
    this.signaling = undefined;
    await this.peer?.close();
    this.peer = undefined;
    await super.disconnect();
  }
}
