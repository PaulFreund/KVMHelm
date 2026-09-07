import {
  createHash,
  randomBytes,
  randomUUID,
  timingSafeEqual,
} from "node:crypto";
import { z } from "zod";
import { scopes, GatewayError } from "../shared/contracts.js";
import type { Store } from "./store.js";
export const tokenSchema = z
  .object({
    name: z.string().min(1).max(100),
    scopes: z.array(z.enum(scopes)).min(1),
    devices: z.array(z.string()).min(1),
    expires_at: z.number().nullable().default(null),
  })
  .strict();
export interface Token {
  id: string;
  name: string;
  hash: string;
  scopes: string[];
  devices: string[];
  expires_at: number | null;
  created_at: number;
  last_used: number | null;
  revoked: boolean;
  revision: number;
}
export interface Identity {
  token: Token;
  client_id: string;
  ui_id?: string;
}
const digest = (s: string) => createHash("sha256").update(s).digest();
export class Auth {
  tokens = new Map<string, Token>();
  uis = new Map<
    string,
    { token_id: string; csrf: string; expires_at: number }
  >();
  onRevoke: (id: string) => Promise<void> = async () => {};
  constructor(private store: Store) {}
  async load() {
    for (const t of await this.store.list("tokens")) this.tokens.set(t.id, t);
  }
  async create(input: unknown, issuer?: Identity) {
    const v = tokenSchema.parse(input);
    if (issuer) {
      this.check(issuer, "tokens:manage");
      if (
        v.scopes.some((s) => !issuer.token.scopes.includes(s)) ||
        v.devices.some(
          (d) =>
            !issuer.token.devices.includes("*") &&
            !issuer.token.devices.includes(d),
        )
      )
        throw new GatewayError(
          "FORBIDDEN",
          "Cannot grant permissions beyond the issuing token",
        );
      if (
        issuer.token.expires_at &&
        (!v.expires_at || v.expires_at > issuer.token.expires_at)
      )
        throw new GatewayError("FORBIDDEN", "Cannot extend token lifetime");
    }
    const id = randomUUID(),
      secret = "kh_" + id + "_" + randomBytes(32).toString("base64url");
    const t = await this.store.put("tokens", id, {
      ...v,
      id,
      hash: digest(secret).toString("hex"),
      created_at: Date.now(),
      last_used: null,
      revoked: false,
    });
    this.tokens.set(id, t);
    return { token: secret, ...this.public(t) };
  }
  public(t: Token) {
    const { hash, ...rest } = t;
    return rest;
  }
  list() {
    return [...this.tokens.values()].map((t) => this.public(t));
  }
  authenticate(secret: string, client_id: string): Identity {
    const id = secret.startsWith("kh_") ? secret.slice(3, 39) : "";
    const t = this.tokens.get(id);
    if (!t || !timingSafeEqual(digest(secret), Buffer.from(t.hash, "hex")))
      throw new GatewayError("AUTH_FAILED");
    const identity = { token: t, client_id };
    this.check(identity);
    t.last_used = Date.now();
    return identity;
  }
  check(i: Identity, scope?: string, device?: string) {
    if (i.ui_id) {
      const ui = this.uis.get(i.ui_id);
      if (!ui || ui.expires_at <= Date.now())
        throw new GatewayError("AUTH_FAILED");
    }
    const t = this.tokens.get(i.token.id);
    if (
      !t ||
      t.revoked ||
      (t.expires_at !== null && t.expires_at <= Date.now())
    )
      throw new GatewayError("AUTH_FAILED");
    if (scope && !t.scopes.includes(scope)) throw new GatewayError("FORBIDDEN");
    if (device && !t.devices.includes("*") && !t.devices.includes(device))
      throw new GatewayError("FORBIDDEN");
    i.token = t;
  }
  async revoke(id: string) {
    const t = this.tokens.get(id);
    if (!t) throw new GatewayError("NOT_FOUND");
    const next = await this.store.put(
      "tokens",
      id,
      { ...t, revoked: true },
      t.revision,
    );
    this.tokens.set(id, next);
    for (const [key, s] of this.uis)
      if (s.token_id === id) this.uis.delete(key);
    await this.onRevoke(id);
  }
  login(pat: string) {
    for (const [key, s] of this.uis)
      if (s.expires_at <= Date.now()) this.uis.delete(key);
    const id = randomBytes(32).toString("base64url"),
      csrf = randomBytes(32).toString("base64url");
    const i = this.authenticate(pat, id);
    if (this.uis.size >= 1024) throw new GatewayError("RESOURCE_LIMIT");
    this.uis.set(id, {
      token_id: i.token.id,
      csrf,
      expires_at: Date.now() + 8 * 3600000,
    });
    return { id, csrf };
  }
  cookie(id: string, client: string) {
    const s = this.uis.get(id);
    if (!s || s.expires_at < Date.now()) throw new GatewayError("AUTH_FAILED");
    const t = this.tokens.get(s.token_id);
    if (!t) throw new GatewayError("AUTH_FAILED");
    const i = {
      token: t,
      client_id: digest(`${id}:${client}`).toString("hex"),
      ui_id: id,
    };
    this.check(i);
    return { identity: i, csrf: s.csrf };
  }
}
