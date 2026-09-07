import { z } from "zod";
export const scopes = [
  "devices:read",
  "devices:manage",
  "video:read",
  "input:write",
  "audio:read",
  "audio:write",
  "plugins:manage",
  "tokens:manage",
  "control:stop",
] as const;
export const profileSchema = z.enum(["auto", "active", "overview", "warm"]);
export const deviceSchema = z
  .object({
    device_id: z.string().uuid().optional(),
    name: z.string().min(1).max(100),
    driver_id: z.string().default("pikvm-v4"),
    model: z.string().default("PiKVM v4"),
    firmware: z.string().default("unknown"),
    address: z.string().url().optional(),
    secret_ref: z.string().optional(),
    tls: z
      .object({
        ca_ref: z.string().optional(),
        fingerprint: z.string().optional(),
        insecure: z.boolean().optional(),
      })
      .strict()
      .default({}),
    tags: z.array(z.string().max(40)).max(50).default([]),
    enabled: z.boolean().default(true),
    media_profile: profileSchema.default("auto"),
    minimum_action_latency_ms: z.number().int().min(0).max(5000).default(30),
    keyboard_layout: z.enum(["us", "de"]).default("us"),
    transport: z.enum(["network", "simulator"]).default("network"),
    revision: z.number().int().default(0),
  })
  .strict();
export type Device = z.infer<typeof deviceSchema> & { device_id: string };
export interface Capabilities {
  video: {
    snapshot: boolean;
    stream: boolean;
    formats: string[];
    signal: "present" | "absent" | "unknown";
  };
  mouse: {
    absolute: boolean;
    relative: boolean;
    buttons: string[];
    horizontal_scroll: boolean;
  };
  keyboard: {
    hid: boolean;
    layouts: string[];
    text_method: string;
    characters: string;
  };
  audio: {
    from_target: boolean;
    to_target: boolean;
    formats: string[];
    reason?: string;
  };
  timebase: { source_timestamp: boolean; uncertainty_ms: number | null };
  extensions: Record<string, unknown>;
}
const point = z
  .object({
    x: z.number().int().nonnegative(),
    y: z.number().int().nonnegative(),
  })
  .strict();
export const actionSchema = z.discriminatedUnion("type", [
  z
    .object({
      type: z.literal("click"),
      ...point.shape,
      button: z.enum(["left", "right", "middle"]).default("left"),
    })
    .strict(),
  z.object({ type: z.literal("double_click"), ...point.shape }).strict(),
  z.object({ type: z.literal("move"), ...point.shape }).strict(),
  z
    .object({ type: z.literal("drag"), path: z.array(point).min(2).max(256) })
    .strict(),
  z
    .object({
      type: z.literal("scroll"),
      ...point.shape,
      scroll_x: z.number().int().min(-1000).max(1000),
      scroll_y: z.number().int().min(-1000).max(1000),
    })
    .strict(),
  z
    .object({
      type: z.literal("keypress"),
      keys: z.array(z.string().min(1).max(40)).min(1).max(6),
    })
    .strict(),
  z.object({ type: z.literal("type"), text: z.string().max(4096) }).strict(),
  z
    .object({
      type: z.literal("wait"),
      duration_ms: z.number().int().min(0).max(5000).default(100),
    })
    .strict(),
  z.object({ type: z.literal("screenshot") }).strict(),
]);
export type ComputerAction = z.infer<typeof actionSchema>;
export const referenceSchema = z
  .object({
    frame_id: z.string(),
    view_id: z.string(),
    input_revision: z.number().int(),
  })
  .strict();
export const toolSchemas = {
  list_computers: z.object({ tags: z.array(z.string()).optional() }).strict(),
  open_computer: z
    .object({
      computer_id: z.string(),
      mode: z.enum(["observe", "control"]),
      request_id: z.string().min(1).max(128).optional(),
      profile: profileSchema.optional(),
    })
    .strict(),
  computer: z
    .object({
      session_id: z.string(),
      request_id: z.string().min(1).max(128),
      reference: referenceSchema,
      actions: z.array(actionSchema).min(1).max(32),
      observation: z
        .object({
          mode: z.enum(["after_actions", "none"]).default("after_actions"),
          timeout_ms: z.number().int().min(50).max(10000).default(1500),
        })
        .strict()
        .optional(),
    })
    .strict(),
  computer_screenshot: z
    .object({
      session_id: z.string(),
      max_age_ms: z.number().int().min(0).max(5000).default(0),
      timeout_ms: z.number().int().min(50).max(10000).default(1500),
    })
    .strict(),
  computer_control: z
    .object({
      session_id: z.string(),
      request_id: z.string().min(1).max(128),
      operation: z.enum(["acquire", "renew", "release"]),
    })
    .strict(),
  close_computer: z.object({ session_id: z.string() }).strict(),
};
export type ToolName = keyof typeof toolSchemas;
export interface FrameInfo {
  frame_id: string;
  view_id: string;
  device_id: string;
  connection_generation: number;
  input_revision: number;
  width: number;
  height: number;
  mime_type: "image/jpeg" | "image/png";
  source_timestamp: number | null;
  received_at_monotonic_ms: number;
  source_time_uncertainty_ms: number | null;
  received_age_ms: number;
  estimated_capture_age_ms: number | null;
  freshness_basis: "source_time" | "calibrated_pipeline" | "receive_time_only";
  after_action: "verified" | "estimated" | "unverified" | "not_applicable";
  signal: "present" | "absent" | "unknown";
  stale: boolean;
}
export interface Frame {
  data: Buffer;
  info: FrameInfo;
}
export interface AudioChunk {
  device_id: string;
  format: "pcm_s16le";
  sample_rate: number;
  channels: number;
  sequence: number;
  timestamp_ms: number;
  connection_generation: number;
  discontinuity: boolean;
  data: Buffer;
}
export interface ActionContext {
  signal: AbortSignal;
  assertControl(): void;
  width: number;
  height: number;
}
export interface DriverFrame {
  data: Buffer;
  width: number;
  height: number;
  mime_type: "image/jpeg" | "image/png";
  source_timestamp?: number;
  signal: "present" | "absent" | "unknown";
}
export interface KvmDriver {
  onDisconnected?: () => void;
  connect(signal: AbortSignal): Promise<void>;
  disconnect(): Promise<void>;
  capabilities(): Promise<Capabilities>;
  snapshot(signal: AbortSignal): Promise<DriverFrame>;
  subscribeVideo?(signal: AbortSignal): AsyncIterable<DriverFrame>;
  execute(
    action: ComputerAction,
    context: ActionContext,
  ): Promise<"sent" | "acknowledged">;
  validate(actions: ComputerAction[], width: number, height: number): void;
  releaseAllInputs(): Promise<void>;
  subscribeAudio?(signal: AbortSignal): AsyncIterable<AudioChunk>;
  sendAudio?(chunk: AudioChunk, context: ActionContext): Promise<void>;
}
export class GatewayError extends Error {
  constructor(
    public code: string,
    message = code,
    public details: Record<string, unknown> = {},
  ) {
    super(message);
  }
}
export const fail = (code: string, message?: string): never => {
  throw new GatewayError(code, message);
};
