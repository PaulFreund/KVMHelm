import { test } from "node:test";
import assert from "node:assert/strict";
import { once } from "node:events";
import { setTimeout as delay } from "node:timers/promises";
import { WebSocketServer } from "ws";
// Exercise the built worker, which runs in a separate Node process context.
// Like the browser integration tests, this requires npm run build first.
// @ts-ignore compiled server module
import { PiAudio } from "../dist/server/audio.js";

test(
  "PiKVM concurrent audio probes await the same Janus feature response",
  { timeout: 15000 },
  async () => {
    const server = new WebSocketServer({ host: "127.0.0.1", port: 0 });
    await once(server, "listening");
    let requested!: () => void, answer!: () => void;
    const request = new Promise<void>((resolve) => {
      requested = resolve;
    });
    let connections = 0;
    server.on("connection", (socket) => {
      connections++;
      socket.on("message", (raw) => {
        const m = JSON.parse(raw.toString());
        if (m.janus === "create" || m.janus === "attach")
          socket.send(
            JSON.stringify({
              janus: "success",
              transaction: m.transaction,
              data: { id: 1 },
            }),
          );
        if (m.body?.request === "features") {
          answer = () =>
            socket.send(
              JSON.stringify({
                janus: "event",
                plugindata: {
                  data: {
                    result: {
                      status: "features",
                      features: { audio: true, mic: false },
                    },
                  },
                },
              }),
            );
          requested();
        }
      });
    });
    const address = server.address() as { port: number };
    const audio = new PiAudio({
      device_id: "fixture",
      address: `http://127.0.0.1:${address.port}`,
      headers: {},
    });
    try {
      const first = audio.probe();
      await request;
      let resolved = false;
      const second = audio.probe().then((caps: any) => {
        resolved = true;
        return caps;
      });
      await delay(50);
      assert.equal(
        resolved,
        false,
        "a subscriber must not see provisional unsupported audio",
      );
      answer();
      const [a, b] = await Promise.all([first, second]);
      assert.equal(a.from_target, true);
      assert.deepEqual(a, b);
      assert.equal(connections, 1);
    } finally {
      await audio.close();
      for (const socket of server.clients) socket.terminate();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    }
  },
);
