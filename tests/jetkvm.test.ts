import { test } from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { randomUUID } from "node:crypto";
import { chromium } from "playwright";
import { WebSocketServer } from "ws";
import { JetKvmDriver } from "../server/drivers/jetkvm.js";
import { usbKey } from "../server/drivers/usb-key.js";
import { deviceSchema } from "../shared/contracts.js";

test(
  "JetKVM: actual WebRTC H264 frames, JSON-RPC HID, authentication and reconnect",
  { timeout: 45000 },
  async () => {
    const browser = await chromium.launch({
      headless: true,
      args: [
        "--disable-features=WebRtcHideLocalIpsWithMdns",
        "--autoplay-policy=no-user-gesture-required",
      ],
    });
    const peers: any[] = [];
    const calls: any[] = [];
    let logins = 0;
    const server = createServer((q, r) => {
      r.setHeader("Content-Type", "application/json");
      if (q.url === "/device/status")
        return r.end(JSON.stringify({ isSetup: true }));
      if (q.url === "/auth/login-local") {
        let data = "";
        q.on("data", (b) => (data += b));
        q.on("end", () => {
          if (JSON.parse(data).password !== "fixture-only") {
            r.writeHead(401);
            r.end("{}");
            return;
          }
          logins++;
          r.setHeader(
            "Set-Cookie",
            "authToken=fixture-token; HttpOnly; Path=/",
          );
          r.end("{}");
        });
        return;
      }
      if (q.headers.cookie !== "authToken=fixture-token") {
        r.writeHead(401);
        r.end("{}");
        return;
      }
      if (q.url === "/device")
        return r.end(JSON.stringify({ authMode: "password" }));
      r.writeHead(404);
      r.end("{}");
    });
    const wss = new WebSocketServer({ noServer: true });
    server.on("upgrade", (q, s, h) => {
      if (
        q.url !== "/webrtc/signaling/client" ||
        q.headers.cookie !== "authToken=fixture-token"
      ) {
        s.destroy();
        return;
      }
      wss.handleUpgrade(q, s, h, (ws) => wss.emit("connection", ws, q));
    });
    wss.on("connection", (ws) => {
      let peer: any;
      let chain = Promise.resolve();
      ws.on("message", (raw) => {
        chain = chain
          .then(async () => {
            const message = JSON.parse(raw.toString());
            if (message.type === "offer") {
              peer = await browser.newPage();
              peers.push(peer);
              await peer.evaluate("globalThis.__name = (fn) => fn");
              await peer.exposeFunction("record", (m: any) => calls.push(m));
              await peer.exposeFunction("candidate", (data: any) => {
                if (ws.readyState === 1)
                  ws.send(JSON.stringify({ type: "new-ice-candidate", data }));
              });
              const answer = await peer.evaluate(async (sd: string) => {
                const pc = ((window as any).pc = new RTCPeerConnection({
                  iceServers: [],
                }));
                pc.onicecandidate = (e) => {
                  if (e.candidate)
                    (window as any).candidate(e.candidate.toJSON());
                };
                const canvas = document.createElement("canvas");
                canvas.width = 640;
                canvas.height = 480;
                const ctx = canvas.getContext("2d")!;
                let n = 0;
                setInterval(() => {
                  ctx.fillStyle = n++ % 2 ? "#222" : "#444";
                  ctx.fillRect(0, 0, 640, 480);
                }, 30);
                pc.addTrack(canvas.captureStream(30).getVideoTracks()[0]);
                const audio = new AudioContext(),
                  oscillator = audio.createOscillator(),
                  destination = audio.createMediaStreamDestination();
                oscillator.connect(destination);
                oscillator.start();
                await audio.resume();
                pc.addTrack(destination.stream.getAudioTracks()[0]);
                pc.ondatachannel = (e) => {
                  e.channel.onmessage = async (event) => {
                    const m = JSON.parse(event.data);
                    await (window as any).record(m);
                    const result =
                      m.method === "getVideoState"
                        ? { ready: true, width: 640, height: 480 }
                        : m.method === "getUSBState"
                          ? "configured"
                          : null;
                    e.channel.send(
                      JSON.stringify({ jsonrpc: "2.0", id: m.id, result }),
                    );
                  };
                };
                await pc.setRemoteDescription(JSON.parse(atob(sd)));
                await pc.setLocalDescription(await pc.createAnswer());
                return btoa(JSON.stringify(pc.localDescription));
              }, message.data.sd);
              ws.send(JSON.stringify({ type: "answer", data: answer }));
            } else if (message.type === "new-ice-candidate" && peer)
              await peer.evaluate(
                (data: any) => (window as any).pc.addIceCandidate(data),
                message.data,
              );
          })
          .catch(() => ws.close());
      });
    });
    await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
    const device = {
      ...deviceSchema.parse({
        name: "Jet fixture",
        driver_id: "jetkvm",
        model: "JetKVM",
        address: `http://127.0.0.1:${(server.address() as any).port}`,
        keyboard_layout: "de",
      }),
      device_id: randomUUID(),
    };
    const driver = new JetKvmDriver(device, { password: "fixture-only" });
    try {
      await driver.connect(AbortSignal.timeout(12000));
      assert.equal((await driver.capabilities()).keyboard.hid, true);
      const frame = await driver.snapshot(AbortSignal.timeout(3000));
      assert.equal(frame.width, 640);
      assert.equal(frame.height, 480);
      assert.equal(frame.mime_type, "image/jpeg");
      assert.ok(frame.data.length > 100);
      assert.equal((await driver.capabilities()).audio.from_target, true);
      for await (const chunk of driver.subscribeAudio(
        AbortSignal.timeout(3000),
      )) {
        assert.equal(chunk.format, "pcm_s16le");
        assert.equal(chunk.sample_rate, 48000);
        assert.ok(chunk.data.length > 0);
        break;
      }
      const context = {
        signal: AbortSignal.timeout(12000),
        width: 640,
        height: 480,
        assertControl() {},
      };
      const actions: any[] = [
        { type: "click", x: 639, y: 479, button: "right" },
        { type: "type", text: "Ä@" },
        { type: "keypress", keys: ["CTRL", "A"] },
        { type: "scroll", x: 0, y: 0, scroll_x: 100, scroll_y: 100 },
        {
          type: "drag",
          path: [
            { x: 0, y: 0 },
            { x: 50, y: 50 },
          ],
        },
      ];
      driver.validate(actions, 640, 480);
      for (const action of actions) await driver.execute(action, context);
      assert.ok(
        calls.some(
          (m) =>
            m.method === "absMouseReport" &&
            m.params.x === 32767 &&
            m.params.y === 32767 &&
            m.params.buttons === 2,
        ),
      );
      assert.ok(
        calls.some(
          (m) =>
            m.method === "keyboardReport" &&
            m.params.modifier === 64 &&
            m.params.keys.includes(20),
        ),
      );
      assert.ok(
        calls.some(
          (m) =>
            m.method === "wheelReport" &&
            m.params.wheelY === -1 &&
            m.params.wheelX === 1,
        ),
      );
      await driver.releaseAllInputs();
      assert.deepEqual(
        calls.filter((m) => m.method === "keyboardReport").at(-1).params,
        { modifier: 0, keys: [] },
      );
      assert.equal(usbKey("F24"), 115);
      assert.equal(usbKey("ALTGR"), 230);
      await driver.disconnect();
      await driver.connect(AbortSignal.timeout(12000));
      assert.equal(
        logins,
        1,
        "reuse cookie rather than invalidating other JetKVM sessions on reconnect",
      );
      assert.equal(
        (await driver.snapshot(AbortSignal.timeout(3000))).width,
        640,
      );
    } finally {
      await driver.disconnect();
      for (const ws of wss.clients) ws.terminate();
      wss.close();
      await browser.close();
      server.closeAllConnections();
      await new Promise<void>((r) => server.close(() => r()));
    }
  },
);
