import { PiKvmDriver } from "./pikvm.js";
import { GatewayError, type Device } from "../../shared/contracts.js";
/** GL.iNet's KVMD fork exposes the same HID events; keep its identity explicit. */
export class CometDriver extends PiKvmDriver {
  constructor(
    device: Device,
    secret: { username?: string; password?: string },
    ca?: string,
  ) {
    super(device, { ...secret, username: secret.username || "admin" }, ca);
    this.caps.extensions = {
      vendor: "GL.iNet",
      model: device.model,
      protocol: "glkvm-kvmd",
    };
  }
  override async connect(signal: AbortSignal) {
    try {
      await super.connect(signal);
    } catch (error) {
      await this.disconnect().catch(() => {});
      if (
        error instanceof GatewayError &&
        error.code === "DEVICE_OFFLINE" &&
        /HTTP (404|405)/.test(error.message)
      )
        throw new GatewayError(
          "UNSUPPORTED_FIRMWARE",
          "This Comet firmware does not expose the required local KVMD API.",
        );
      throw error;
    }
  }
}
