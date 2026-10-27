let context: AudioContext | undefined;
let ringingUntil = 0;

export function armNotificationSound() {
  if (typeof AudioContext === "undefined") return;
  try {
    context ??= new AudioContext();
    if (context.state === "suspended") void context.resume().catch(() => {});
  } catch {
    // A visible notification still works when the browser has no audio output.
  }
}

export function playNotificationSound() {
  const audio = context;
  if (!audio || audio.state !== "running") return;
  try {
    const now = audio.currentTime;
    if (now < ringingUntil) return;
    ringingUntil = now + 1.56;
    for (let pulse = 0; pulse < 6; pulse++) {
      const start = now + pulse * 0.26,
        tone = audio.createOscillator(),
        gain = audio.createGain();
      tone.type = "square";
      tone.frequency.setValueAtTime(pulse % 2 === 0 ? 880 : 1175, start);
      gain.gain.setValueAtTime(0.0001, start);
      gain.gain.linearRampToValueAtTime(0.55, start + 0.01);
      gain.gain.setValueAtTime(0.55, start + 0.16);
      gain.gain.exponentialRampToValueAtTime(0.0001, start + 0.22);
      gain.connect(audio.destination);
      tone.connect(gain);
      tone.onended = () => {
        tone.disconnect();
        gain.disconnect();
      };
      tone.start(start);
      tone.stop(start + 0.22);
    }
  } catch {
    // Notification delivery must not depend on optional sound playback.
  }
}

export function closeNotificationSound() {
  const audio = context;
  context = undefined;
  ringingUntil = 0;
  if (audio && audio.state !== "closed") void audio.close().catch(() => {});
}
