import { request, Agent } from "undici";
import { checkServerIdentity, connect as tlsConnect } from "node:tls";
import { createHash } from "node:crypto";
import { setTimeout as delay } from "node:timers/promises";
import {
  type KvmDriver,
  type Device,
  type Capabilities,
  type DriverFrame,
  type ComputerAction,
  type ActionContext,
  GatewayError,
} from "../../shared/contracts.js";
import { keyName, textKeys } from "./keyboard.js";
import { localLookup, isLocalAddress } from "../network.js";
import { isIP } from "node:net";
/** Shared LAN transport and HID planning; no vendor endpoints or connections. */
export abstract class NetworkHidDriver implements KvmDriver {
  onDisconnected?: () => void;
  protected closing = false;
  protected agent: Agent;
  protected headers: Record<string, string>;
  protected keys = new Set<string>();
  protected buttons = new Set<string>();
  protected caps: Capabilities = {
    video: {
      snapshot: true,
      stream: true,
      formats: ["image/jpeg"],
      signal: "unknown",
    },
    mouse: {
      absolute: false,
      relative: false,
      buttons: ["left", "middle", "right"],
      horizontal_scroll: true,
    },
    keyboard: {
      hid: false,
      layouts: ["us", "de"],
      text_method: "hid-chords/v1",
      characters: "US / DE mapping v1",
    },
    audio: {
      from_target: false,
      to_target: false,
      formats: [],
      reason: "Audio capability probe pending",
    },
    timebase: { source_timestamp: false, uncertainty_ms: null },
    extensions: {},
  };
  constructor(
    public device: Device,
    secret: { username?: string; password?: string },
    protected ca?: string,
  ) {
    this.headers = {
      "X-KVMD-User": secret.username ?? "admin",
      "X-KVMD-Passwd": secret.password ?? "",
    };
    this.agent = new Agent({
      connections: 4,
      pipelining: 1,
      connect: {
        ca,
        rejectUnauthorized: !device.tls.insecure,
        timeout: 5000,
        lookup: localLookup,
      },
    });
  }
  async tlsOptions() {
    if (!this.device.tls.fingerprint)
      return { ca: this.ca, rejectUnauthorized: !this.device.tls.insecure };
    const expected = this.device.tls.fingerprint
      .replaceAll(":", "")
      .toLowerCase();
    if (!/^[a-f0-9]{64}$/.test(expected))
      throw new GatewayError("AUTH_FAILED", "Invalid SHA-256 certificate pin");
    const url = new URL(this.device.address!);
    await new Promise<void>((resolve, reject) => {
      const s = tlsConnect(
        {
          host: url.hostname,
          port: Number(url.port || 443),
          rejectUnauthorized: false,
          lookup: localLookup,
        },
        () => {
          const cert = s.getPeerCertificate();
          const actual = cert.raw
            ? createHash("sha256").update(cert.raw).digest("hex")
            : "";
          s.destroy();
          actual === expected
            ? resolve()
            : reject(
                new GatewayError(
                  "AUTH_FAILED",
                  "Certificate fingerprint mismatch",
                ),
              );
        },
      );
      s.setTimeout(5000, () => s.destroy(Error("TLS timeout")));
      s.on("error", reject);
    });
    return { ca: this.ca, rejectUnauthorized: false, checkServerIdentity };
  }
  async initPin() {
    if (!this.device.tls.fingerprint) return;
    const expected = this.device.tls.fingerprint
      .replaceAll(":", "")
      .toLowerCase();
    await this.tlsOptions();
    await this.agent.close();
    this.agent = new Agent({
      connections: 4,
      connect: (opts, cb) => {
        const s = tlsConnect(
          {
            ...opts,
            host: String(opts.hostname),
            port: Number(opts.port || 443),
            rejectUnauthorized: false,
            lookup: localLookup,
          },
          () => {
            const cert = s.getPeerCertificate();
            if (
              !cert.raw ||
              createHash("sha256").update(cert.raw).digest("hex") !== expected
            ) {
              s.destroy();
              cb(new Error("Certificate fingerprint mismatch"), null);
            } else cb(null, s);
          },
        );
        s.once("error", (e) => cb(e, null));
      },
    });
  }
  async http(
    path: string,
    signal: AbortSignal,
    method: "GET" | "POST" = "GET",
    body?: string,
  ) {
    const r = await request(new URL(path, this.device.address!), {
      dispatcher: this.agent,
      method,
      headers: this.headers,
      signal,
      body,
      headersTimeout: 5000,
      bodyTimeout: 5000,
    });
    if (r.statusCode >= 400) {
      await r.body.dump();
      throw new GatewayError(
        [401, 403].includes(r.statusCode) ? "AUTH_FAILED" : "DEVICE_OFFLINE",
        `${this.device.driver_id} HTTP ${r.statusCode}`,
      );
    }
    return r;
  }
  async json(path: string, signal = AbortSignal.timeout(5000)) {
    return ((await (await this.http(path, signal)).body.json()) as any).result;
  }
  protected async prepareTransport() {
    const hostname = new URL(this.device.address!).hostname.replace(
      /^\[|\]$/g,
      "",
    );
    if (isIP(hostname) && !isLocalAddress(hostname))
      throw new GatewayError(
        "INVALID_ADDRESS",
        "KVM target must be a LAN address",
      );
    if (this.closing)
      this.agent = new Agent({
        connections: 4,
        pipelining: 1,
        connect: {
          ca: this.ca,
          rejectUnauthorized: !this.device.tls.insecure,
          timeout: 5000,
          lookup: localLookup,
        },
      });
    this.closing = false;

    await this.initPin();
  }
  abstract connect(signal: AbortSignal): Promise<void>;
  abstract snapshot(signal: AbortSignal): Promise<DriverFrame>;
  abstract releaseAllInputs(): Promise<void>;
  protected abstract event(
    type: string,
    event: unknown,
    context?: ActionContext,
  ): Promise<void>;
  async capabilities() {
    return this.caps;
  }
  async disconnect() {
    this.closing = true;
    await this.agent.close();
  }
  validate(actions: ComputerAction[], width: number, height: number) {
    for (const a of actions) {
      if ("x" in a && (a.x >= width || a.y >= height))
        throw new GatewayError(
          "VIEW_CHANGED",
          "Coordinate outside reference image",
        );
      if (
        a.type === "drag" &&
        a.path.some((p) => p.x >= width || p.y >= height)
      )
        throw new GatewayError("VIEW_CHANGED");
      if (
        ["click", "double_click", "move", "drag", "scroll"].includes(a.type) &&
        !this.caps.mouse.absolute
      )
        throw new GatewayError(
          "UNSUPPORTED_ACTION",
          "Absolute mouse mode is unavailable",
        );
      if (a.type === "keypress") {
        if (!this.caps.keyboard.hid)
          throw new GatewayError("UNSUPPORTED_ACTION");
        a.keys.forEach(keyName);
      }
      if (a.type === "type") {
        if (!this.caps.keyboard.hid)
          throw new GatewayError("UNSUPPORTED_ACTION");
        textKeys(a.text, this.device.keyboard_layout);
      }
    }
  }
  private async chord(keys: string[], ctx: ActionContext) {
    try {
      for (const key of keys) {
        this.keys.add(key);
        await this.event("key", { key, state: true }, ctx);
      }
    } finally {
      for (const key of [...keys].reverse()) {
        await this.event("key", { key, state: false });
        this.keys.delete(key);
      }
    }
  }
  async execute(a: ComputerAction, c: ActionContext): Promise<"sent"> {
    const move = async (p: { x: number; y: number }) =>
      this.event(
        "mouse_move",
        {
          to: {
            x: Math.round((p.x / Math.max(1, c.width - 1)) * 65535 - 32768),
            y: Math.round((p.y / Math.max(1, c.height - 1)) * 65535 - 32768),
          },
        },
        c,
      );
    const button = async (b: string, state: boolean) => {
      if (state) this.buttons.add(b);
      await this.event(
        "mouse_button",
        { button: b, state },
        state ? c : undefined,
      );
      if (!state) this.buttons.delete(b);
    };
    if ("x" in a) await move(a);
    switch (a.type) {
      case "click":
      case "double_click":
        for (let i = 0; i < (a.type === "double_click" ? 2 : 1); i++) {
          try {
            await button(a.type === "click" ? a.button : "left", true);
          } finally {
            await button(a.type === "click" ? a.button : "left", false);
          }
          if (a.type === "double_click" && i === 0)
            await delay(60, undefined, { signal: c.signal });
        }
        break;
      case "drag":
        await move(a.path[0]);
        try {
          await button("left", true);
          for (const p of a.path.slice(1)) {
            await move(p);
            await delay(4, undefined, { signal: c.signal });
          }
        } finally {
          await button("left", false);
        }
        break;
      case "scroll": {
        let x = Math.round(a.scroll_x / 100),
          y = -Math.round(a.scroll_y / 100);
        while (x || y) {
          const dx = Math.max(-127, Math.min(127, x)),
            dy = Math.max(-127, Math.min(127, y));
          await this.event("mouse_wheel", { delta: { x: dx, y: dy } }, c);
          x -= dx;
          y -= dy;
        }
        break;
      }
      case "keypress":
        await this.chord(a.keys.map(keyName), c);
        break;
      case "type":
        for (const keys of textKeys(a.text, this.device.keyboard_layout))
          await this.chord(keys, c);
        break;
    }
    return "sent";
  }
}
