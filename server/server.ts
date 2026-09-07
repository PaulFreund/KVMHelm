import express from "express";
import cookieParser from "cookie-parser";
import rateLimit from "express-rate-limit";
import { createServer as httpServer } from "node:http";
import { createServer as httpsServer } from "node:https";
import { readFile } from "node:fs/promises";
import { randomUUID, createHash } from "node:crypto";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { existsSync } from "node:fs";
import { z } from "zod";
import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js";
import { isInitializeRequest } from "@modelcontextprotocol/sdk/types.js";
import { type Identity } from "./auth.js";
import { Core } from "./core.js";
import { mcpServer, pack } from "./mcp.js";
import {
  GatewayError,
  toolSchemas,
  type ToolName,
} from "../shared/contracts.js";
import type { PluginHost } from "./plugins.js";
import { audioServer } from "./audio.js";
import { connect as tlsConnect } from "node:tls";
import { isIP } from "node:net";
import { localLookup, isLocalAddress } from "./network.js";
import { requestLimits } from "./request-limits.js";
export interface ServerOptions {
  host: string;
  port: number;
  cert?: string;
  key?: string;
  origins: string[];
  dev?: boolean;
  limits?: { media: number; control: number };
}
export async function serve(
  core: Core,
  plugins: PluginHost,
  options: ServerOptions,
) {
  const loopback = ["127.0.0.1", "::1", "localhost"].includes(options.host);
  if (!loopback && (!options.cert || !options.key))
    throw Error("LAN access requires explicit TLS certificate and key");
  const secure = !!options.cert;
  const origins = new Set([
    `${secure ? "https" : "http"}://${options.host}:${options.port}`,
    ...options.origins,
  ]);
  if (loopback) {
    origins.add(`${secure ? "https" : "http"}://localhost:${options.port}`);
    if (options.dev) origins.add("http://127.0.0.1:5173");
  }
  const app = express();
  app.disable("x-powered-by");
  app.use((req, res, next) => {
    res.setHeader("X-Content-Type-Options", "nosniff");
    res.setHeader("Referrer-Policy", "no-referrer");
    res.setHeader("Cache-Control", "no-store");
    res.setHeader(
      "Content-Security-Policy",
      "default-src 'self'; img-src 'self' blob: data:; media-src 'self' blob:; style-src 'self' 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'",
    );
    if (req.headers.origin && !origins.has(req.headers.origin)) {
      res
        .status(403)
        .json({ error: { code: "FORBIDDEN", message: "Origin rejected" } });
      return;
    }
    const host = req.headers.host;
    if (!host || ![...origins].some((o) => new URL(o).host === host)) {
      res
        .status(403)
        .json({ error: { code: "FORBIDDEN", message: "Host rejected" } });
      return;
    }
    next();
  });
  app.use(express.json({ limit: "256kb" }));
  app.use(cookieParser());
  const loginLimit = rateLimit({
    windowMs: 60000,
    limit: 10,
    standardHeaders: true,
    legacyHeaders: false,
  });
  const failedAuthLimit = rateLimit({
    windowMs: 60000,
    limit: 120,
    standardHeaders: true,
    legacyHeaders: false,
    message: {
      ok: false,
      error: {
        code: "RATE_LIMIT",
        message: "Too many failed authentication attempts",
      },
    },
  });
  app.get("/healthz", (_q, r) => r.json({ ok: true, service: "KVMHelm" }));
  app.get("/readyz", async (_q, r) => {
    try {
      await core.store.health();
      r.json({ ok: true });
    } catch {
      r.status(503).json({ ok: false, error: { code: "STORAGE_ERROR" } });
    }
  });
  app.post("/api/v1/login", loginLimit, (req, res) => {
    if (!req.headers.origin || !origins.has(req.headers.origin))
      throw new GatewayError("FORBIDDEN");
    const { pat } = z
      .object({ pat: z.string().max(256) })
      .strict()
      .parse(req.body);
    const s = core.auth.login(pat);
    res
      .cookie("kvmhelm", s.id, {
        httpOnly: true,
        secure,
        sameSite: "strict",
        maxAge: 8 * 3600000,
        path: "/",
      })
      .json({ csrf: s.csrf });
  });
  app.use(["/api", "/mcp"], (req, res, next) => {
    try {
      let identity: Identity;
      const bearer = req.headers.authorization?.match(/^Bearer (.+)$/)?.[1];
      const client = String(req.headers["x-kvmhelm-client"] ?? "default");
      if (!/^[a-zA-Z0-9_-]{1,100}$/.test(client))
        throw new GatewayError("FORBIDDEN");
      if (bearer)
        identity = core.auth.authenticate(
          bearer,
          createHash("sha256").update(`${bearer}:${client}`).digest("hex"),
        );
      else {
        const session = core.auth.cookie(req.cookies.kvmhelm, client);
        identity = session.identity;
        res.locals.csrf = session.csrf;
        if (
          !["GET", "HEAD", "OPTIONS"].includes(req.method) &&
          (req.headers["x-csrf-token"] !== session.csrf ||
            !req.headers.origin ||
            !origins.has(req.headers.origin))
        )
          throw new GatewayError("FORBIDDEN", "CSRF validation failed");
      }
      res.locals.identity = identity;
      next();
    } catch (e) {
      failedAuthLimit(req, res, () => next(e));
    }
  });
  const who = (r: express.Response) => r.locals.identity as Identity;
  app.use(...requestLimits(options.limits));
  let audio: ReturnType<typeof audioServer>;
  app.post("/api/v1/sessions/:id/audio", (q, r) => {
    const { microphone } = z
      .object({ microphone: z.boolean() })
      .strict()
      .parse(q.body);
    const i = who(r),
      s = core.session(i, String(q.params.id));
    r.json({ ticket: audio.ticket(i, s, microphone) });
  });
  app.post("/api/v1/sessions/:id/release-inputs", async (q, r) => {
    const i = who(r),
      s = core.session(i, String(q.params.id)),
      d = core.get(s.device_id);
    core.assertControl(i, s, d);
    d.abort?.abort();
    await d.tail;
    await d.driver.releaseAllInputs();
    r.json({ ok: true });
  });
  app.get("/api/v1/me", (_q, r) =>
    r.json({
      owner_id: "owner",
      csrf: r.locals.csrf,
      token: core.auth.public(who(r).token),
    }),
  );
  app.post("/api/v1/logout", async (_q, r) => {
    const i = who(r);
    if (i.ui_id) core.auth.uis.delete(i.ui_id);
    await core.disconnectClient(i.client_id);
    r.clearCookie("kvmhelm").json({ ok: true });
  });
  app.get("/api/v1/devices", (_q, r) => r.json(core.list(who(r))));
  app.post("/api/v1/certificates/inspect", async (q, r) => {
    core.auth.check(who(r), "devices:manage");
    const { address } = z
      .object({ address: z.string().url() })
      .strict()
      .parse(q.body);
    const u = new URL(address);
    const host = u.hostname.replace(/^\[|\]$/g, "");
    if (
      u.protocol !== "https:" ||
      u.username ||
      u.password ||
      u.pathname !== "/" ||
      u.search ||
      u.hash ||
      (isIP(host) && !isLocalAddress(host))
    )
      throw new GatewayError("INVALID_ADDRESS", "Use a LAN HTTPS origin");
    const certificate = await new Promise((resolve, reject) => {
      // Inspect only: no password or HTTP request is sent before trust is selected.
      const socket = tlsConnect(
        {
          host,
          port: Number(u.port || 443),
          lookup: localLookup,
          rejectUnauthorized: false,
        },
        () => {
          const cert = socket.getPeerCertificate();
          socket.destroy();
          if (!cert.raw)
            return reject(new GatewayError("TLS_CERTIFICATE_ERROR"));
          resolve({
            fingerprint: createHash("sha256").update(cert.raw).digest("hex"),
            subject: cert.subject?.CN ?? "",
            valid_to: cert.valid_to,
          });
        },
      );
      socket.setTimeout(5000, () =>
        socket.destroy(Error("Certificate inspection timeout")),
      );
      socket.once("error", reject);
    });
    r.json(certificate);
  });
  app.post("/api/v1/devices", async (q, r) => {
    const { device, secret, ca } = z
      .object({
        device: z.unknown(),
        secret: z
          .object({
            username: z.string().max(100),
            password: z.string().max(1000),
          })
          .strict()
          .nullable()
          .optional(),
        ca: z.string().max(65536).nullable().optional(),
      })
      .strict()
      .parse(q.body);
    r.json(await core.saveDevice(who(r), device, secret, ca));
  });
  app.delete("/api/v1/devices/:id", async (q, r) => {
    const b = z
      .object({
        revision: z.number().int(),
        confirmed: z.boolean().default(false),
      })
      .strict()
      .parse(q.body);
    await core.deleteDevice(
      who(r),
      String(q.params.id),
      b.revision,
      b.confirmed,
    );
    r.json({ ok: true });
  });
  app.post("/api/v1/devices/:id/test", async (q, r) => {
    const id = String(q.params.id);
    core.auth.check(who(r), "devices:manage", id);
    const d = core.get(id);
    await core.connect(d);
    await d.media.screenshot(0, 5000);
    r.json(core.describe(d));
  });
  app.post("/api/v1/devices/:id/benchmark", async (q, r) => {
    const id = String(q.params.id);
    core.auth.check(who(r), "devices:manage", id);
    const d = core.get(id);
    await core.connect(d);
    r.json(await d.media.benchmark());
  });
  app.post("/api/v1/devices/:id/stop", async (q, r) =>
    r.json(await core.stopControl(who(r), String(q.params.id))),
  );
  app.post("/api/v1/devices/:id/resume", async (q, r) =>
    r.json(await core.stopControl(who(r), String(q.params.id), true)),
  );
  app.get("/api/v1/tokens", (_q, r) => {
    core.auth.check(who(r), "tokens:manage");
    r.json(core.auth.list());
  });
  app.post("/api/v1/tokens", async (q, r) =>
    r.json(await core.auth.create(q.body, who(r))),
  );
  app.delete("/api/v1/tokens/:id", async (q, r) => {
    core.auth.check(who(r), "tokens:manage");
    await core.auth.revoke(String(q.params.id));
    r.json({ ok: true });
  });
  app.get("/api/v1/preferences", async (_q, r) => {
    core.auth.check(who(r), "devices:read");
    r.json(
      (await core.store.get("preferences", "owner")) ?? {
        revision: 0,
        sidebar: true,
        theme: "dark",
        selected: "",
        order: [],
        visible: [],
        tile_size: 400,
      },
    );
  });
  app.put("/api/v1/preferences", async (q, r) => {
    core.auth.check(who(r), "devices:read");
    const b = z
      .object({
        revision: z.number().int(),
        sidebar: z.boolean(),
        theme: z.enum(["light", "dark"]),
        selected: z.string(),
        order: z.array(z.string()).max(1000),
        visible: z.array(z.string()).max(1000),
        tile_size: z.number().int().min(240).max(1000),
      })
      .strict()
      .parse(q.body);
    r.json(await core.store.put("preferences", "owner", b, b.revision));
  });
  app.get("/api/v1/sessions", (_q, r) =>
    r.json(
      [...core.sessions.values()]
        .filter((s) => s.client_id === who(r).client_id)
        .map((s) => core.publicSession(s)),
    ),
  );
  app.post("/api/v1/tools/:name", async (q, r) => {
    const name = String(q.params.name) as ToolName;
    if (!Object.hasOwn(toolSchemas, name)) throw new GatewayError("NOT_FOUND");
    r.json(pack(await core.tool(who(r), name, q.body)));
  });
  app.get("/api/v1/sessions/:id/frame", async (q, r) => {
    const s = core.session(who(r), String(q.params.id));
    const f = await core.observe(who(r), s, 100, 3000);
    r.setHeader("X-KVMHelm-Frame", JSON.stringify(f.info));
    r.type(f.info.mime_type).send(f.data);
  });
  app.get("/api/v1/sessions/:id/video", async (q, r) => {
    const i = who(r),
      s = core.session(i, String(q.params.id));
    core.auth.check(i, "video:read", s.device_id);
    const d = core.get(s.device_id),
      consumer = randomUUID();
    d.media.retain(consumer, "overview");
    r.setHeader("Content-Type", "multipart/x-mixed-replace; boundary=kvmhelm");
    let blocked = false;
    const send = (f: any) => {
      try {
        core.session(i, s.session_id);
        core.auth.check(i, "video:read", s.device_id);
        if (blocked) {
          d.media.metrics.dropped++;
          return;
        }
        blocked = !r.write(
          Buffer.concat([
            Buffer.from(
              `--kvmhelm\r\nContent-Type: ${f.info.mime_type}\r\nContent-Length: ${f.data.length}\r\n\r\n`,
            ),
            f.data,
            Buffer.from("\r\n"),
          ]),
        );
      } catch {
        r.end();
      }
    };
    r.on("drain", () => (blocked = false));
    d.media.on("frame", send);
    const timer = setInterval(() => {
      try {
        core.session(i, s.session_id);
        core.auth.check(i, "video:read", s.device_id);
      } catch {
        r.end();
      }
    }, 1000);
    q.on("close", () => {
      clearInterval(timer);
      d.media.off("frame", send);
      d.media.release(consumer);
    });
    if (d.media.latest) send(d.media.latest);
  });
  app.get("/api/v1/events", (q, r) => {
    const i = who(r);
    core.auth.check(i, "devices:read");
    r.setHeader("Content-Type", "text/event-stream");
    r.setHeader("X-KVMHelm-Instance", core.instanceId);
    r.setHeader("Connection", "keep-alive");
    r.flushHeaders();
    const send = (event: any) => {
      try {
        core.auth.check(i, "devices:read", event.device_id);
        if (r.writableLength > 256 * 1024) {
          r.end();
          return;
        }
        r.write(`id: ${event.event_id}\ndata: ${JSON.stringify(event)}\n\n`);
      } catch {
        if (!event.device_id) r.end();
      }
    };
    const cursor =
      q.headers["x-kvmhelm-instance"] === core.instanceId
        ? Number(q.headers["last-event-id"] ?? 0)
        : 0;
    for (const e of core.events)
      if (e.event_id > cursor) send({ ...e, historical: true });
    core.on("event", send);
    const timer = setInterval(() => {
      try {
        core.auth.check(i);
        r.write(": heartbeat\n\n");
      } catch {
        r.end();
      }
    }, 1000);
    q.on("close", () => {
      clearInterval(timer);
      core.off("event", send);
    });
  });
  app.get("/api/v1/plugins", (_q, r) => {
    core.auth.check(who(r), "plugins:manage");
    r.json(plugins.list());
  });
  app.get("/api/v1/ui-extensions", (_q, r) => {
    const i = who(r);
    core.auth.check(i, "devices:read");
    r.json(
      plugins.list().map((p) => ({
        id: p.id,
        status: p.status,
        panels: p.panels.filter(
          (panel: any) =>
            !panel.device_id ||
            i.token.devices.includes("*") ||
            i.token.devices.includes(panel.device_id),
        ),
      })),
    );
  });
  app.post("/api/v1/plugins", async (q, r) => {
    core.auth.check(who(r), "plugins:manage");
    r.json(
      await plugins.install(
        z
          .object({ path: z.string(), devices: z.array(z.string()) })
          .strict()
          .parse(q.body),
      ),
    );
  });
  app.post("/api/v1/plugins/:id/:op", async (q, r) => {
    core.auth.check(who(r), "plugins:manage");
    r.json(await plugins.operation(String(q.params.id), String(q.params.op)));
  });
  app.get("/api/v1/diagnostics", (_q, r) => {
    core.auth.check(who(r), "devices:manage");
    r.json({
      runtime: process.version,
      platform: process.platform,
      memory: process.memoryUsage(),
      metrics: core.metrics,
      health: core.health(),
      devices: core.list(who(r)),
      plugins: plugins.list(),
    });
  });
  const transports = new Map<
    string,
    {
      transport: StreamableHTTPServerTransport;
      token: string;
      identity: Identity;
      server: ReturnType<typeof mcpServer>;
      last: number;
    }
  >();
  app.all("/mcp", async (q, r) => {
    const i = who(r),
      id = q.headers["mcp-session-id"];
    if (id) {
      const entry = transports.get(String(id));
      if (!entry) {
        r.status(404).json({ error: "Unknown MCP session" });
        return;
      }
      if (entry.token !== i.token.id) throw new GatewayError("FORBIDDEN");
      core.auth.check(entry.identity);
      entry.last = Date.now();
      await entry.transport.handleRequest(q, r, q.body);
      return;
    }
    if (q.method !== "POST" || !isInitializeRequest(q.body)) {
      r.status(400).json({ error: "Initialize MCP first" });
      return;
    }
    if (transports.size >= 128) throw new GatewayError("RESOURCE_LIMIT");
    const identity = { ...i, client_id: `mcp:${randomUUID()}` };
    const server = mcpServer(core, identity);
    const transport = new StreamableHTTPServerTransport({
      sessionIdGenerator: randomUUID,
      onsessioninitialized: (sessionId) => {
        transports.set(sessionId, {
          transport,
          token: i.token.id,
          identity,
          server,
          last: Date.now(),
        });
      },
    });
    transport.onclose = () => {
      if (transport.sessionId) transports.delete(transport.sessionId);
      void core.disconnectClient(identity.client_id);
    };
    await server.connect(transport);
    await transport.handleRequest(q, r, q.body);
  });
  const cleanup = setInterval(() => {
    for (const [id, e] of transports) {
      try {
        core.auth.check(e.identity);
        if (Date.now() - e.last > 3600000) throw Error();
      } catch {
        void e.server.close();
        transports.delete(id);
        void core.disconnectClient(e.identity.client_id);
      }
    }
  }, 1000);
  const webRoot = fileURLToPath(
    new URL(
      existsSync(new URL("../web/assets", import.meta.url))
        ? "../web/"
        : "../dist/web/",
      import.meta.url,
    ),
  );
  app.use("/api", (_q, r) =>
    r.status(404).json({ ok: false, error: { code: "NOT_FOUND" } }),
  );
  app.use(express.static(webRoot, { index: "index.html" }));
  app.get("/{*path}", (_q, r) => r.sendFile(resolve(webRoot, "index.html")));
  app.use(
    (
      e: any,
      _q: express.Request,
      r: express.Response,
      _next: express.NextFunction,
    ) => {
      if (r.headersSent) {
        r.end();
        return;
      }
      const code =
        e instanceof GatewayError
          ? e.code
          : e?.name === "ZodError"
            ? "INVALID_ARGUMENT"
            : "INTERNAL_ERROR";
      core.recordError(code);
      r.status(
        code === "AUTH_FAILED"
          ? 401
          : code === "FORBIDDEN"
            ? 403
            : code === "NOT_FOUND"
              ? 404
              : code === "SESSION_LOST"
                ? 410
                : code === "STORAGE_ERROR"
                  ? 503
                  : code === "INTERNAL_ERROR"
                    ? 500
                    : code === "REVISION_CONFLICT"
                      ? 409
                      : 400,
      ).json({
        ok: false,
        error: { code, message: e instanceof GatewayError ? e.message : code },
      });
    },
  );
  const server = secure
    ? httpsServer(
        {
          cert: await readFile(options.cert!),
          key: await readFile(options.key!),
        },
        app,
      )
    : httpServer(app);
  audio = audioServer(server, core, origins);
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(options.port, options.host, resolve);
  });
  if (options.port === 0) {
    const address = server.address();
    if (address && typeof address !== "string") {
      origins.add(
        `${secure ? "https" : "http"}://${options.host}:${address.port}`,
      );
      if (loopback)
        origins.add(`${secure ? "https" : "http"}://localhost:${address.port}`);
    }
  }
  return {
    server,
    app,
    close: async () => {
      clearInterval(cleanup);
      audio.close();
      for (const e of transports.values()) await e.server.close();
      server.closeAllConnections();
      await new Promise<void>((r) => server.close(() => r()));
    },
  };
}
