<script setup lang="ts">
import { ref, onMounted, onBeforeUnmount, watch } from "vue";
import Button from "./ui/button/Button.vue";
import { api, headers } from "../api";
import { useComputer } from "../use-computer";
import { useAudio } from "../use-audio";
import type { DeviceView } from "../../shared/contracts";
const props = defineProps<{ device: DeviceView; overview?: boolean }>();
const emit = defineEmits(["error", "refresh"]);
const {
  session,
  frame,
  source,
  busy,
  control,
  last,
  error,
  frameError,
  acquire,
  send,
  stop,
  releaseInputs,
  isDisposed,
} = useComputer(
  () => props.device,
  () => !!props.overview,
  () => emit("refresh"),
);
const typing = ref("");
const image = ref<HTMLImageElement>(),
  root = ref<HTMLElement>();
const keyboard = ref(false);
let drag: { x: number; y: number }[] = [];
function coords(e: PointerEvent | MouseEvent | WheelEvent) {
  const b = image.value!.getBoundingClientRect();
  return {
    x: Math.max(
      0,
      Math.min(
        frame.value!.width - 1,
        Math.floor(((e.clientX - b.left) / b.width) * frame.value!.width),
      ),
    ),
    y: Math.max(
      0,
      Math.min(
        frame.value!.height - 1,
        Math.floor(((e.clientY - b.top) / b.height) * frame.value!.height),
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
  if (!keyboard.value || !control.value || e.target !== root.value) return;
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

function blur() {
  keyboard.value = false;
  drag = [];
  void stopAudio();
  void releaseInputs();
}
function localFocus(e: FocusEvent) {
  if (e.target !== root.value && keyboard.value) {
    keyboard.value = false;
    void releaseInputs();
  }
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
const {
  audioPlaying,
  mic,
  volume,
  audioError,
  inputs,
  input,
  level,
  startAudio,
  stopAudio,
} = useAudio(session, control, isDisposed);
watch(control, (value) => {
  if (!value) {
    keyboard.value = false;
    drag = [];
  }
});
onMounted(() => {
  window.addEventListener("blur", blur);
  window.addEventListener("pagehide", pageHide);
  document.addEventListener("visibilitychange", blur);
});
onBeforeUnmount(() => {
  window.removeEventListener("blur", blur);
  window.removeEventListener("pagehide", pageHide);
  document.removeEventListener("visibilitychange", blur);
  void stopAudio();
});
</script>
<template>
  <section
    ref="root"
    class="screen"
    :class="{ overview }"
    @keydown="key"
    @focusin="localFocus"
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
              scroll_x: Math.max(
                -1000,
                Math.min(1000, Math.round($event.deltaX)),
              ),
              scroll_y: Math.max(
                -1000,
                Math.min(1000, Math.round($event.deltaY)),
              ),
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
          @pointerleave="stopAudio"
          @pointercancel="stopAudio"
          @keydown.space.prevent="startAudio(true)"
          @keyup.space.prevent="stopAudio"
          >{{ mic ? "● Mikrofon sendet" : "Zum Sprechen halten" }}</Button
        ><small>{{ audioError || device.capabilities?.audio.reason }}</small>
      </div></template
    >
  </section>
</template>
