import type { Request } from "express";
import rateLimit from "express-rate-limit";

export function safetyRequest(q: Request) {
  if (q.method !== "POST") return false;
  if (
    /^\/api\/v1\/devices\/[^/]+\/stop$/.test(q.path) ||
    /^\/api\/v1\/sessions\/[^/]+\/release-inputs$/.test(q.path)
  )
    return true;
  const name = q.path.startsWith("/api/v1/tools/")
    ? q.path.slice(14)
    : q.path === "/mcp" && q.body?.method === "tools/call"
      ? q.body.params?.name
      : undefined;
  const args = q.path === "/mcp" ? q.body?.params?.arguments : q.body;
  return (
    name === "close_computer" ||
    (name === "computer_control" && args?.operation === "release")
  );
}
export function requestLimits(limits = { media: 12000, control: 2400 }) {
  const media = (q: Request) =>
    q.method === "GET" && /\/sessions\/[^/]+\/(frame|video)$/.test(q.path);
  const create = (limit: number, skip: (q: Request) => boolean) =>
    rateLimit({
      windowMs: 60000,
      limit,
      standardHeaders: true,
      legacyHeaders: false,
      keyGenerator: (_q, r) => r.locals.identity.token.id,
      skip: (q, r) => !r.locals.identity || skip(q),
      message: {
        ok: false,
        error: {
          code: "RATE_LIMIT",
          message: "Request budget exceeded; retry after the indicated delay",
        },
      },
    });
  return [
    create(limits.media, (q) => !media(q)),
    create(limits.control, (q) => media(q) || safetyRequest(q)),
  ];
}
