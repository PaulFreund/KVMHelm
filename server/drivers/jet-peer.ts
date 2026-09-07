import { EventEmitter } from "node:events";
import {
  RTCPeerConnection,
  useH264,
  useOPUS,
  type RTCDataChannel,
} from "werift";
import OpusScript from "opusscript";
import { H264Assembler, H264Decoder } from "./h264.js";
import { GatewayError, type DriverFrame } from "../../shared/contracts.js";
/** Persistent native WebRTC peer. Only H.264 decoding/encoding runs in FFmpeg. */
export class JetPeer extends EventEmitter {
  private pc = new RTCPeerConnection({
    iceServers: [],
    iceUseIpv6: false,
    codecs: {
      video: [useH264({ mimeType: "video/H264" })],
      audio: [useOPUS()],
    },
    bundlePolicy: "max-bundle",
  });
  private channel: RTCDataChannel;
  private decoder = new H264Decoder();
  private opus?: OpusScript;
  private closed = false;
  private seq = 0;
  private audioEnabled = false;
  private pending = new Map<
    number,
    {
      resolve: (v: any) => void;
      reject: (e: Error) => void;
      timer: NodeJS.Timeout;
    }
  >();
  private candidates: any[] = [];
  private remoteSet = false;
  constructor() {
    super();
    this.channel = this.pc.createDataChannel("rpc", { ordered: true });
    this.pc.onIceCandidate.subscribe((c) => {
      if (c)
        this.emit("signal", { type: "new-ice-candidate", data: c.toJSON() });
    });
    this.pc.connectionStateChange.subscribe((s) => {
      if (["closed", "failed", "disconnected"].includes(s) && !this.closed)
        this.emit(
          "fault",
          new GatewayError("DEVICE_OFFLINE", "JetKVM WebRTC disconnected"),
        );
    });
    this.channel.onMessage.subscribe((raw) => {
      try {
        const m = JSON.parse(raw.toString()),
          p = this.pending.get(m.id);
        if (p) {
          clearTimeout(p.timer);
          this.pending.delete(m.id);
          m.error
            ? p.reject(
                new GatewayError("EXECUTION_UNKNOWN", "JetKVM rejected RPC"),
              )
            : p.resolve(m.result);
        } else this.emit("event", m.method, m.params);
      } catch {}
    });
    this.channel.stateChange.subscribe((s) => {
      if (s === "closed" && !this.closed)
        this.emit(
          "fault",
          new GatewayError("DEVICE_OFFLINE", "JetKVM input channel closed"),
        );
    });
    this.decoder.on("frame", (f) => this.emit("frame", f));
    this.decoder.on("fault", (e) => this.emit("fault", e));
    const video = this.pc.addTransceiver("video", { direction: "recvonly" });
    this.pc.addTransceiver("audio", { direction: "recvonly" });
    this.pc.onTrack.subscribe((track) => {
      if (track.kind === "video") {
        let lastPli = 0,
          needKey = true;
        const requestKey = () => {
          needKey = true;
          if (track.ssrc && Date.now() - lastPli > 500) {
            lastPli = Date.now();
            void video.receiver.sendRtcpPLI(track.ssrc).catch(() => {});
          }
        };
        const assembler = new H264Assembler((data, key) => {
          if (needKey && !key) {
            requestKey();
            return;
          }
          if (key) needKey = false;
          if (!this.decoder.write(data)) requestKey();
        }, requestKey);
        track.onReceiveRtp.subscribe((packet) => {
          try {
            assembler.push(packet);
          } catch {
            requestKey();
          }
        });
      } else if (track.kind === "audio") {
        this.opus = new OpusScript(48000, 2, OpusScript.Application.AUDIO);
        let previous: number | undefined;
        track.onReceiveRtp.subscribe((packet) => {
          if (!this.audioEnabled || this.closed) return;
          try {
            const seq = packet.header.sequenceNumber;
            const discontinuity =
              previous !== undefined && (seq - previous + 65536) % 65536 !== 1;
            previous = seq;
            this.emit(
              "audio",
              this.opus!.decode(packet.payload),
              discontinuity,
            );
          } catch {
            previous = undefined;
          }
        });
        this.emit("audioAvailable");
      }
    });
  }
  async offer() {
    await this.decoder.start();
    // werift supplies a public STUN default even with iceServers: []; keep LAN-only gathering.
    for (const transport of this.pc.iceTransports)
      transport.connection.stunServer = undefined;
    await this.pc.setLocalDescription(await this.pc.createOffer());
    return Buffer.from(
      JSON.stringify({ type: "offer", sdp: this.pc.localDescription!.sdp }),
    ).toString("base64");
  }
  async signal(m: any) {
    if (m.type === "answer") {
      await this.pc.setRemoteDescription(
        JSON.parse(Buffer.from(m.data, "base64").toString()),
      );
      this.remoteSet = true;
      for (const c of this.candidates.splice(0))
        await this.pc.addIceCandidate(c);
    } else if (m.type === "new-ice-candidate") {
      if (this.remoteSet) await this.pc.addIceCandidate(m.data);
      else this.candidates.push(m.data);
    }
  }
  async ready(signal: AbortSignal) {
    if (this.channel.readyState === "open") return;
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(
        () =>
          done(
            new GatewayError(
              "DEVICE_OFFLINE",
              "JetKVM WebRTC connection timed out",
            ),
          ),
        6000,
      );
      const subscription = this.channel.stateChange.subscribe((s) => {
        if (s === "open") done();
        else if (s === "closed") done(new GatewayError("DEVICE_OFFLINE"));
      });
      const abort = () =>
        done(new GatewayError("DEVICE_OFFLINE", "JetKVM connection cancelled"));
      const done = (e?: Error) => {
        clearTimeout(timer);
        subscription.unSubscribe();
        signal.removeEventListener("abort", abort);
        e ? reject(e) : resolve();
      };
      signal.addEventListener("abort", abort, { once: true });
      if (signal.aborted) abort();
    });
  }
  call(method: string, params: unknown): Promise<any> {
    if (this.channel.readyState !== "open")
      return Promise.reject(new GatewayError("DEVICE_OFFLINE"));
    if (this.channel.bufferedAmount > 65536 || this.pending.size >= 64)
      return Promise.reject(new GatewayError("RESOURCE_LIMIT"));
    return new Promise((resolve, reject) => {
      const id = ++this.seq,
        timer = setTimeout(() => {
          this.pending.delete(id);
          reject(
            new GatewayError(
              "EXECUTION_UNKNOWN",
              "JetKVM RPC response timed out",
            ),
          );
        }, 2000);
      this.pending.set(id, { resolve, reject, timer });
      try {
        this.channel.send(
          JSON.stringify({ jsonrpc: "2.0", id, method, params }),
        );
      } catch (e) {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(e);
      }
    });
  }
  async frame(signal: AbortSignal): Promise<DriverFrame> {
    signal.throwIfAborted();
    if (this.closed) throw new GatewayError("DEVICE_OFFLINE");
    return new Promise((resolve, reject) => {
      const frame = (f: DriverFrame) => {
        cleanup();
        resolve(f);
      };
      const fault = (e: Error) => {
        cleanup();
        reject(e);
      };
      const abort = () => fault(new GatewayError("FRAME_TIMEOUT"));
      const timer = setTimeout(abort, 1500);
      const cleanup = () => {
        clearTimeout(timer);
        this.off("frame", frame);
        this.off("fault", fault);
        signal.removeEventListener("abort", abort);
      };
      this.on("frame", frame);
      this.on("fault", fault);
      signal.addEventListener("abort", abort, { once: true });
    });
  }
  audio(enabled: boolean) {
    this.audioEnabled = enabled;
  }
  async close() {
    if (this.closed) return;
    this.closed = true;
    this.emit("fault", new GatewayError("DEVICE_OFFLINE"));
    for (const p of this.pending.values()) {
      clearTimeout(p.timer);
      p.reject(
        new GatewayError("EXECUTION_UNKNOWN", "JetKVM connection closed"),
      );
    }
    this.pending.clear();
    await this.pc.close();
    await this.decoder.close();
    this.opus?.delete();
    this.opus = undefined;
  }
}
