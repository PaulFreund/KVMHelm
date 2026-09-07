import { randomUUID } from "node:crypto";
import { setTimeout as delay } from "node:timers/promises";
import { EventEmitter } from "node:events";
import {
  type Frame,
  type DriverFrame,
  type KvmDriver,
  GatewayError,
} from "../shared/contracts.js";
export class Media extends EventEmitter {
  latest?: Frame;
  view = randomUUID();
  generation = 0;
  revision = 0;
  consumers = new Map<string, string>();
  strategy: "snapshot" | "mjpeg" = "snapshot";
  metrics = {
    snapshot_ms: [] as number[],
    frame_intervals_ms: [] as number[],
    frames: 0,
    dropped: 0,
    bytes: 0,
  };
  private pending?: Promise<Frame>;
  private controller?: AbortController;
  private running?: Promise<void>;
  private lastShape = "";
  private lastReceived = 0;
  private stopped = false;
  private paused = false;
  private benchmarkResult?: Record<string, unknown>;
  private benchmarking?: Promise<Record<string, unknown>>;
  constructor(
    public device_id: string,
    private driver: KvmDriver,
    private budget = 8 * 1024 * 1024,
  ) {
    super();
  }
  reconnect() {
    this.generation++;
    this.view = randomUUID();
    this.latest = undefined;
    this.lastShape = "";
  }
  retain(id: string, profile: string) {
    this.consumers.set(id, profile);
    this.ensure();
  }
  release(id: string) {
    this.consumers.delete(id);
    if (!this.consumers.size) this.controller?.abort();
  }
  ingest(raw: DriverFrame, revision = this.revision) {
    if (raw.data.length > this.budget || raw.width * raw.height > 16777216)
      throw new GatewayError("RESOURCE_LIMIT", "Frame budget exceeded");
    const shape = `${raw.width}:${raw.height}:${raw.mime_type}`;
    if (this.lastShape !== shape) {
      this.view = randomUUID();
      this.lastShape = shape;
    }
    const now = performance.now();
    if (this.lastReceived) {
      this.metrics.frame_intervals_ms.push(now - this.lastReceived);
      if (this.metrics.frame_intervals_ms.length > 128)
        this.metrics.frame_intervals_ms.shift();
    }
    this.lastReceived = now;
    this.metrics.frames++;
    this.metrics.bytes = raw.data.length;
    const f: Frame = {
      data: raw.data,
      info: {
        frame_id: randomUUID(),
        view_id: this.view,
        device_id: this.device_id,
        connection_generation: this.generation,
        input_revision: revision,
        width: raw.width,
        height: raw.height,
        mime_type: raw.mime_type,
        source_timestamp: raw.source_timestamp ?? null,
        received_at_monotonic_ms: now,
        source_time_uncertainty_ms:
          raw.source_timestamp === undefined ? null : 0,
        received_age_ms: 0,
        estimated_capture_age_ms: null,
        freshness_basis:
          raw.source_timestamp === undefined
            ? "receive_time_only"
            : "source_time",
        after_action: "not_applicable",
        signal: raw.signal,
        stale: false,
      },
    };
    this.latest = f;
    this.emit("frame", f);
    return f;
  }
  age(f: Frame, after = false): Frame {
    const age = performance.now() - f.info.received_at_monotonic_ms;
    return {
      data: f.data,
      info: {
        ...f.info,
        received_age_ms: age,
        estimated_capture_age_ms:
          f.info.source_timestamp === null
            ? null
            : performance.now() - f.info.source_timestamp,
        stale: age > 1500,
        after_action: after
          ? f.info.source_timestamp === null
            ? "unverified"
            : "verified"
          : "not_applicable",
      },
    };
  }
  private capture() {
    if (!this.pending) {
      const start = performance.now();
      const revision = this.revision,
        generation = this.generation;
      this.pending = this.driver
        .snapshot(AbortSignal.timeout(5000))
        .then((f) => {
          this.metrics.snapshot_ms.push(performance.now() - start);
          if (this.metrics.snapshot_ms.length > 64)
            this.metrics.snapshot_ms.shift();
          if (generation !== this.generation)
            throw new GatewayError("VIEW_CHANGED");
          return this.ingest(f, revision);
        })
        .finally(() => {
          this.pending = undefined;
        });
    }
    return this.pending;
  }
  async screenshot(maxAge = 0, timeout = 1500, after = false): Promise<Frame> {
    const cutoff = performance.now();
    if (
      !after &&
      this.latest &&
      this.latest.info.input_revision === this.revision &&
      cutoff - this.latest.info.received_at_monotonic_ms <= maxAge &&
      this.latest.info.signal !== "absent"
    )
      return this.age(this.latest);
    let timer: NodeJS.Timeout | undefined;
    let listener: ((f: Frame) => void) | undefined;
    try {
      const frame = await Promise.race([
        this.strategy === "mjpeg" && this.running
          ? new Promise<Frame>((resolve) => {
              listener = (f) => {
                if (f.info.received_at_monotonic_ms >= cutoff) resolve(f);
              };
              this.on("frame", listener);
            })
          : (async () => {
              // A capture already in flight may predate the action/image deadline.
              if (
                this.pending &&
                (after || this.latest?.info.input_revision !== this.revision)
              )
                await this.pending;
              return this.capture();
            })().then(async (f) =>
              (after && f.info.received_at_monotonic_ms < cutoff) ||
              f.info.input_revision !== this.revision
                ? this.capture()
                : f,
            ),
        new Promise<never>((_, reject) => {
          timer = setTimeout(
            () =>
              reject(
                new GatewayError(
                  "FRAME_TIMEOUT",
                  "No fresh complete frame received",
                ),
              ),
            timeout,
          );
        }),
      ]);
      if (frame.info.signal === "absent") throw new GatewayError("NO_SIGNAL");
      return this.age(frame, after);
    } finally {
      clearTimeout(timer);
      if (listener) this.off("frame", listener);
    }
  }
  private ensure() {
    if (this.running || this.stopped || this.paused || !this.consumers.size)
      return;
    this.controller = new AbortController();
    const signal = this.controller.signal;
    this.running = this.loop(signal).finally(() => {
      this.running = undefined;
      if (this.consumers.size && !this.stopped)
        setTimeout(() => this.ensure(), 500);
    });
    this.running.catch(() => {});
  }
  private async loop(signal: AbortSignal) {
    let failures = 0;
    while (!signal.aborted) {
      try {
        if (this.strategy === "mjpeg" && this.driver.subscribeVideo) {
          for await (const frame of this.driver.subscribeVideo(signal)) {
            if (signal.aborted) break;
            this.ingest(frame);
          }
        } else {
          await this.capture();
          const fps = [...this.consumers.values()].some((p) => p === "active")
            ? 15
            : [...this.consumers.values()].some((p) => p === "overview")
              ? 5
              : 1;
          await delay(1000 / fps, undefined, { signal });
        }
        failures = 0;
      } catch (e) {
        if (signal.aborted) return;
        this.emit("fault", e);
        if (this.strategy === "mjpeg") this.strategy = "snapshot";
        await delay(Math.min(10000, 250 * 2 ** failures++), undefined, {
          signal,
        }).catch(() => {});
      }
    }
  }
  async benchmark() {
    if (this.benchmarking) return this.benchmarking;
    this.benchmarking = this.measure().finally(() => {
      this.benchmarking = undefined;
    });
    return this.benchmarking;
  }
  private async measure(): Promise<Record<string, unknown>> {
    const snapshot: number[] = [];
    for (let i = 0; i < 5; i++) {
      const t = performance.now();
      await this.capture();
      snapshot.push(performance.now() - t);
    }
    let stream: number | null = null;
    if (this.driver.subscribeVideo) {
      const c = new AbortController();
      const timeout = setTimeout(() => c.abort(), 2500);
      try {
        const t = performance.now();
        let count = 0;
        for await (const f of this.driver.subscribeVideo(c.signal)) {
          this.ingest(f);
          if (++count === 5) {
            stream = (performance.now() - t) / count;
            break;
          }
        }
      } catch {
      } finally {
        c.abort();
        clearTimeout(timeout);
      }
    }
    const avg = snapshot.reduce((a, b) => a + b, 0) / snapshot.length;
    const chosen = stream !== null && stream < avg * 0.8 ? "mjpeg" : "snapshot";
    if (chosen !== this.strategy) {
      this.strategy = chosen;
      this.controller?.abort();
    }
    this.benchmarkResult = {
      snapshot_mean_ms: avg,
      stream_frame_interval_ms: stream,
      strategy: this.strategy,
      basis: "local receive timing; not target capture latency",
      hysteresis: 0.2,
    };
    return this.benchmarkResult;
  }
  async pause() {
    this.paused = true;
    this.controller?.abort();
    await this.running;
  }
  resume() {
    this.paused = false;
    this.ensure();
  }
  async stop() {
    this.stopped = true;
    this.consumers.clear();
    this.controller?.abort();
    await this.running;
    this.latest = undefined;
  }
}
