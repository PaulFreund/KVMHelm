<script setup lang="ts">
import { ref, onMounted, onBeforeUnmount, watch } from "vue";
import Button from "./ui/button/Button.vue";
import { api, tool, headers } from "../api";
const props = defineProps<{ device: any; overview?: boolean }>();
const emit = defineEmits(["error", "refresh"]);
const session = ref<any>();
const frame = ref<any>();
const source = ref("");
const busy = ref(false);
const control = ref(false);
const typing = ref("");
const last = ref(0);
const error = ref("");
const frameError = ref("");
let pendingFrameError = "";
let healthTimer: ReturnType<typeof setInterval>;
let pollAbort: AbortController | undefined;
const image = ref<HTMLImageElement>();
const root = ref<HTMLElement>();
const keyboard = ref(false);
let timer: ReturnType<typeof setTimeout>;
let disposed = false;
let working = false;
let drag: { x: number; y: number }[] = [];
let actionTail = Promise.resolve();
function receivedFrame(info: any) {
  last.value = Date.now() - Math.max(0, info?.received_age_ms ?? 0);
  pendingFrameError = "";
  frameError.value = info?.stale ? "Kein aktuelles Bild" : "";
}
function checkFrameHealth() {
  // A dropped HTTP request does not make a recent, complete image stale.
  if (pendingFrameError && (!last.value || Date.now() - last.value >= 2000))
    frameError.value = pendingFrameError;
}
function show(result: any) {
  const picture = result.content.find((x: any) => x.type === "image");
  if (picture) {
    source.value = `data:${picture.mimeType};base64,${picture.data}`;
    frame.value = result.structuredContent.frames.at(-1);
    receivedFrame(frame.value);
  }
  if (result.structuredContent.session)
    session.value = result.structuredContent.session;
}
async function poll() {
  if (disposed) return;
  if (!working && session.value) {
    try {
      pollAbort = new AbortController();
      const response = await fetch(
        `/api/v1/sessions/${session.value.session_id}/frame`,
        { headers: headers(), signal: AbortSignal.any([pollAbort.signal, AbortSignal.timeout(5000)]) },
      );
      if (!response.ok) {
        const body = await response.json().catch(() => null);
        const message = body?.error?.message ?? `Bildabruf fehlgeschlagen (HTTP ${response.status})`;
        if ([401, 403].includes(response.status)) frameError.value = message;
        throw Error(message);
      }
      const info = JSON.parse(response.headers.get("X-KVMHelm-Frame")!);
      const blob = await response.blob();
      if (disposed) return;
      if (source.value.startsWith("blob:")) URL.revokeObjectURL(source.value);
      source.value = URL.createObjectURL(blob);
      frame.value = info;
      receivedFrame(info);
    } catch (e) {
      if (disposed) return;
      pendingFrameError = e instanceof TypeError ? "Bildverbindung unterbrochen – erneuter Versuch läuft" : (e as Error).message;
      checkFrameHealth();
    } finally {
      pollAbort = undefined;
    }
  }
  timer = setTimeout(poll, props.overview ? 200 : 60);
}
async function open() {
  try {
    const result = await tool("open_computer", {
      computer_id: props.device.device_id,
      mode: "observe",
      profile: props.overview ? "overview" : "active",
    });
    if (disposed) {
      await tool("close_computer", {
        session_id: result.structuredContent.session.session_id,
      });
      return;
    }
    show(result);
    void poll();
  } catch (e) {
    error.value = (e as Error).message;
  }
}
async function acquire() {
  if (!session.value) return;
  busy.value = true;
  try {
    await tool("computer_control", {
      session_id: session.value.session_id,
      request_id: crypto.randomUUID(),
      operation: control.value ? "release" : "acquire",
    });
    control.value = !control.value;
    keyboard.value = false;
    if (control.value)
      show(
        await tool("computer_screenshot", {
          session_id: session.value.session_id,
        }),
      );
    emit("refresh");
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    busy.value = false;
  }
}
async function send(actions: any[]) {
  actionTail = actionTail.then(async () => {
    if (!control.value || !frame.value || disposed) return;
    working = true;
    try {
      const f = frame.value;
      show(
        await tool("computer", {
          session_id: session.value.session_id,
          request_id: crypto.randomUUID(),
          reference: {
            frame_id: f.frame_id,
            view_id: f.view_id,
            input_revision: f.input_revision,
          },
          actions,
        }),
      );
      error.value = "";
    } catch (e) {
      error.value = (e as Error).message;
      const result = (e as any).result;
      if (
        ["LEASE_EXPIRED", "LEASE_REVOKED", "CONTROL_SUSPENDED"].includes(
          result?.error?.code,
        )
      ) {
        control.value = false;
        keyboard.value = false;
      }
      try {
        show(
          await tool("computer_screenshot", {
            session_id: session.value.session_id,
          }),
        );
      } catch {}
    } finally {
      working = false;
    }
  });
  await actionTail;
}
function coords(e: PointerEvent | MouseEvent | WheelEvent) {
  const b = image.value!.getBoundingClientRect();
  return {
    x: Math.max(
      0,
      Math.min(
        frame.value.width - 1,
        Math.floor(((e.clientX - b.left) / b.width) * frame.value.width),
      ),
    ),
    y: Math.max(
      0,
      Math.min(
        frame.value.height - 1,
        Math.floor(((e.clientY - b.top) / b.height) * frame.value.height),
      ),
    ),
  };
}
function down(e: PointerEvent) {
  if (!control.value || !frame.value) return;
  e.preventDefault();
  (e.target as HTMLElement).setPointerCapture(e.pointerId);
  drag = [coords(e)];
}
function move(e: PointerEvent) {
  if (drag.length && drag.length < 255) drag.push(coords(e));
}
function up(e: PointerEvent) {
  if (!drag.length) return;
  const path = drag;
  drag = [];
  const p = coords(e);
  if (
    path.length > 2 &&
    Math.abs(p.x - path[0].x) + Math.abs(p.y - path[0].y) > 4
  )
    void send([{ type: "drag", path: [...path, p] }]);
  else
    void send([
      {
        type: "click",
        ...p,
        button: e.button === 2 ? "right" : e.button === 1 ? "middle" : "left",
      },
    ]);
}
function key(e: KeyboardEvent) {
  if (!keyboard.value || !control.value) return;
  e.preventDefault();
  if (e.repeat || ["Control", "Shift", "Alt", "Meta"].includes(e.key)) return;
  const keys = [];
  if (e.ctrlKey) keys.push("CTRL");
  if (e.shiftKey) keys.push("SHIFT");
  if (e.altKey) keys.push("ALT");
  if (e.metaKey) keys.push("META");
  keys.push(e.code);
  void send([{ type: "keypress", keys }]);
}
async function stop() {
  try {
    await api(`/devices/${props.device.device_id}/stop`, "POST", {});
    control.value = false;
    keyboard.value = false;
    emit("refresh");
  } catch (e) {
    error.value = (e as Error).message;
  }
}
function blur() {
  keyboard.value = false;
  drag = [];
  void stopAudio();
  if (control.value && session.value)
    void api(
      `/sessions/${session.value.session_id}/release-inputs`,
      "POST",
      {},
    ).catch(() => {});
}
function pageHide() {
  blur();
  if (session.value)
    void fetch("/api/v1/tools/close_computer", {
      method: "POST",
      keepalive: true,
      headers: { ...headers(), "Content-Type": "application/json" },
      body: JSON.stringify({ session_id: session.value.session_id }),
    }).catch(() => {});
}
const audioPlaying = ref(false),
  mic = ref(false),
  volume = ref(0.8),
  audioError = ref(""),
  inputs = ref<MediaDeviceInfo[]>([]),
  input = ref(""),
  level = ref(0);
let audioSocket: WebSocket | undefined,
  audioContext: AudioContext | undefined,
  micStream: MediaStream | undefined,
  processor: ScriptProcessorNode | undefined;
let scheduled = 0;
let audioGeneration = 0;
async function stopAudio() {
  audioGeneration++;
  audioPlaying.value = false;
  mic.value = false;
  level.value = 0;
  if (audioSocket) {
    audioSocket.onclose = null;
    audioSocket.close();
  }
  audioSocket = undefined;
  processor?.disconnect();
  processor = undefined;
  micStream?.getTracks().forEach((t) => t.stop());
  micStream = undefined;
  const context = audioContext;
  audioContext = undefined;
  await context?.close().catch(() => {});
}
async function startAudio(withMic = false) {
  const stopping = stopAudio();
  const generation = audioGeneration;
  await stopping;
  if (generation !== audioGeneration || disposed) return;
  if (withMic && !control.value) return;
  try {
    audioContext = new AudioContext({ sampleRate: 48000 });
    await audioContext.resume();
    if (generation !== audioGeneration || disposed) return;
    const ticket = await api(
      `/sessions/${session.value.session_id}/audio`,
      "POST",
      { microphone: withMic },
    );
    if (generation !== audioGeneration || disposed) return;
    audioSocket = new WebSocket(
      `${location.protocol === "https:" ? "wss:" : "ws:"}//${location.host}/api/v1/audio`,
    );
    audioSocket.binaryType = "arraybuffer";
    audioSocket.onopen = () =>
      audioSocket?.send(JSON.stringify({ ticket: ticket.ticket }));
    audioSocket.onclose = () => {
      void stopAudio();
    };
    audioSocket.onmessage = (e) => {
      if (typeof e.data === "string") {
        const m = JSON.parse(e.data);
        if (m.error) audioError.value = m.error;
        return;
      }
      if (!audioContext) return;
      const data = new Int16Array(e.data),
        channels = 2,
        length = data.length / channels,
        buffer = audioContext.createBuffer(channels, length, 48000);
      let peak = 0;
      for (let ch = 0; ch < channels; ch++) {
        const target = buffer.getChannelData(ch);
        for (let n = 0; n < length; n++) {
          target[n] = data[n * channels + ch] / 32768;
          peak = Math.max(peak, Math.abs(target[n]));
        }
      }
      level.value = peak;
      const source = audioContext.createBufferSource(),
        gain = audioContext.createGain();
      gain.gain.value = volume.value;
      source.buffer = buffer;
      source.connect(gain).connect(audioContext.destination);
      if (scheduled > audioContext.currentTime + 0.3)
        scheduled = audioContext.currentTime;
      scheduled = Math.max(scheduled, audioContext.currentTime);
      source.start(scheduled);
      scheduled += length / 48000;
    };
    audioPlaying.value = true;
    if (withMic) {
      const acquiredStream = await navigator.mediaDevices.getUserMedia({
        audio: {
          deviceId: input.value ? { exact: input.value } : undefined,
          channelCount: 1,
          sampleRate: 48000,
        },
      });
      if (generation !== audioGeneration || disposed || !control.value) {
        acquiredStream.getTracks().forEach((t) => t.stop());
        return;
      }
      micStream = acquiredStream;
      mic.value = true;
      inputs.value = (await navigator.mediaDevices.enumerateDevices()).filter(
        (d) => d.kind === "audioinput",
      );
      const source = audioContext.createMediaStreamSource(micStream);
      processor = audioContext.createScriptProcessor(4096, 1, 1);
      processor.onaudioprocess = (e) => {
        if (
          !mic.value ||
          audioSocket?.readyState !== WebSocket.OPEN ||
          audioSocket.bufferedAmount > 96000
        )
          return;
        const samples = e.inputBuffer.getChannelData(0),
          pcm = new Int16Array(samples.length);
        for (let n = 0; n < samples.length; n++)
          pcm[n] = Math.max(-1, Math.min(1, samples[n])) * 32767;
        audioSocket.send(pcm.buffer);
      };
      const silent = audioContext.createGain();
      silent.gain.value = 0;
      source.connect(processor);
      processor.connect(silent).connect(audioContext.destination);
    }
  } catch (e) {
    audioError.value = (e as Error).message;
    await stopAudio();
  }
}
onMounted(() => {
  void open();
  healthTimer = setInterval(checkFrameHealth, 200);
  window.addEventListener("blur", blur);
  window.addEventListener("pagehide", pageHide);
  document.addEventListener("visibilitychange", blur);
});
onBeforeUnmount(() => {
  disposed = true;
  clearTimeout(timer);
  clearInterval(healthTimer);
  pollAbort?.abort();
  window.removeEventListener("blur", blur);
  window.removeEventListener("pagehide", pageHide);
  document.removeEventListener("visibilitychange", blur);
  void stopAudio();
  if (source.value.startsWith("blob:")) URL.revokeObjectURL(source.value);
  if (session.value)
    void tool("close_computer", { session_id: session.value.session_id }).catch(
      () => {},
    );
});
watch(
  () => props.device.control_suspended,
  (v) => {
    if (v) {
      control.value = false;
      blur();
    }
  },
);
</script>
<template>
  <section
    ref="root"
    class="screen"
    :class="{ overview }"
    @keydown="key"
    tabindex="0"
  >
    <div class="screen-top">
      <div>
        <span class="status-dot" :class="device.status" />
        <strong>{{ device.name }}</strong
        ><small>{{ device.model }} · {{ device.status }}</small>
      </div>
      <span class="pill" :class="{ accent: control }">{{
        control ? "Steuerung aktiv" : "Beobachten"
      }}</span>
    </div>
    <div class="display" @contextmenu.prevent>
      <img
        v-if="source"
        ref="image"
        :src="source"
        :alt="'Livebild ' + device.name"
        draggable="false"
        @pointerdown="down"
        @pointermove="move"
        @pointerup="up"
        @pointercancel="drag = []"
        @wheel.prevent="
          control &&
          send([
            {
              type: 'scroll',
              ...coords($event),
              scroll_x: Math.round($event.deltaX),
              scroll_y: Math.round($event.deltaY),
            },
          ])
        "
      />
      <div v-else class="empty-display">
        {{ error || "Verbindung wird aufgebaut …" }}
      </div>
      <span v-if="frameError" class="frame-error"
        >{{ frameError }} · Bild möglicherweise veraltet</span
      ><span v-if="device.driver_id === 'simulator'" class="simulated"
        >SIMULATOR</span
      >
    </div>
    <div class="screen-meta">
      <span
        >{{ frame ? `${frame.width} × ${frame.height}` : "Kein Bild" }}
        <span class="muted"
          >· {{ device.media?.strategy ?? "snapshot" }}</span
        ></span
      ><span>{{
        frameError
          ? "⚠ Kein aktuelles Bild"
          : last
            ? "● Live · Empfangszeit"
            : "Verbinden"
      }}</span>
    </div>
    <p v-if="error" class="error" role="alert">{{ error }}</p>
    <template v-if="!overview"
      ><div class="toolbar">
        <Button
          :disabled="busy || !session || device.control_suspended"
          @click="acquire"
          >{{ control ? "Steuerung freigeben" : "Steuerung anfordern" }}</Button
        ><Button
          variant="outline"
          :disabled="!control"
          @click="
            keyboard = !keyboard;
            root?.focus();
          "
          >Tastatur {{ keyboard ? "aktiv" : "aktivieren" }}</Button
        ><Button variant="ghost" @click="root?.requestFullscreen()"
          >Vollbild ↗</Button
        ><Button variant="destructive" @click="stop">■ Not-Stopp</Button>
      </div>
      <div v-if="device.lease && !control" class="notice">
        Steuerung belegt durch {{ device.lease.client_id }}
      </div>
      <div v-if="device.control_suspended" class="notice">
        Steuerung suspendiert.
        <Button
          variant="outline"
          @click="
            api(`/devices/${device.device_id}/resume`, 'POST', {}).then(() =>
              emit('refresh'),
            )
          "
          >Entsperren</Button
        >
      </div>
      <div class="input-tools">
        <input
          v-model="typing"
          aria-label="Text auf Ziel eingeben"
          placeholder="Text bewusst auf dem Ziel eingeben …"
          :disabled="!control"
        /><Button
          variant="outline"
          :disabled="!control || !typing"
          @click="
            send([{ type: 'type', text: typing }]);
            typing = '';
          "
          >Senden</Button
        ><Button
          variant="ghost"
          :disabled="!control"
          @click="send([{ type: 'keypress', keys: ['CTRL', 'ALT', 'DELETE'] }])"
          >Ctrl Alt Del</Button
        ><Button
          variant="ghost"
          :disabled="!control"
          @click="send([{ type: 'keypress', keys: ['ALT', 'TAB'] }])"
          >Alt Tab</Button
        >
      </div>
      <div class="audio-bar">
        <span>Audio</span
        ><Button
          size="sm"
          variant="outline"
          :disabled="!device.capabilities?.audio.from_target"
          @click="audioPlaying ? stopAudio() : startAudio()"
          >{{ audioPlaying ? "Stummschalten" : "Ziel hören" }}</Button
        ><input
          v-model="volume"
          type="range"
          min="0"
          max="1"
          step="0.05"
          aria-label="Lautstärke"
        /><meter
          :value="level"
          min="0"
          max="1"
          aria-label="Audiopegel"
        /><select v-if="inputs.length" v-model="input" aria-label="Mikrofon">
          <option value="">Standardmikrofon</option>
          <option v-for="d in inputs" :key="d.deviceId" :value="d.deviceId">
            {{ d.label }}
          </option></select
        ><Button
          size="sm"
          variant="outline"
          :disabled="!control || !device.capabilities?.audio.to_target"
          @pointerdown="startAudio(true)"
          @pointerup="stopAudio"
          @pointerleave="mic && stopAudio()"
          @keydown.space.prevent="startAudio(true)"
          @keyup.space.prevent="stopAudio"
          >{{ mic ? "● Mikrofon sendet" : "Zum Sprechen halten" }}</Button
        ><small>{{ audioError || device.capabilities?.audio.reason }}</small>
      </div></template
    >
  </section>
</template>
