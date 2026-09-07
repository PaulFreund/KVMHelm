export const client = crypto.randomUUID();
const tokenKey = "kvmhelm.access-token";
let accessToken = localStorage.getItem(tokenKey) ?? "";
export function rememberToken(value: string) {
  if (value) localStorage.setItem(tokenKey, value);
  else localStorage.removeItem(tokenKey);
  accessToken = value;
}
let csrf = "";
export function setCsrf(value: string) {
  csrf = value;
}
interface ReadResponses {
  "/devices": DeviceView[];
  "/plugins": PluginView[];
  "/ui-extensions": ExtensionView[];
  "/tokens": TokenView[];
  "/me": MeView;
  "/preferences": Preferences;
  "/diagnostics": Record<string, unknown>;
}
export function api<P extends keyof ReadResponses>(
  path: P,
): Promise<ReadResponses[P]>;
export function api<T = unknown>(
  path: string,
  method?: string,
  body?: unknown,
): Promise<T>;
export async function api<T = unknown>(
  path: string,
  method = "GET",
  body?: unknown,
): Promise<T> {
  const r = await fetch("/api/v1" + path, {
    method,
    headers: {
      "Content-Type": "application/json",
      ...headers(),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(40000),
  });
  const data = await r.json().catch(() => ({}));
  if (!r.ok)
    throw new ApiError(
      data.error?.code ?? "HTTP_ERROR",
      data.error?.message ?? "Request failed",
      r.status,
      Number(r.headers.get("retry-after") ?? 0) * 1000,
    );
  return data;
}
export async function tool(
  name: ToolName,
  args: unknown,
): Promise<ToolEnvelope> {
  const result = await api<ToolEnvelope>("/tools/" + name, "POST", args);
  if (result.isError) {
    const e = new ApiError(
      result.structuredContent.error?.code ?? "EXECUTION_UNKNOWN",
      result.structuredContent.error?.message ?? "Operation failed",
    );
    Object.assign(e, { result: result.structuredContent });
    throw e;
  }
  return result;
}
export function headers() {
  return {
    "X-KVMHelm-Client": client,
    "X-CSRF-Token": csrf,
    ...(accessToken ? { Authorization: `Bearer ${accessToken}` } : {}),
  };
}
import type {
  ToolEnvelope,
  ToolName,
  DeviceView,
  PluginView,
  ExtensionView,
  TokenView,
  MeView,
  Preferences,
} from "../shared/contracts";
export class ApiError extends Error {
  constructor(
    public code: string,
    message: string,
    public status = 0,
    public retryAfter = 0,
  ) {
    super(message);
  }
}
