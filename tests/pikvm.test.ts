import { test } from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { WebSocketServer } from "ws";
import sharp from "sharp";
import { CometDriver } from "../server/drivers/comet.js";
import { PiKvmDriver } from "../server/drivers/pikvm.js";
import { deviceSchema } from "../shared/contracts.js";
import { randomUUID } from "node:crypto";
import { PiAudio } from "../server/audio.js";
for (const model of ["PiKVM v4", "GL-RM1 (Comet)", "GL-RM10 (Comet Pro)"])
  test(
    model +
      " protocol fixture: persistent HID, native pixel mapping, release and reconnect",
    async (t) => {
      let finishOldProbe: (() => void) | undefined;
      if (model === "PiKVM v4") {
        let probes = 0;
        t.mock.method(PiAudio.prototype, "probe", () => {
          if (++probes === 1)
            return new Promise((resolve) => {
              finishOldProbe = () =>
                resolve({
                  from_target: false,
                  to_target: false,
                  formats: [],
                  reason: "Audio unavailable",
                });
            });
          return Promise.resolve({
            from_target: true,
            to_target: false,
            formats: ["pcm_s16le/48000/2"],
            reason: "",
          });
        });
      }
      const image = await sharp({
        create: { width: 640, height: 480, channels: 3, background: "#234" },
      })
        .jpeg()
        .toBuffer();
      const events: any[] = [];
      const server = createServer((q, r) => {
        assert.equal(q.headers["x-kvmd-passwd"], "fixture-only");
        if (q.url === "/api/streamer/snapshot") {
          r.setHeader("Content-Type", "image/jpeg");
          r.end(image);
          return;
        }
        r.setHeader("Content-Type", "application/json");
        r.end(
          JSON.stringify({
            ok: true,
            result:
              q.url === "/api/hid"
                ? { keyboard: { online: true }, mouse: { absolute: true } }
                : {},
          }),
        );
      });
      const wss = new WebSocketServer({ noServer: true });
      server.on("upgrade", (q, s, h) =>
        wss.handleUpgrade(q, s, h, (ws) => {
          if (q.url === "/janus/ws") {
            ws.on("message", (raw) => {
              const m = JSON.parse(raw.toString());
              if (m.janus === "create" || m.janus === "attach")
                ws.send(
                  JSON.stringify({
                    janus: "success",
                    transaction: m.transaction,
                    data: { id: m.janus === "create" ? 1 : 2 },
                  }),
                );
              if (m.body?.request === "features")
                ws.send(
                  JSON.stringify({
                    janus: "event",
                    plugindata: {
                      data: {
                        result: {
                          status: "features",
                          features: { audio: false, mic: false },
                        },
                      },
                    },
                  }),
                );
            });
            return;
          }
          ws.on("message", (raw) => {
            const m = JSON.parse(raw.toString());
            events.push(m);
            if (m.event_type === "ping")
              ws.send(JSON.stringify({ event_type: "pong", event: {} }));
          });
        }),
      );
      await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
      const port = (server.address() as any).port;
      const device = {
        ...deviceSchema.parse({
          name: model + " protocol fixture",
          model,
          driver_id: model.startsWith("GL-") ? "glinet-comet" : "pikvm-v4",
          address: `http://127.0.0.1:${port}`,
          keyboard_layout: "de",
        }),
        device_id: randomUUID(),
      };
      const Driver = model.startsWith("GL-") ? CometDriver : PiKvmDriver;
      const driver = new Driver(device, {
        username: "admin",
        password: "fixture-only",
      });
      try {
        await driver.connect(AbortSignal.timeout(3000));
        const frame = await driver.snapshot(AbortSignal.timeout(3000));
        assert.equal(frame.width, 640);
        assert.equal(frame.mime_type, "image/jpeg");
        const context = {
          width: 640,
          height: 480,
          signal: AbortSignal.timeout(3000),
          assertControl() {},
        };
        driver.validate(
          [
            { type: "type", text: "Ä@" },
            { type: "click", x: 639, y: 479, button: "left" },
          ],
          640,
          480,
        );
        await driver.execute(
          { type: "click", x: 639, y: 479, button: "left" },
          context,
        );
        await driver.execute(
          { type: "keypress", keys: ["CTRL", "ENTER"] },
          context,
        );
        await new Promise((r) => setTimeout(r, 50));
        assert.ok(
          events.some(
            (m) =>
              m.event_type === "mouse_move" &&
              m.event.to.x === 32767 &&
              m.event.to.y === 32767,
          ),
        );
        assert.ok(
          events.some(
            (m) => m.event_type === "mouse_button" && m.event.state === false,
          ),
        );
        assert.ok(
          events.some(
            (m) =>
              m.event_type === "key" &&
              m.event.key === "Enter" &&
              m.event.state === false,
          ),
        );
        assert.throws(
          () => driver.validate([{ type: "type", text: "🙂" }], 640, 480),
          /U\+1F642/,
        );
        await driver.disconnect();
        await driver.connect(AbortSignal.timeout(3000));
        assert.equal(
          (await driver.snapshot(AbortSignal.timeout(3000))).height,
          480,
        );
        if (finishOldProbe) {
          assert.equal((await driver.capabilities()).audio.from_target, true);
          finishOldProbe();
          await Promise.resolve();
          assert.equal(
            (await driver.capabilities()).audio.from_target,
            true,
            "a late probe from the disconnected audio source must not overwrite the current capabilities",
          );
        }
      } finally {
        await driver.disconnect();
        for (const ws of wss.clients) ws.terminate();
        wss.close();
        server.closeAllConnections();
        await new Promise<void>((r) => server.close(() => r()));
      }
    },
  );

test("PiKVM retries snapshot warmup without reconnecting and preserves auth failures", async () => {
  const image = await sharp({
    create: { width: 32, height: 24, channels: 3, background: "#234" },
  })
    .jpeg()
    .toBuffer();
  let calls = 0,
    status = 503;
  const server = createServer((q, r) => {
    calls++;
    if (status === 503 && calls > 2) status = 200;
    r.statusCode = status;
    r.end(status === 200 ? image : "unavailable");
  });
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
  const driver = new PiKvmDriver(
    {
      ...deviceSchema.parse({
        name: "Warmup",
        driver_id: "pikvm-v4",
        address: `http://127.0.0.1:${(server.address() as any).port}`,
      }),
      device_id: randomUUID(),
    },
    {},
  );
  try {
    const frame = await driver.snapshot(AbortSignal.timeout(2000));
    assert.equal(calls, 3);
    assert.equal(frame.width, 32);
    status = 401;
    await assert.rejects(driver.snapshot(AbortSignal.timeout(2000)), {
      code: "AUTH_FAILED",
    });
    assert.equal(calls, 4, "authentication failures must not be retried");
    status = 503;
    calls = -100;
    await assert.rejects(driver.snapshot(AbortSignal.timeout(50)));
    assert.equal(calls, -99, "cancellation must stop the retry");
  } finally {
    await driver.disconnect();
    server.closeAllConnections();
    await new Promise<void>((r) => server.close(() => r()));
  }
});
