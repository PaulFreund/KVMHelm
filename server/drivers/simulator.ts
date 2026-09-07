import sharp from "sharp";
import { PiKvmDriver } from "./pikvm.js";
import {
  type Device,
  type Capabilities,
  type DriverFrame,
  type ComputerAction,
  type ActionContext,
} from "../../shared/contracts.js";
export class Simulator extends PiKvmDriver {
  count = 0;
  private marker = Math.random().toString(36).slice(2, 10);
  constructor(d: Device) {
    super(d, {});
  }
  override async connect() {}
  override async disconnect() {}
  override async capabilities(): Promise<Capabilities> {
    return {
      video: {
        snapshot: true,
        stream: false,
        formats: ["image/png"],
        signal: "present",
      },
      mouse: {
        absolute: true,
        relative: true,
        buttons: ["left", "right", "middle"],
        horizontal_scroll: true,
      },
      keyboard: {
        hid: true,
        layouts: ["us", "de"],
        text_method: "simulated",
        characters: "US / DE",
      },
      audio: {
        from_target: false,
        to_target: false,
        formats: [],
        reason: "Simulator has no audio",
      },
      timebase: { source_timestamp: true, uncertainty_ms: 0 },
      extensions: { simulated: true },
    };
  }
  override validate(actions: ComputerAction[], w: number, h: number) {
    for (const a of actions) {
      if ("x" in a && (a.x >= w || a.y >= h)) throw Error("VIEW_CHANGED");
    }
  }
  override async snapshot(): Promise<DriverFrame> {
    const data = await sharp(
      Buffer.from(
        `<svg width="1280" height="720" xmlns="http://www.w3.org/2000/svg"><rect width="1280" height="720" fill="#101b2a"/><rect x="48" y="48" width="1184" height="624" rx="20" fill="#18283d"/><text x="90" y="140" fill="#55dfb0" font-family="sans-serif" font-size="28">KVMHelm · SIMULATED TARGET</text><text x="90" y="240" fill="white" font-family="sans-serif" font-size="48">Visual code: ${this.marker}</text><text x="90" y="320" fill="#a5b8ce" font-family="sans-serif" font-size="24">Input actions received: ${this.count}</text><text x="90" y="600" fill="#a5b8ce" font-family="sans-serif" font-size="18">${new Date().toISOString()}</text></svg>`,
      ),
    )
      .png()
      .toBuffer();
    return {
      data,
      width: 1280,
      height: 720,
      mime_type: "image/png",
      source_timestamp: performance.now(),
      signal: "present",
    };
  }
  override async execute(
    _a: ComputerAction,
    c: ActionContext,
  ): Promise<"sent"> {
    c.assertControl();
    this.count++;
    return "sent";
  }
  override async releaseAllInputs() {}
}
