import { request } from "node:https";
import { readFileSync } from "node:fs";
const req = request(
  {
    host: "127.0.0.1",
    port: 8765,
    path: "/readyz",
    ca: readFileSync("/tls/server.crt"),
    servername: "localhost",
    headers: { Host: "localhost:8765" },
    timeout: 4000,
  },
  (r) => {
    r.resume();
    process.exitCode = r.statusCode === 200 ? 0 : 1;
  },
);
req.on("error", () => (process.exitCode = 1));
req.on("timeout", () => {
  req.destroy();
  process.exitCode = 1;
});
req.end();
