import { headers } from "./api";
import type { GatewayEvent } from "../shared/contracts";

export function listenEvents(
  receive: (event: GatewayEvent) => void,
  connection: (connected: boolean) => void,
) {
  let stopped = false,
    cursor = 0,
    instance = "",
    failures = 0;
  let controller: AbortController | undefined,
    retry: ReturnType<typeof setTimeout>;
  async function connect() {
    controller = new AbortController();
    let last = Date.now();
    const watchdog = setInterval(() => {
      if (Date.now() - last > 10000) controller?.abort();
    }, 1000);
    try {
      const response = await fetch("/api/v1/events", {
        headers: {
          ...headers(),
          "Last-Event-ID": String(cursor),
          "X-KVMHelm-Instance": instance,
        },
        signal: controller.signal,
      });
      if (!response.ok || !response.body)
        throw Error("Event connection failed");
      const current = response.headers.get("X-KVMHelm-Instance") ?? "";
      if (current !== instance) {
        cursor = 0;
        instance = current;
      }
      failures = 0;
      connection(true);
      const reader = response.body.getReader(),
        decoder = new TextDecoder();
      let buffer = "";
      while (!stopped) {
        const { done, value } = await reader.read();
        if (done) break;
        last = Date.now();
        buffer += decoder.decode(value, { stream: true });
        if (buffer.length > 1024 * 1024) throw Error("Event buffer limit");
        let n;
        while ((n = buffer.indexOf("\n\n")) >= 0) {
          const packet = buffer.slice(0, n);
          buffer = buffer.slice(n + 2);
          const line = packet.split("\n").find((x) => x.startsWith("data: "));
          if (!line) continue;
          const event: GatewayEvent = JSON.parse(line.slice(6));
          if (event.event_id <= cursor) continue;
          receive(event);
          cursor = event.event_id;
        }
      }
    } catch {
      /* Connection state and bounded retry replace silent failure. */
    } finally {
      controller.abort();
      clearInterval(watchdog);
      connection(false);
      if (!stopped)
        retry = setTimeout(
          connect,
          Math.min(10000, 250 * 2 ** Math.min(failures++, 6)),
        );
    }
  }
  void connect();
  return () => {
    stopped = true;
    clearTimeout(retry);
    controller?.abort();
  };
}
