import { test } from "node:test";
import assert from "node:assert/strict";
import { once } from "node:events";
import { setTimeout as delay } from "node:timers/promises";
import { WebSocketServer } from "ws";
import {
  RTCPeerConnection,
  RTCRtpCodecParameters,
  MediaStreamTrack,
  RtpHeader,
  RtpPacket,
} from "werift";
import OpusScript from "opusscript";
import { localIceOnly } from "../server/lan-ice.js";
// Exercise the built worker, which runs in a separate Node process context.
// Like the browser integration tests, this requires npm run build first.
// @ts-ignore compiled server module
import { PiAudio } from "../dist/server/audio.js";

test(
  "an unanswered audio probe is retried instead of leaving audio permanently unavailable",
  { timeout: 20000 },
  async () => {
    const server = new WebSocketServer({ host: "127.0.0.1", port: 0 });
    await once(server, "listening");
    let connections = 0;
    server.on("connection", (socket) => {
      const attempt = ++connections;
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
        if (m.body?.request === "features" && attempt > 1)
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
      });
    });
    const audio = new PiAudio({
      device_id: "fixture",
      address: `http://127.0.0.1:${(server.address() as { port: number }).port}`,
      headers: {},
    });
    try {
      const recovered = once(audio, "features");
      const unavailable = await audio.probe();
      assert.equal(unavailable.reason, "AUDIO_PROBE_TIMEOUT");
      await recovered;
      assert.equal((await audio.probe()).from_target, true);
      assert.equal(connections, 2);
    } finally {
      await audio.close();
      for (const socket of server.clients) socket.terminate();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    }
  },
);

test(
  "PiKVM audio survives repeated stops and concurrent listeners with fresh Janus handles",
  { timeout: 45000 },
  async () => {
    const server = new WebSocketServer({ host: "127.0.0.1", port: 0 });
    await once(server, "listening");
    let nextHandle = 0;
    const watched: number[] = [],
      peers = new Map<number, RTCPeerConnection>();
    const timers: ReturnType<typeof setInterval>[] = [];
    const errors: unknown[] = [];
    const encoder = new OpusScript(48000, 2, OpusScript.Application.AUDIO);
    const pcm = Buffer.alloc(3840);
    for (let i = 0; i < 960; i++)
      for (let c = 0; c < 2; c++)
        pcm.writeInt16LE(
          Math.round(12000 * Math.sin((i * 2 * Math.PI * 1000) / 48000)),
          i * 4 + c * 2,
        );
    server.on("connection", (socket) => {
      let chain = Promise.resolve();
      socket.on("message", (raw) => {
        chain = chain
          .then(async () => {
            const m = JSON.parse(raw.toString()),
              id = m.handle_id;
            const send = (data: any) => {
              if (socket.readyState === 1) socket.send(JSON.stringify(data));
            };
            const success = (data = {}) =>
              send({ janus: "success", transaction: m.transaction, data });
            if (m.janus === "create") success({ id: 1 });
            if (m.janus === "attach") success({ id: ++nextHandle });
            if (m.janus === "detach") {
              await peers.get(id)?.close();
              peers.delete(id);
              success();
            }
            if (m.body?.request === "features")
              send({
                janus: "event",
                sender: id,
                plugindata: {
                  data: {
                    result: {
                      status: "features",
                      features: { audio: true, mic: false },
                    },
                  },
                },
              });
            if (m.body?.request === "watch") {
              assert.ok(
                !watched.includes(id),
                "a stopped Janus handle cannot be reused",
              );
              watched.push(id);
              const peer = new RTCPeerConnection({
                iceServers: [],
                codecs: {
                  audio: [
                    new RTCRtpCodecParameters({
                      mimeType: "audio/opus",
                      clockRate: 48000,
                      channels: 2,
                      payloadType: 111,
                    }),
                  ],
                },
              });
              peers.set(id, peer);
              const track = new MediaStreamTrack({ kind: "audio" });
              peer.addTrack(track);
              localIceOnly(peer);
              await peer.setLocalDescription(await peer.createOffer());
              send({
                janus: "event",
                sender: id,
                jsep: {
                  type: "offer",
                  sdp:
                    peer.localDescription!.sdp +
                    "a=candidate:99 1 UDP 1 8.8.8.8 9000 typ host\r\n",
                },
              });
              let sequence = 0;
              timers.push(
                setInterval(() => {
                  if (peer.connectionState === "connected")
                    track.writeRtp(
                      new RtpPacket(
                        new RtpHeader({
                          payloadType: 111,
                          sequenceNumber: sequence++ % 65536,
                          timestamp: sequence * 960,
                          ssrc: 123,
                        }),
                        encoder.encode(pcm, 960),
                      ),
                    );
                }, 20),
              );
              // Late signaling from the detached handle must not replace the new peer.
              send({
                janus: "event",
                sender: id - 1,
                jsep: { type: "offer", sdp: "invalid stale offer" },
              });
            }
            if (m.body?.request === "start")
              await peers.get(id)!.setRemoteDescription(m.jsep);
            if (m.janus === "trickle" && !m.candidate.completed)
              await peers.get(id)?.addIceCandidate(m.candidate);
          })
          .catch((e) => errors.push(e));
      });
    });
    const audio = new PiAudio({
      device_id: "fixture",
      address: `http://127.0.0.1:${(server.address() as { port: number }).port}`,
      headers: {},
    });
    try {
      for (let round = 0; round < 3; round++) {
        const abort = new AbortController(),
          stream = audio.subscribe(abort.signal)[Symbol.asyncIterator]();
        const read = () =>
          Promise.race([
            stream.next(),
            delay(8000).then(() => {
              throw Error("No audio received");
            }),
          ]);
        const first: any = await read();
        assert.equal(first.value.discontinuity, true);
        assert.ok(
          first.value.data.some((v: number) => v !== 0),
          "decoded tone must be non-silent",
        );
        const otherAbort = new AbortController(),
          other = audio.subscribe(otherAbort.signal)[Symbol.asyncIterator]();
        assert.equal((await other.next()).done, false);
        otherAbort.abort();
        await other.return();
        assert.equal(
          (await read()).done,
          false,
          "stopping one listener keeps the other running",
        );
        abort.abort();
        await stream.return();
      }
      assert.equal(watched.length, 3);
      assert.deepEqual(errors, []);
    } finally {
      await audio.close();
      for (const timer of timers) clearInterval(timer);
      for (const peer of peers.values()) await peer.close();
      encoder.delete();
      for (const socket of server.clients) socket.terminate();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    }
  },
);

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
