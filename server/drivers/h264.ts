import { createRequire } from "node:module";
const ffmpeg: string | null = createRequire(import.meta.url)("ffmpeg-static");
import { existsSync } from "node:fs";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { EventEmitter } from "node:events";
import sharp from "sharp";
import { GatewayError, type DriverFrame } from "../../shared/contracts.js";
import type { RtpPacket } from "werift";
const prefix = Buffer.from([0, 0, 0, 1]);
const aud = Buffer.from([0, 0, 0, 1, 9, 0xf0]);
/** Bounded RFC 6184 mode-1 assembly; never forward incomplete access units. */
export class H264Assembler {
  private sequence?: number;
  private timestamp?: number;
  private units: Buffer[] = [];
  private fragments: Buffer[] = [];
  private size = 0;
  private damaged = false;
  private parameters = new Map<number, Buffer>();
  constructor(
    private output: (data: Buffer, keyframe: boolean) => void,
    private lost: () => void,
  ) {}
  push(packet: RtpPacket) {
    const { sequenceNumber: seq, timestamp, marker } = packet.header,
      p = packet.payload;
    if (!p.length) return;
    let gap = false;
    if (this.sequence !== undefined) {
      const delta = (seq - this.sequence + 65536) % 65536;
      if (delta === 0 || delta > 32768) return;
      if (delta !== 1) {
        gap = true;
        this.units = [];
        this.fragments = [];
        this.size = 0;
        this.damaged = true;
        this.lost();
      }
    }
    this.sequence = seq;
    if (timestamp !== this.timestamp) {
      this.units = [];
      this.fragments = [];
      this.size = 0;
      this.timestamp = timestamp;
      this.damaged = gap;
    }
    this.size += p.length;
    if (this.size > 8 * 1024 * 1024) {
      this.damaged = true;
      this.lost();
      return;
    }
    const type = p[0] & 31;
    if (type > 0 && type < 24) this.units.push(p);
    else if (type === 24) {
      let offset = 1;
      while (offset < p.length) {
        if (offset + 2 > p.length) {
          this.damaged = true;
          break;
        }
        const length = p.readUInt16BE(offset);
        offset += 2;
        if (!length || offset + length > p.length) {
          this.damaged = true;
          break;
        }
        this.units.push(p.subarray(offset, offset + length));
        offset += length;
      }
    } else if (type === 28 && p.length >= 3) {
      if (p[1] & 128)
        this.fragments = [
          Buffer.from([(p[0] & 224) | (p[1] & 31)]),
          p.subarray(2),
        ];
      else if (this.fragments.length) this.fragments.push(p.subarray(2));
      else this.damaged = true;
      if (p[1] & 64) {
        if (this.fragments.length)
          this.units.push(Buffer.concat(this.fragments));
        this.fragments = [];
      }
    } else this.damaged = true;
    if (marker) {
      if (!this.damaged && !this.fragments.length && this.units.length) {
        for (const unit of this.units) {
          const kind = unit[0] & 31;
          if ((kind === 7 || kind === 8) && unit.length <= 65536)
            this.parameters.set(kind, Buffer.from(unit));
        }
        const keyframe = this.units.some((n) => (n[0] & 31) === 5);
        const units = keyframe
          ? [
              ...this.parameters.values(),
              ...this.units.filter((n) => ![7, 8].includes(n[0] & 31)),
            ]
          : this.units;
        this.output(Buffer.concat(units.flatMap((n) => [prefix, n])), keyframe);
      } else this.lost();
      this.units = [];
      this.fragments = [];
      this.size = 0;
    }
  }
}
export class H264Decoder extends EventEmitter {
  private child?: ChildProcessWithoutNullStreams;
  private buffer = Buffer.alloc(0);
  private closing = false;
  private pending?: Buffer;
  private parsing = false;
  async start() {
    const child = (this.child = spawn(
      process.env.KVMHELM_FFMPEG ||
        (ffmpeg && existsSync(ffmpeg) ? ffmpeg : "ffmpeg"),
      [
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-protocol_whitelist",
        "pipe",
        "-probesize",
        "32",
        "-analyzeduration",
        "0",
        "-flags",
        "low_delay",
        "-threads",
        "2",
        "-thread_type",
        "slice",
        "-f",
        "h264",
        "-i",
        "pipe:0",
        "-an",
        "-c:v",
        "mjpeg",
        "-threads",
        "2",
        "-q:v",
        "3",
        "-fps_mode",
        "passthrough",
        "-f",
        "image2pipe",
        "-flush_packets",
        "1",
        "pipe:1",
      ],
      { windowsHide: true, shell: false, stdio: "pipe" },
    ));
    child.stdin.on("error", () => {
      if (!this.closing)
        this.emit(
          "fault",
          new GatewayError("DEVICE_OFFLINE", "Video decoder pipe closed"),
        );
    });
    child.stderr.resume(); // No target-controlled output or frames in logs.
    child.on("exit", () => {
      if (!this.closing)
        this.emit(
          "fault",
          new GatewayError("DEVICE_OFFLINE", "FFmpeg video decoder stopped"),
        );
    });
    child.stdout.on("data", (chunk: Buffer) => {
      this.buffer = Buffer.concat([this.buffer, chunk]);
      if (this.buffer.length > 12 * 1024 * 1024) {
        this.buffer = Buffer.alloc(0);
        this.emit(
          "fault",
          new GatewayError("RESOURCE_LIMIT", "Video decoder output limit"),
        );
        return;
      }
      for (;;) {
        const start = this.buffer.indexOf(Buffer.from([255, 216]));
        if (start < 0) {
          this.buffer = this.buffer.subarray(-1);
          break;
        }
        const end = this.buffer.indexOf(Buffer.from([255, 217]), start + 2);
        if (end < 0) break;
        this.pending = Buffer.from(this.buffer.subarray(start, end + 2));
        this.buffer = this.buffer.subarray(end + 2);
        void this.parseLatest();
      }
    });
    await new Promise<void>((resolve, reject) => {
      child.once("spawn", resolve);
      child.once("error", () =>
        reject(
          new GatewayError(
            "DEPENDENCY_MISSING",
            "JetKVM requires FFmpeg with H.264 decoding and MJPEG encoding. Install ffmpeg or set KVMHELM_FFMPEG.",
          ),
        ),
      );
    });
  }
  private async parseLatest() {
    if (this.parsing) return;
    this.parsing = true;
    try {
      while (this.pending && !this.closing) {
        const data = this.pending;
        this.pending = undefined;
        const m = await sharp(data, { limitInputPixels: 16777216 }).metadata();
        if (!m.width || !m.height || data.length > 8 * 1024 * 1024)
          throw new GatewayError("RESOURCE_LIMIT");
        this.emit("frame", {
          data,
          width: m.width,
          height: m.height,
          mime_type: "image/jpeg",
          signal: "present",
        } satisfies DriverFrame);
      }
    } catch (e) {
      if (!this.closing) this.emit("fault", e);
    } finally {
      this.parsing = false;
    }
  }
  write(data: Buffer): boolean {
    if (
      !this.child?.stdin.writable ||
      this.child.stdin.writableLength > 1024 * 1024
    )
      return false;
    this.child.stdin.write(Buffer.concat([data, aud]));
    return true;
  }
  async close() {
    this.closing = true;
    this.pending = undefined;
    this.buffer = Buffer.alloc(0);
    const child = this.child;
    this.child = undefined;
    if (
      !child ||
      !child.pid ||
      child.exitCode !== null ||
      child.signalCode !== null
    )
      return;
    await new Promise<void>((resolve) => {
      const timer = setTimeout(() => {
        child.kill("SIGKILL");
      }, 1000);
      timer.unref();
      child.once("close", () => {
        clearTimeout(timer);
        resolve();
      });
      child.stdin.destroy();
      child.kill();
    });
  }
}
