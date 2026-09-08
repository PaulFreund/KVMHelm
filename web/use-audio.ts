import { ref, watch, type Ref } from "vue";
import { api } from "./api";
import type { SessionView } from "../shared/contracts";

/** Listening and microphone have independent resources and cancellation lifetimes. */
export function useAudio(
  session: Ref<SessionView | undefined>,
  control: Ref<boolean>,
  isDisposed: () => boolean,
  rotating: Ref<boolean>,
) {
  const audioPlaying = ref(false),
    audioPending = ref(false),
    mic = ref(false),
    micPending = ref(false);
  const volume = ref(0.8),
    audioError = ref(""),
    inputs = ref<MediaDeviceInfo[]>([]),
    input = ref(""),
    level = ref(0),
    micLevel = ref(0);
  type Channel = {
    generation: number;
    timeout?: ReturnType<typeof setTimeout>;
    socket?: WebSocket;
    context?: AudioContext;
    stream?: MediaStream;
    processor?: ScriptProcessorNode;
    gain?: GainNode;
  };
  const listening: Channel = { generation: 0 },
    speaking: Channel = { generation: 0 };
  function clear(c: Channel) {
    c.generation++;
    clearTimeout(c.timeout);
    const socket = c.socket;
    c.socket = undefined;
    if (socket) {
      socket.onclose = null;
      socket.close();
    }
    c.processor?.disconnect();
    c.processor = undefined;
    c.stream?.getTracks().forEach((t) => t.stop());
    c.stream = undefined;
    const context = c.context;
    c.context = undefined;
    c.gain = undefined;
    void context?.close().catch(() => {});
  }
  let resumeListening = false,
    resumeMicrophone = false;
  function stopListening() {
    resumeListening = false;
    clear(listening);
    audioPlaying.value = audioPending.value = false;
    level.value = 0;
  }
  function stopMicrophone() {
    resumeMicrophone = false;
    clear(speaking);
    mic.value = micPending.value = false;
    micLevel.value = 0;
  }
  function stopAudio() {
    stopListening();
    stopMicrophone();
  }
  async function refreshInputs() {
    try {
      inputs.value = (
        (await navigator.mediaDevices?.enumerateDevices()) ?? []
      ).filter((d) => d.kind === "audioinput");
    } catch {
      /* Device labels are optional before permission. */
    }
  }
  async function startAudio(withMic = false) {
    const c = withMic ? speaking : listening;
    if (!session.value || isDisposed() || (withMic && !control.value)) return;
    if (
      withMic
        ? mic.value || micPending.value
        : audioPlaying.value || audioPending.value
    )
      return;
    const generation = ++c.generation,
      sessionId = session.value.session_id;
    const current = () =>
      c.generation === generation &&
      !isDisposed() &&
      session.value?.session_id === sessionId &&
      (!withMic || control.value);
    const pending = withMic ? micPending : audioPending;
    pending.value = true;
    audioError.value = "";
    c.timeout = setTimeout(() => {
      if (current()) {
        audioError.value =
          "Audioverbindung dauert zu lange. Bitte erneut einschalten.";
        withMic ? stopMicrophone() : stopListening();
      }
    }, 15000);
    try {
      const context = new AudioContext({ sampleRate: 48000 });
      c.context = context;
      await context.resume();
      if (!current()) return;
      if (withMic) {
        if (!navigator.mediaDevices?.getUserMedia)
          throw Error("Mikrofon benötigt HTTPS oder localhost.");
        const stream = await navigator.mediaDevices.getUserMedia({
          audio: {
            deviceId: input.value ? { exact: input.value } : undefined,
            channelCount: 1,
            sampleRate: 48000,
            echoCancellation: true,
            noiseSuppression: true,
            autoGainControl: true,
          },
        });
        if (!current()) {
          stream.getTracks().forEach((t) => t.stop());
          return;
        }
        c.stream = stream;
        stream.getTracks().forEach((t) => {
          t.onended = () => {
            if (current()) {
              audioError.value = "Mikrofon getrennt. Bitte erneut einschalten.";
              stopMicrophone();
            }
          };
        });
        void refreshInputs();
      }
      const ticket = await api<{ ticket: string }>(
        `/sessions/${sessionId}/audio`,
        "POST",
        { microphone: withMic },
      );
      if (!current()) return;
      const socket = new WebSocket(
        `${location.protocol === "https:" ? "wss:" : "ws:"}//${location.host}/api/v1/audio`,
      );
      c.socket = socket;
      socket.binaryType = "arraybuffer";
      let scheduled = 0;
      socket.onopen = () => {
        if (!current()) {
          socket.close();
          return;
        }
        socket.send(JSON.stringify({ ticket: ticket.ticket }));
      };
      const ready = () => {
        if (!current() || !pending.value) return;
        clearTimeout(c.timeout);
        pending.value = false;
        if (withMic) {
          const source = context.createMediaStreamSource(c.stream!);
          const processor = context.createScriptProcessor(4096, 1, 1);
          c.processor = processor;
          processor.onaudioprocess = (e) => {
            if (
              !current() ||
              !mic.value ||
              socket.readyState !== WebSocket.OPEN ||
              socket.bufferedAmount > 96000
            )
              return;
            const samples = e.inputBuffer.getChannelData(0),
              pcm = new Int16Array(samples.length);
            let peak = 0;
            for (let n = 0; n < samples.length; n++) {
              pcm[n] = Math.max(-1, Math.min(1, samples[n])) * 32767;
              peak = Math.max(peak, Math.abs(samples[n]));
            }
            micLevel.value = peak;
            socket.send(pcm.buffer);
          };
          const silent = context.createGain();
          silent.gain.value = 0;
          source.connect(processor);
          processor.connect(silent).connect(context.destination);
          mic.value = true;
        } else {
          c.gain = context.createGain();
          c.gain.gain.value = volume.value;
          c.gain.connect(context.destination);
          audioPlaying.value = true;
        }
      };
      socket.onclose = () => {
        if (current()) {
          audioError.value =
            "Audioverbindung beendet. Bitte erneut einschalten.";
          withMic ? stopMicrophone() : stopListening();
        }
      };
      socket.onerror = () => {
        if (current()) audioError.value = "Audioverbindung fehlgeschlagen.";
      };
      socket.onmessage = (e) => {
        if (!current()) return;
        if (typeof e.data === "string") {
          try {
            const message = JSON.parse(e.data);
            if (message.ready && withMic) ready();
            else scheduled = context.currentTime;
          } catch {
            /* Ignore unknown metadata. */
          }
          return;
        }
        if (withMic) return;
        ready();
        if (!audioPlaying.value) return;
        clearTimeout(c.timeout);
        c.timeout = setTimeout(() => {
          if (current()) {
            audioError.value =
              "Kein Zielaudio empfangen. Bitte erneut einschalten.";
            stopListening();
          }
        }, 10000);
        const data = new Int16Array(e.data),
          length = Math.floor(data.length / 2);
        if (!length) return;
        const buffer = context.createBuffer(2, length, 48000);
        let peak = 0;
        for (let ch = 0; ch < 2; ch++) {
          const target = buffer.getChannelData(ch);
          for (let n = 0; n < length; n++) {
            target[n] = data[n * 2 + ch] / 32768;
            peak = Math.max(peak, Math.abs(target[n]));
          }
        }
        level.value = peak;
        const source = context.createBufferSource();
        source.buffer = buffer;
        source.connect(c.gain!);
        if (scheduled > context.currentTime + 0.3)
          scheduled = context.currentTime;
        scheduled = Math.max(scheduled, context.currentTime);
        source.start(scheduled);
        scheduled += length / 48000;
      };
    } catch (e) {
      if (!current()) return;
      audioError.value = (e as Error).message;
      withMic ? stopMicrophone() : stopListening();
    }
  }
  watch(volume, (value) => {
    if (listening.gain) listening.gain.gain.value = value;
  });
  watch(control, (value) => {
    if (!value) stopMicrophone();
  });
  watch(
    [() => session.value?.session_id, rotating],
    ([id, renewing], [old, wasRenewing]) => {
      if (renewing) {
        const listen =
          resumeListening || audioPlaying.value || audioPending.value;
        const speak = resumeMicrophone || mic.value || micPending.value;
        stopAudio();
        resumeListening = listen;
        resumeMicrophone = speak && control.value;
      } else if (wasRenewing) {
        const listen = resumeListening,
          speak = resumeMicrophone;
        resumeListening = resumeMicrophone = false;
        if (id && !isDisposed()) {
          if (listen) void startAudio();
          if (speak && control.value) void startAudio(true);
        }
      } else if (id !== old) stopAudio();
    },
    { flush: "sync" },
  );
  watch(input, () => {
    if (mic.value || micPending.value) {
      stopMicrophone();
      audioError.value = "Mikrofon gewechselt. Bitte erneut einschalten.";
    }
  });
  return {
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
  };
}
