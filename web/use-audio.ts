import { ref, watch, type Ref } from "vue";
import { api } from "./api";
import type { SessionView } from "../shared/contracts";
export function useAudio(
  session: Ref<SessionView | undefined>,
  control: Ref<boolean>,
  isDisposed: () => boolean,
) {
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
    if (generation !== audioGeneration || isDisposed()) return;
    if (withMic && !control.value) return;
    try {
      audioContext = new AudioContext({ sampleRate: 48000 });
      await audioContext.resume();
      if (generation !== audioGeneration || isDisposed()) return;
      const ticket = await api<{ ticket: string }>(
        `/sessions/${session.value!.session_id}/audio`,
        "POST",
        { microphone: withMic },
      );
      if (generation !== audioGeneration || isDisposed()) return;
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
        if (generation !== audioGeneration || isDisposed()) return;
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
        if (generation !== audioGeneration || isDisposed() || !control.value) {
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
  watch(control, (value) => {
    if (!value) void stopAudio();
  });
  return {
    audioPlaying,
    mic,
    volume,
    audioError,
    inputs,
    input,
    level,
    startAudio,
    stopAudio,
  };
}
