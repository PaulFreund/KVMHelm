import { ref, onMounted, onBeforeUnmount, watch } from "vue";
import { api, tool, headers, ApiError } from "./api";
import type {
  DeviceView,
  FrameInfo,
  SessionView,
  ToolEnvelope,
  ComputerAction,
} from "../shared/contracts";
import { InputQueue } from "./input-queue";

export function useComputer(
  device: () => DeviceView,
  overview: () => boolean,
  refresh: () => void,
) {
  const session = ref<SessionView>(),
    frame = ref<FrameInfo>(),
    source = ref("");
  const rotating = ref(false);
  const busy = ref(false),
    control = ref(false),
    last = ref(0),
    error = ref(""),
    frameError = ref("");
  let disposed = false,
    working = false,
    epoch = 0,
    failures = 0;
  let timer: ReturnType<typeof setTimeout>,
    healthTimer: ReturnType<typeof setInterval>;
  let pollAbort: AbortController | undefined;
  let pendingFrameError = "";
  const queue = new InputQueue(execute);
  const isDisposed = () => disposed;
  function cancelInputs() {
    epoch++;
    queue.cancel();
    pollAbort?.abort();
  }
  function loseControl() {
    control.value = false;
    cancelInputs();
  }
  function received(info: FrameInfo) {
    last.value = Date.now() - Math.max(0, info.received_age_ms);
    pendingFrameError = "";
    frameError.value = info.stale ? "Kein aktuelles Bild" : "";
    failures = 0;
  }
  function replaceSource(value: string) {
    if (source.value.startsWith("blob:")) URL.revokeObjectURL(source.value);
    source.value = value;
  }
  function show(result: ToolEnvelope) {
    const pictures = result.content.filter((x) => x.type === "image");
    const picture = pictures.at(-1),
      info = result.structuredContent.frames.at(-1);
    if (picture && info) {
      replaceSource(`data:${picture.mimeType};base64,${picture.data}`);
      frame.value = info;
      received(info);
    }
    if (result.structuredContent.session)
      session.value = result.structuredContent.session;
    if (
      session.value &&
      result.structuredContent.journal_remaining !== undefined
    )
      session.value.journal_remaining =
        result.structuredContent.journal_remaining;
  }
  async function open(mode: "observe" | "control" = "observe") {
    const result = await tool("open_computer", {
      computer_id: device().device_id,
      mode,
      ...(mode === "control" ? { request_id: crypto.randomUUID() } : {}),
      profile: overview() ? "overview" : "active",
    });
    if (disposed) {
      if (result.structuredContent.session)
        await tool("close_computer", {
          session_id: result.structuredContent.session.session_id,
        });
      return;
    }
    show(result);
  }
  function handleFailure(e: unknown) {
    const code = e instanceof ApiError ? e.code : "CONNECTION_LOST";
    if (["SESSION_LOST", "LEASE_EXPIRED"].includes(code)) {
      session.value = undefined;
      loseControl();
    }
    if (
      [
        "LEASE_REVOKED",
        "CONTROL_SUSPENDED",
        "AUTH_FAILED",
        "FORBIDDEN",
        "CONNECTION_LOST",
      ].includes(code)
    )
      loseControl();
    return e instanceof Error ? e.message : "Verbindung unterbrochen";
  }
  async function poll() {
    if (disposed) return;
    let retry = overview() ? 200 : 80;
    if (!working && !busy.value) {
      const version = epoch;
      try {
        if (!session.value) await open();
        else {
          const id = session.value.session_id;
          const controller = new AbortController();
          pollAbort = controller;
          const response = await fetch(`/api/v1/sessions/${id}/frame`, {
            headers: headers(),
            signal: AbortSignal.any([
              controller.signal,
              AbortSignal.timeout(5000),
            ]),
          });
          if (!response.ok) {
            const body = await response.json().catch(() => ({}));
            throw new ApiError(
              body.error?.code ?? "HTTP_ERROR",
              body.error?.message ?? "Bildabruf fehlgeschlagen",
              response.status,
              Number(response.headers.get("retry-after") ?? 0) * 1000,
            );
          }
          const info: FrameInfo = JSON.parse(
            response.headers.get("X-KVMHelm-Frame")!,
          );
          const blob = await response.blob();
          if (
            disposed ||
            epoch !== version ||
            session.value?.session_id !== id ||
            working ||
            busy.value
          )
            return;
          replaceSource(URL.createObjectURL(blob));
          frame.value = info;
          received(info);
        }
      } catch (e) {
        if (disposed || epoch !== version) return;
        pendingFrameError = handleFailure(e);
        retry = Math.max(
          Math.min(10000, 250 * 2 ** Math.min(failures++, 6)),
          e instanceof ApiError ? e.retryAfter : 0,
        );
        if (
          e instanceof ApiError &&
          ["AUTH_FAILED", "FORBIDDEN"].includes(e.code)
        ) {
          frameError.value = pendingFrameError;
          retry = 30000;
        }
      } finally {
        pollAbort = undefined;
        if (!disposed) timer = setTimeout(poll, retry);
      }
    } else timer = setTimeout(poll, retry);
  }
  async function acquire() {
    if (!session.value || busy.value) return;
    busy.value = true;
    cancelInputs();
    try {
      const releasing = control.value;
      const result = await tool("computer_control", {
        session_id: session.value.session_id,
        request_id: crypto.randomUUID(),
        operation: releasing ? "release" : "acquire",
      });
      control.value = !releasing;
      if (result.structuredContent.session_closed) session.value = undefined;
      if (control.value && session.value)
        show(
          await tool("computer_screenshot", {
            session_id: session.value.session_id,
          }),
        );
      error.value = "";
      refresh();
    } catch (e) {
      error.value = handleFailure(e);
      loseControl();
    } finally {
      busy.value = false;
    }
  }
  async function execute(actions: ComputerAction[]) {
    if (!control.value || !frame.value || !session.value || disposed)
      throw new Error("Steuerung nicht verfügbar.");
    working = true;
    pollAbort?.abort();
    const version = ++epoch;
    try {
      if (session.value.journal_remaining <= 8) {
        rotating.value = true;
        await tool("close_computer", { session_id: session.value.session_id });
        session.value = undefined;
        if (epoch !== version || disposed) return false;
        await open();
        if (epoch !== version || disposed) {
          if (session.value)
            await tool("close_computer", {
              session_id: (session.value as SessionView).session_id,
            });
          session.value = undefined;
          return false;
        }
        const rotated = session.value as SessionView | undefined;
        if (!rotated) return false;
        await tool("computer_control", {
          session_id: rotated.session_id,
          request_id: crypto.randomUUID(),
          operation: "acquire",
        });
        if (epoch !== version || disposed) {
          await tool("close_computer", { session_id: rotated.session_id });
          session.value = undefined;
          return false;
        }
        show(
          await tool("computer_screenshot", { session_id: rotated.session_id }),
        );
      }
      if (!session.value || !frame.value || epoch !== version) return false;
      const f = frame.value;
      const result = await tool("computer", {
        session_id: (session.value as SessionView).session_id,
        request_id: crypto.randomUUID(),
        reference: {
          frame_id: f.frame_id,
          view_id: f.view_id,
          input_revision: f.input_revision,
        },
        actions,
      });
      if (!disposed && epoch === version) {
        show(result);
        error.value = "";
      }
    } catch (e) {
      error.value = handleFailure(e);
      queue.cancel();
      if (
        !session.value ||
        (e instanceof ApiError &&
          ["RESOURCE_LIMIT", "CONTROL_BUSY"].includes(e.code))
      )
        loseControl();
      // Never replay uncertain input. Preserve paste drafts on failure.
      throw e;
    } finally {
      working = false;
      rotating.value = false;
    }
  }
  async function send(actions: ComputerAction[]): Promise<boolean> {
    if (!control.value || disposed) return false;
    const ok = await queue.enqueue(actions);
    if (!ok && control.value && !error.value)
      error.value =
        "Eingabe abgebrochen oder Warteschlange voll. Bitte erneut beobachten.";
    return ok;
  }
  async function releaseInputs() {
    cancelInputs();
    if (control.value && session.value)
      await api(
        `/sessions/${session.value.session_id}/release-inputs`,
        "POST",
        {},
      ).catch(() => {});
  }
  async function stop() {
    loseControl();
    try {
      await api(`/devices/${device().device_id}/stop`, "POST", {});
      refresh();
    } catch (e) {
      error.value = handleFailure(e);
    }
  }
  watch(
    () => device().control_suspended,
    (value) => {
      if (value) loseControl();
    },
  );
  watch(
    () => device().lease?.session_id,
    (id, old) => {
      if (old === session.value?.session_id && id !== old) loseControl();
    },
  );
  onMounted(() => {
    void poll();
    healthTimer = setInterval(() => {
      if (pendingFrameError && (!last.value || Date.now() - last.value >= 2000))
        frameError.value = pendingFrameError;
    }, 200);
  });
  onBeforeUnmount(() => {
    disposed = true;
    loseControl();
    clearTimeout(timer);
    clearInterval(healthTimer);
    replaceSource("");
    if (session.value)
      void tool("close_computer", {
        session_id: session.value.session_id,
      }).catch(() => {});
  });
  return {
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
  };
}
