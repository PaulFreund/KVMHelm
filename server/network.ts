import { lookup, type LookupAllOptions } from "node:dns";
import { isIP } from "node:net";
import { Agent } from "node:https";
import { connect, type ConnectionOptions } from "node:tls";
import { createHash } from "node:crypto";
export function isLocalAddress(value: string): boolean {
  const ip = value.toLowerCase().replace(/^::ffff:/, "");
  if (isIP(ip) === 4) {
    const [a, b] = ip.split(".").map(Number);
    return (
      a === 10 ||
      a === 127 ||
      (a === 192 && b === 168) ||
      (a === 172 && b >= 16 && b <= 31) ||
      (a === 169 && b === 254)
    );
  }
  return (
    ip === "::1" ||
    /^f[cd][0-9a-f]{2}:/.test(ip) ||
    /^fe[89ab][0-9a-f]:/.test(ip)
  );
}
export const localLookup: typeof lookup = ((
  hostname: string,
  options: any,
  callback: any,
) => {
  if (typeof options === "function") {
    callback = options;
    options = {};
  }
  lookup(
    hostname,
    { ...options, all: true } as LookupAllOptions,
    (error, addresses) => {
      if (error) {
        callback(error);
        return;
      }
      if (
        !addresses.length ||
        addresses.some((a) => !isLocalAddress(a.address))
      ) {
        callback(
          Object.assign(
            new Error("KVM targets must resolve only to local/LAN addresses"),
            { code: "EACCES" },
          ),
        );
        return;
      }
      if (options?.all) callback(null, addresses);
      else callback(null, addresses[0].address, addresses[0].family);
    },
  );
}) as typeof lookup;
export function assertPin(raw: Buffer | undefined, pin: string) {
  if (
    !raw ||
    createHash("sha256").update(raw).digest("hex") !==
      pin.replaceAll(":", "").toLowerCase()
  )
    throw Error("Certificate fingerprint mismatch");
}
/** HTTP must not write credentials until the certificate pin is checked. */
export class PinnedAgent extends Agent {
  constructor(private pin: string) {
    super({ keepAlive: true, lookup: localLookup });
  }
  override createConnection(
    options: ConnectionOptions,
    callback: (error: Error | null, socket?: any) => void,
  ): any {
    let complete = false;
    const done = (e: Error | null, s?: any) => {
      if (complete) return;
      complete = true;
      callback(e, s);
    };
    const socket = connect(
      { ...options, lookup: localLookup, rejectUnauthorized: false },
      () => {
        try {
          assertPin(socket.getPeerCertificate().raw, this.pin);
          done(null, socket);
        } catch (e) {
          socket.destroy();
          done(e as Error);
        }
      },
    );
    socket.once("error", (e) => done(e));
    return undefined;
  }
}
