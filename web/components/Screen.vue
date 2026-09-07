<script setup lang="ts">
import { ref, onMounted, onBeforeUnmount, watch, nextTick } from "vue";
import Button from "./ui/button/Button.vue";
import { api, headers } from "../api";
import { useComputer } from "../use-computer";
import { useAudio } from "../use-audio";
import type { DeviceView } from "../../shared/contracts";
const props = defineProps<{ device: DeviceView; overview?: boolean }>();
const emit = defineEmits(["error", "refresh"]);
const {
  session,
  rotating,
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
const {
  audioPlaying,
  audioPending,
  mic,
  micPending,
  volume,
  audioError,
  inputs,
  input,
  level,
  micLevel,
  startAudio,
  stopAudio,
  stopListening,
  stopMicrophone,
  refreshInputs,
} = useAudio(session, control, isDisposed, rotating);
const typing = ref(""),
  pasteOpen = ref(false),
  sending = ref(false),
  localError = ref("");
const image = ref<HTMLImageElement>(),
  root = ref<HTMLElement>(),
  viewport = ref<HTMLElement>(),
  pasteField = ref<HTMLTextAreaElement>();
const keyboard = ref(false),
  nativeSize = ref(false),
  fullscreen = ref(false),
  micMode = ref("toggle");
let drag: { x: number; y: number }[] = [],
  pointer: number | undefined,
  moved = false;
let clickTimer: ReturnType<typeof setTimeout> | undefined;
let pendingClick: (() => void) | undefined;
let hoverAt = 0;
function cancelPointer() {
  drag = [];
  pointer = undefined;
  clearTimeout(clickTimer);
  pendingClick = undefined;
}
function focusTarget() {
  viewport.value?.focus({ preventScroll: true });
}
async function takeControl() {
  await acquire();
  if (control.value) focusTarget();
}
function coords(e: MouseEvent | WheelEvent) {
  const b = image.value!.getBoundingClientRect(),
    f = frame.value!;
  return {
    x: Math.max(
      0,
      Math.min(
        f.width - 1,
        Math.floor(((e.clientX - b.left) / b.width) * f.width),
      ),
    ),
    y: Math.max(
      0,
      Math.min(
        f.height - 1,
        Math.floor(((e.clientY - b.top) / b.height) * f.height),
      ),
    ),
  };
}
function down(e: PointerEvent) {
  if (!control.value || !frame.value || pointer !== undefined) return;
  e.preventDefault();
  focusTarget();
  pointer = e.pointerId;
  moved = false;
  (e.target as HTMLElement).setPointerCapture(e.pointerId);
  drag = [coords(e)];
}
function move(e: PointerEvent) {
  if (!control.value || !frame.value) return;
  const p = coords(e);
  if (pointer === e.pointerId && drag.length) {
    if (Math.abs(p.x - drag[0].x) + Math.abs(p.y - drag[0].y) > 4) moved = true;
    if (drag.length < 255) drag.push(p);
  } else if (keyboard.value && Date.now() - hoverAt > 120) {
    hoverAt = Date.now();
    void send([{ type: "move", ...p }]);
  }
}
function up(e: PointerEvent) {
  if (pointer !== e.pointerId || !drag.length || !control.value || !frame.value)
    return;
  const path = drag,
    p = coords(e);
  drag = [];
  pointer = undefined;
  if (moved && e.button === 0) {
    clearTimeout(clickTimer);
    pendingClick?.();
    pendingClick = undefined;
    void send([{ type: "drag", path: [...path, p] }]);
  } else if (e.button !== 0)
    void send([
      { type: "click", ...p, button: e.button === 2 ? "right" : "middle" },
    ]);
}
function click(e: MouseEvent) {
  if (!control.value || !frame.value || moved) return;
  const p = coords(e);
  clearTimeout(clickTimer);
  if (e.detail === 2 && pendingClick) {
    pendingClick = undefined;
    void send([{ type: "double_click", ...p }]);
  } else {
    pendingClick?.();
    pendingClick = () => {
      if (control.value && keyboard.value)
        void send([{ type: "click", ...p, button: "left" }]);
    };
    clickTimer = setTimeout(() => {
      pendingClick?.();
      pendingClick = undefined;
    }, 300);
  }
}
function wheel(e: WheelEvent) {
  if (!control.value || !frame.value) return;
  e.preventDefault();
  const scale =
    e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? frame.value.height : 1;
  const clamp = (n: number) =>
    Math.max(-1000, Math.min(1000, Math.round(n * scale)));
  void send([
    {
      type: "scroll",
      ...coords(e),
      scroll_x: clamp(e.deltaX),
      scroll_y: clamp(e.deltaY),
    },
  ]);
}
function key(e: KeyboardEvent) {
  if (!control.value || e.target !== viewport.value) return;
  if (e.ctrlKey && e.altKey && e.code === "Escape") {
    e.preventDefault();
    viewport.value?.blur();
    return;
  }
  // Let the native paste event supply text; no remote Ctrl+V without a remote clipboard.
  if ((e.ctrlKey || e.metaKey) && e.code === "KeyV") return;
  e.preventDefault();
  if (
    e.isComposing ||
    ["Control", "Shift", "Alt", "Meta", "Dead"].includes(e.key)
  )
    return;
  if (
    e.key.length === 1 &&
    ((!e.ctrlKey && !e.altKey && !e.metaKey) || e.getModifierState("AltGraph"))
  ) {
    void send([{ type: "type", text: e.key }]);
    return;
  }
  const keys: string[] = [];
  if (e.ctrlKey) keys.push("CTRL");
  if (e.shiftKey) keys.push("SHIFT");
  if (e.altKey) keys.push("ALT");
  if (e.metaKey) keys.push("META");
  keys.push(e.code);
  void send([{ type: "keypress", keys }]);
}
function unfocus() {
  keyboard.value = false;
  cancelPointer();
  void releaseInputs();
}
function hide() {
  if (document.hidden) {
    unfocus();
    stopMicrophone();
  }
}
function blur() {
  unfocus();
  stopMicrophone();
}
async function openPaste(text?: string) {
  if (text !== undefined) typing.value = text;
  pasteOpen.value = true;
  await nextTick();
  pasteField.value?.focus();
}
function pasted(e: ClipboardEvent) {
  if (!control.value) return;
  e.preventDefault();
  void openPaste(e.clipboardData?.getData("text/plain") ?? "");
}
async function clipboard() {
  try {
    await openPaste(await navigator.clipboard.readText());
    localError.value = "";
  } catch {
    await openPaste();
    localError.value =
      "Zwischenablage nicht verfügbar. Bitte hier mit Strg+V / ⌘V einfügen.";
  }
}
async function sendText() {
  if (sending.value || !typing.value || typing.value.length > 4096) return;
  sending.value = true;
  const text = typing.value;
  try {
    if (await send([{ type: "type", text }])) {
      if (typing.value === text) typing.value = "";
      focusTarget();
    }
  } finally {
    sending.value = false;
  }
}
function shortcut(keys: readonly string[]) {
  focusTarget();
  void send([{ type: "keypress", keys: [...keys] }]);
}
function hold(e: PointerEvent) {
  if (e.button !== 0) return;
  (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
  void startAudio(true);
}
async function toggleFullscreen() {
  try {
    if (document.fullscreenElement) await document.exitFullscreen();
    else await root.value?.requestFullscreen();
  } catch {
    localError.value = "Vollbild ist in diesem Browser nicht verfügbar.";
  }
}
function fullscreenChanged() {
  fullscreen.value = document.fullscreenElement === root.value;
}
function pageHide() {
  blur();
  stopAudio();
  if (session.value)
    void fetch("/api/v1/tools/close_computer", {
      method: "POST",
      keepalive: true,
      headers: { ...headers(), "Content-Type": "application/json" },
      body: JSON.stringify({ session_id: session.value.session_id }),
    }).catch(() => {});
}
watch(control, (value) => {
  if (!value) {
    keyboard.value = false;
    cancelPointer();
  }
});
watch(micMode, stopMicrophone);
onMounted(() => {
  window.addEventListener("blur", blur);
  window.addEventListener("pagehide", pageHide);
  document.addEventListener("visibilitychange", hide);
  document.addEventListener("fullscreenchange", fullscreenChanged);
  void refreshInputs();
  navigator.mediaDevices?.addEventListener("devicechange", refreshInputs);
});
onBeforeUnmount(() => {
  cancelPointer();
  stopAudio();
  window.removeEventListener("blur", blur);
  window.removeEventListener("pagehide", pageHide);
  document.removeEventListener("visibilitychange", hide);
  document.removeEventListener("fullscreenchange", fullscreenChanged);
  navigator.mediaDevices?.removeEventListener("devicechange", refreshInputs);
});
</script>
<template>
  <section
    ref="root"
    class="screen"
    :class="{ overview, 'has-control': control }"
  >
    <div class="screen-top">
      <div>
        <span class="status-dot" :class="device.status" /><strong>{{
          device.name
        }}</strong
        ><small>{{ device.model }} · {{ device.status }}</small>
      </div>
      <span class="pill" :class="{ accent: control }">{{
        control
          ? keyboard
            ? "Tastatur & Maus aktiv"
            : "Steuerung bereit"
          : "Beobachten"
      }}</span>
    </div>
    <div v-if="!overview" class="toolbar control-toolbar">
      <Button
        :disabled="busy || !session || device.control_suspended"
        @click="takeControl"
        >{{
          busy
            ? "Bitte warten …"
            : control
              ? "Steuerung freigeben"
              : "Steuerung übernehmen"
        }}</Button
      >
      <Button
        variant="outline"
        :disabled="!control"
        :aria-expanded="pasteOpen"
        @click="pasteOpen ? (pasteOpen = false) : openPaste()"
        >Text / Paste</Button
      >
      <Button
        variant="ghost"
        :aria-pressed="nativeSize"
        @click="nativeSize = !nativeSize"
        >{{ nativeSize ? "An Fenster anpassen" : "Originalgröße 1:1" }}</Button
      >
      <Button variant="ghost" @click="toggleFullscreen">{{
        fullscreen ? "Vollbild verlassen" : "Vollbild ↗"
      }}</Button>
      <Button variant="destructive" @click="stop">■ Not-Stopp</Button>
    </div>
    <div v-if="!overview" class="audio-controls">
      <div class="audio-bar">
        <strong>Zielaudio</strong
        ><Button
          size="sm"
          variant="outline"
          :disabled="!session || !device.capabilities?.audio.from_target"
          :aria-pressed="audioPlaying"
          @click="audioPlaying || audioPending ? stopListening() : startAudio()"
          >{{
            audioPending
              ? "Verbinden abbrechen"
              : audioPlaying
                ? "Ton ausschalten"
                : "Ziel hören"
          }}</Button
        ><input
          v-model.number="volume"
          type="range"
          min="0"
          max="1"
          step="0.05"
          aria-label="Lautstärke"
        /><span>{{ Math.round(volume * 100) }} %</span
        ><meter :value="level" min="0" max="1" aria-label="Zielaudiopegel" />
      </div>
      <div class="audio-bar" :class="{ 'mic-live': mic }">
        <strong>Mikrofon</strong
        ><select v-model="micMode" aria-label="Mikrofonmodus">
          <option value="toggle">Ein / Aus</option>
          <option value="hold">Zum Sprechen halten</option>
        </select>
        <Button
          v-if="micMode === 'toggle'"
          size="sm"
          :variant="mic ? 'default' : 'outline'"
          :disabled="!control || !device.capabilities?.audio.to_target"
          :aria-pressed="mic"
          @click="mic || micPending ? stopMicrophone() : startAudio(true)"
          >{{
            micPending
              ? "Aktivierung abbrechen"
              : mic
                ? "● Mikrofon ausschalten"
                : "Mikrofon einschalten"
          }}</Button
        >
        <Button
          v-else
          size="sm"
          variant="outline"
          :disabled="!control || !device.capabilities?.audio.to_target"
          @pointerdown="hold"
          @pointerup="stopMicrophone"
          @pointercancel="stopMicrophone"
          @lostpointercapture="stopMicrophone"
          @keydown.space.prevent="startAudio(true)"
          @keyup.space.prevent="stopMicrophone"
          @keydown.enter.prevent="startAudio(true)"
          @keyup.enter.prevent="stopMicrophone"
          @blur="stopMicrophone"
          >{{
            mic || micPending
              ? "● Loslassen zum Stummschalten"
              : "Zum Sprechen halten"
          }}</Button
        >
        <select v-model="input" aria-label="Mikrofon">
          <option value="">Standardmikrofon</option>
          <option v-for="d in inputs" :key="d.deviceId" :value="d.deviceId">
            {{ d.label || "Mikrofon " + (inputs.indexOf(d) + 1) }}
          </option></select
        ><meter
          :value="micLevel"
          min="0"
          max="1"
          aria-label="Mikrofonpegel"
        /><span role="status">{{
          mic
            ? "● Mikrofon sendet an das Ziel"
            : micPending
              ? "Mikrofon wird aktiviert …"
              : "Mikrofon aus"
        }}</span>
      </div>
      <p class="audio-note" :role="audioError ? 'alert' : undefined">
        {{
          audioError ||
          device.capabilities?.audio.reason ||
          "Mikrofon wird bei Fensterwechsel oder Kontrollverlust ausgeschaltet. Zielaudio läuft unabhängig weiter."
        }}
      </p>
    </div>
    <div
      ref="viewport"
      class="display"
      :class="{ 'native-size': nativeSize, 'input-focused': keyboard }"
      :tabindex="overview ? -1 : 0"
      role="group"
      :aria-label="'KVM-Bildschirm ' + device.name"
      @focus="keyboard = control"
      @blur="unfocus"
      @keydown="key"
      @paste="pasted"
      @compositionend="
        control && $event.data && send([{ type: 'type', text: $event.data }])
      "
      @contextmenu.prevent
    >
      <img
        v-if="source"
        ref="image"
        :src="source"
        :alt="'Livebild ' + device.name"
        draggable="false"
        @pointerdown="down"
        @pointermove="move"
        @pointerup="up"
        @pointercancel="cancelPointer"
        @click="click"
        @wheel="wheel"
      />
      <div v-else class="empty-display">
        {{ error || "Verbindung wird aufgebaut …" }}
      </div>
      <span v-if="frameError" class="frame-error"
        >{{ frameError }} · Bild möglicherweise veraltet</span
      >
      <span v-if="device.driver_id === 'simulator'" class="simulated"
        >SIMULATOR</span
      >
    </div>
    <div class="screen-meta">
      <span
        >{{ frame ? `${frame.width} × ${frame.height}` : "Kein Bild" }} ·
        {{ device.keyboard_layout.toUpperCase() }}</span
      ><span>{{
        frameError
          ? "⚠ Kein aktuelles Bild"
          : last
            ? "● Live · Empfangszeit"
            : "Verbinden"
      }}</span>
    </div>
    <p v-if="error || localError" class="error" role="alert">
      {{ error || localError }}
    </p>
    <template v-if="!overview">
      <div class="control-hint">
        {{
          control
            ? "Klick ins Bild aktiviert Tastatur und Maus. Strg + Alt + Esc gibt den Tastaturfokus frei."
            : "Steuerung übernehmen, um Tastatur, Maus und Mikrofon zu verwenden."
        }}
      </div>
      <div v-if="device.lease && !control" class="notice">
        Steuerung belegt durch {{ device.lease.client_id }}
      </div>
      <div v-if="device.control_suspended" class="notice">
        Steuerung suspendiert.
        <Button
          variant="outline"
          @click="
            api(`/devices/${device.device_id}/resume`, 'POST', {})
              .then(() => emit('refresh'))
              .catch((e) => (localError = e.message))
          "
          >Entsperren</Button
        >
      </div>
      <div class="shortcut-bar" aria-label="Sondertasten">
        <span>Sondertasten</span>
        <Button
          v-for="s in [
            { label: 'Ctrl Alt Del', keys: ['CTRL', 'ALT', 'DELETE'] },
            { label: 'Alt Tab', keys: ['ALT', 'TAB'] },
            { label: 'Esc', keys: ['ESC'] },
            { label: 'Tab', keys: ['TAB'] },
            { label: 'Enter', keys: ['ENTER'] },
            { label: 'Super', keys: ['META'] },
            { label: 'F2', keys: ['F2'] },
            { label: 'F11', keys: ['F11'] },
          ]"
          :key="s.label"
          size="sm"
          variant="ghost"
          :disabled="!control"
          @click="shortcut(s.keys)"
          >{{ s.label }}</Button
        >
      </div>
      <div v-if="pasteOpen" class="paste-panel">
        <div class="paste-label">
          Text auf dem Ziel eingeben
          <small>{{ typing.length }} / 4096 Zeichen</small>
        </div>
        <textarea
          ref="pasteField"
          v-model="typing"
          aria-label="Text auf Ziel eingeben"
          rows="3"
          placeholder="Text oder mehrzeilige Befehle hier einfügen …"
          :disabled="!control || sending"
          @keydown.ctrl.enter.prevent="sendText"
          @keydown.meta.enter.prevent="sendText"
        />
        <div class="paste-actions">
          <Button
            variant="outline"
            :disabled="!control || sending"
            @click="clipboard"
            >Aus Zwischenablage</Button
          ><Button
            :disabled="!control || !typing || typing.length > 4096 || sending"
            @click="sendText"
            >{{ sending ? "Wird gesendet …" : "Senden" }}</Button
          ><small
            >Wird als Tastatureingabe gesendet, einschließlich Zeilenumbrüchen.
            Strg/⌘ + Enter zum Senden.</small
          >
        </div>
      </div>
    </template>
  </section>
</template>
