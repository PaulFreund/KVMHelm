import { test } from "node:test";
import assert from "node:assert/strict";
import { once } from "node:events";
import { setTimeout as delay } from "node:timers/promises";
import WebSocket from "ws";
import { fixture } from "./fixture.js";
import { serve } from "../server/server.js";

test(
  "audio uses driver contract and enforces one microphone across listeners",
  { timeout: 15000 },
  async (t) => {
    const f = await fixture(t);
    const a = await serve(f.core, f.plugins, {
      host: "127.0.0.1",
      port: 0,
      origins: [],
    });
    const b = await serve(f.core, f.plugins, {
      host: "127.0.0.1",
      port: 0,
      origins: [],
    });
    const url = (s: typeof a) =>
      `http://127.0.0.1:${(s.server.address() as { port: number }).port}`;
    const request = async (base: string, path: string, body: unknown) => {
      const r = await fetch(base + "/api/v1" + path, {
        method: "POST",
        headers: {
          Authorization: `Bearer ${f.token.token}`,
          "Content-Type": "application/json",
        },
        body: JSON.stringify(body),
      });
      assert.equal(r.status, 200);
      return r.json();
    };
    const sockets: WebSocket[] = [];
    try {
      const opened = await request(url(a), "/tools/open_computer", {
        computer_id: f.device.device_id,
        mode: "control",
        request_id: "open",
      });
      const session_id = opened.structuredContent.session.session_id;
      const r = f.core.get(f.device.device_id);
      Object.assign(r.caps!.audio, { from_target: true, to_target: true });
      const mic: boolean[] = [],
        packets: Buffer[] = [];
      r.driver.setMicrophone = async (enabled) => {
        mic.push(enabled);
      };
      r.driver.sendAudio = async (chunk, ctx) => {
        ctx.assertControl();
        packets.push(chunk.data);
      };
      r.driver.subscribeAudio = async function* (signal) {
        while (!signal.aborted) {
          await delay(10);
          if (!signal.aborted)
            yield {
              device_id: f.device.device_id,
              format: "pcm_s16le" as const,
              sample_rate: 48000,
              channels: 2,
              sequence: 0,
              timestamp_ms: performance.now(),
              connection_generation: 1,
              discontinuity: false,
              data: Buffer.alloc(8),
            };
        }
      };
      const connect = async (server: typeof a) => {
        const base = url(server),
          ticket = await request(base, `/sessions/${session_id}/audio`, {
            microphone: true,
          });
        const ws = new WebSocket(
          base.replace("http:", "ws:") + "/api/v1/audio",
          { origin: base },
        );
        sockets.push(ws);
        await once(ws, "open");
        ws.send(JSON.stringify({ ticket: ticket.ticket }));
        return ws;
      };
      const first = await connect(a);
      for (let n = 0; !mic.includes(true) && n < 50; n++) await delay(10);
      assert.deepEqual(mic, [true]);
      first.send(Buffer.from([1, 0, 2, 0]));
      for (let n = 0; !packets.length && n < 50; n++) await delay(10);
      assert.deepEqual(packets, [Buffer.from([1, 0, 2, 0])]);
      const second = await connect(b);
      const [code] = await once(second, "close");
      assert.equal(code, 1008);
      assert.equal(mic.filter((x) => x).length, 1);
      first.close();
      await once(first, "close");
      for (let n = 0; mic.at(-1) !== false && n < 50; n++) await delay(10);
      assert.equal(mic.at(-1), false);
    } finally {
      for (const ws of sockets) ws.terminate();
      await a.close();
      await b.close();
    }
  },
);
