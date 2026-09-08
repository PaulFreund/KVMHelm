import { parentPort, workerData } from "node:worker_threads";
import { randomUUID, randomBytes, createHash } from "node:crypto";
import WebSocket from "ws";
import {
  RTCPeerConnection,
  RTCRtpCodecParameters,
  MediaStreamTrack,
  RtpPacket,
  RtpHeader,
} from "werift";
import OpusScript from "opusscript";
import { localLookup, PinnedAgent } from "./network.js";
import {
  localIceOnly,
  checkIceCandidate,
  localIceDescription,
} from "./lan-ice.js";
const port = parentPort!;
let ws: WebSocket | undefined,
  session: number | undefined,
  handle: number | undefined,
  pc: RTCPeerConnection | undefined,
  track: MediaStreamTrack | undefined;
let microphone = false,
  active = false,
  credits = 8,
  gap = true,
  seq = 0,
  generation = 0,
  lastSequence: number | undefined;
let pcm = Buffer.alloc(0);
const decoder = new OpusScript(48000, 2, OpusScript.Application.AUDIO),
  encoder = new OpusScript(48000, 1, OpusScript.Application.VOIP);
let rtpSeq = randomBytes(2).readUInt16BE(),
  timestamp = randomBytes(4).readUInt32BE();
const ssrc = randomBytes(4).readUInt32BE();
const pending = new Map<
  string,
  {
    resolve: (m: any) => void;
    reject: (e: Error) => void;
    timer: NodeJS.Timeout;
  }
>();
function send(m: any) {
  if (ws?.readyState !== WebSocket.OPEN) throw Error("Audio signaling offline");
  ws.send(JSON.stringify({ session_id: session, handle_id: handle, ...m }));
}
function request(m: any) {
  return new Promise<any>((resolve, reject) => {
    const transaction = randomUUID(),
      timer = setTimeout(() => {
        pending.delete(transaction);
        reject(Error("Janus timeout"));
      }, 5000);
    pending.set(transaction, { resolve, reject, timer });
    send({ ...m, transaction });
  });
}
async function watch() {
  await pc?.close();
  pc = undefined;
  track = undefined;
  pcm = Buffer.alloc(0);
  generation++;
  gap = true;
  lastSequence = undefined;
  send({
    janus: "message",
    transaction: randomUUID(),
    body: { request: "stop" },
  });
  if (active)
    send({
      janus: "message",
      transaction: randomUUID(),
      body: {
        request: "watch",
        params: { orientation: 0, audio: true, mic: microphone, camera: false },
      },
    });
  else
    send({
      janus: "message",
      transaction: randomUUID(),
      body: { request: "stop" },
    });
}
async function offer(jsep: any) {
  if (!active) return;
  const description = { ...jsep, sdp: localIceDescription(String(jsep.sdp)) };
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
      video: [
        new RTCRtpCodecParameters({
          mimeType: "video/H264",
          clockRate: 90000,
          payloadType: 96,
          parameters:
            "packetization-mode=1;profile-level-id=42e01f;level-asymmetry-allowed=1",
        }),
      ],
    },
  });
  pc = peer;
  peer.onIceCandidate.subscribe((c) => {
    try {
      send({
        janus: "trickle",
        transaction: randomUUID(),
        candidate: c ? c.toJSON() : { completed: true },
      });
    } catch {}
  });
  peer.onTrack.subscribe((t) => {
    if (t.kind !== "audio") return;
    t.onReceiveRtp.subscribe((packet) => {
      if (!active) return;
      try {
        const current = packet.header.sequenceNumber;
        if (lastSequence !== undefined) {
          const delta = (current - lastSequence + 65536) % 65536;
          if (delta === 0 || delta > 32768) return;
          if (delta !== 1) gap = true;
        }
        lastSequence = current;
        const data = decoder.decode(packet.payload);
        if (credits <= 0) {
          gap = true;
          return;
        }
        credits--;
        port.postMessage({
          type: "chunk",
          chunk: {
            device_id: workerData.device_id,
            format: "pcm_s16le",
            sample_rate: 48000,
            channels: 2,
            sequence: seq++,
            timestamp_ms: performance.now(),
            connection_generation: generation,
            discontinuity: gap,
            data,
          },
        });
        gap = false;
      } catch {
        gap = true;
      }
    });
  });
  await peer.setRemoteDescription(description);
  for (const transceiver of peer.getTransceivers()) {
    if (transceiver.kind === "video") transceiver.setDirection("inactive");
    if (transceiver.kind === "audio")
      transceiver.setDirection(microphone ? "sendrecv" : "recvonly");
  }
  if (microphone) {
    track = new MediaStreamTrack({ kind: "audio" });
    peer.addTrack(track);
  }
  localIceOnly(peer);
  await peer.setLocalDescription(await peer.createAnswer());
  if (pc !== peer) return;
  send({
    janus: "message",
    transaction: randomUUID(),
    body: { request: "start" },
    jsep: { type: "answer", sdp: peer.localDescription!.sdp },
  });
}
async function start() {
  const url = new URL("/janus/ws", workerData.address);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  ws = new WebSocket(url, "janus-protocol", {
    headers: workerData.headers,
    ca: workerData.ca,
    rejectUnauthorized: !workerData.fingerprint && !workerData.insecure,
    lookup: localLookup,
    agent: workerData.fingerprint
      ? new PinnedAgent(workerData.fingerprint)
      : undefined,
    handshakeTimeout: 5000,
  });
  await new Promise<void>((resolve, reject) => {
    ws!.once("error", reject);
    ws!.once("open", () => {
      if (workerData.fingerprint) {
        const cert = (ws as any)._socket.getPeerCertificate();
        if (
          !cert.raw ||
          createHash("sha256").update(cert.raw).digest("hex") !==
            workerData.fingerprint.replaceAll(":", "").toLowerCase()
        ) {
          ws?.terminate();
          reject(Error("Audio TLS pin mismatch"));
          return;
        }
      }
      resolve();
    });
  });
  ws.on("error", () =>
    port.postMessage({ type: "error", code: "AUDIO_SIGNALING_FAILED" }),
  );
  ws.on("close", () => {
    void pc?.close();
    port.postMessage({ type: "error", code: "AUDIO_DISCONNECTED" });
  });
  ws.on("message", (raw) => {
    try {
      const m = JSON.parse(raw.toString());
      const p = pending.get(m.transaction);
      if (p && m.janus !== "ack") {
        clearTimeout(p.timer);
        pending.delete(m.transaction);
        m.error ? p.reject(Error("Janus error")) : p.resolve(m);
      }
      if (m.jsep)
        void offer(m.jsep).catch(() =>
          port.postMessage({ type: "error", code: "AUDIO_NEGOTIATION_FAILED" }),
        );
      if (m.janus === "trickle" && m.candidate && !m.candidate.completed) {
        checkIceCandidate(m.candidate.candidate);
        void pc?.addIceCandidate(m.candidate).catch(() => {});
      }
      const result = m.plugindata?.data?.result;
      if (result?.status === "features")
        port.postMessage({
          type: "features",
          audio: result.features.audio === true,
          mic: result.features.mic === true,
        });
    } catch {}
  });
  session = (await request({ janus: "create" })).data.id;
  handle = (
    await request({ janus: "attach", plugin: "janus.plugin.ustreamer" })
  ).data.id;
  send({
    janus: "message",
    transaction: randomUUID(),
    body: { request: "features" },
  });
}
const heartbeat = setInterval(() => {
  if (session)
    try {
      send({ janus: "keepalive", transaction: randomUUID() });
    } catch {}
}, 15000);
const clock = setInterval(() => {
  if (!microphone || !track || pcm.length < 1920) return;
  try {
    const samples = pcm.subarray(0, 1920);
    pcm = pcm.subarray(1920);
    track.writeRtp(
      new RtpPacket(
        new RtpHeader({
          payloadType: 111,
          sequenceNumber: rtpSeq++ % 65536,
          timestamp,
          ssrc,
        }),
        encoder.encode(samples, 960),
      ),
    );
    timestamp = (timestamp + 960) >>> 0;
  } catch {
    pcm = Buffer.alloc(0);
  }
}, 20);
port.on("message", (m) => {
  if (m.type === "credit") {
    credits = Math.min(8, credits + 1);
  }
  if (m.type === "watch") {
    active = m.active;
    microphone = m.microphone;
    void watch().catch(() =>
      port.postMessage({ type: "error", code: "AUDIO_NEGOTIATION_FAILED" }),
    );
  }
  if (m.type === "pcm" && microphone) {
    const data = Buffer.from(m.data);
    if (pcm.length + data.length > 48000) {
      pcm = Buffer.alloc(0);
      gap = true;
    }
    pcm = Buffer.concat([pcm, data]);
  }
  if (m.type === "stop") {
    active = false;
    clearInterval(clock);
    clearInterval(heartbeat);
    try {
      send({ janus: "destroy", transaction: randomUUID() });
    } catch {}
    ws?.close();
    void pc?.close();
    decoder.delete();
    encoder.delete();
    port.close();
  }
});
void start().catch(() => {
  port.postMessage({ type: "error", code: "AUDIO_UNAVAILABLE" });
  clearInterval(clock);
  clearInterval(heartbeat);
});
