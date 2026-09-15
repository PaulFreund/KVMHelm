let context: AudioContext | undefined;

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
    const now = audio.currentTime,
      gain = audio.createGain();
    gain.gain.setValueAtTime(0.0001, now);
    gain.gain.exponentialRampToValueAtTime(0.14, now + 0.015);
    gain.gain.exponentialRampToValueAtTime(0.0001, now + 0.48);
    gain.connect(audio.destination);
    for (const [frequency, offset] of [
      [880, 0],
      [1175, 0.18],
    ] as const) {
      const tone = audio.createOscillator();
      tone.type = "sine";
      tone.frequency.setValueAtTime(frequency, now + offset);
      tone.connect(gain);
      tone.start(now + offset);
      tone.stop(now + offset + 0.16);
    }
  } catch {
    // Notification delivery must not depend on optional sound playback.
  }
}

export function closeNotificationSound() {
  const audio = context;
  context = undefined;
  if (audio && audio.state !== "closed") void audio.close().catch(() => {});
}
