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
export async function api(
  path: string,
  method = "GET",
  body?: unknown,
): Promise<any> {
  const r = await fetch("/api/v1" + path, {
    method,
    headers: {
      "Content-Type": "application/json",
      ...headers(),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const data = await r.json();
  if (!r.ok) throw new Error(data.error?.message ?? "Request failed");
  return data;
}
export async function tool(name: string, args: unknown) {
  const result = await api("/tools/" + name, "POST", args);
  if (result.isError) {
    const e = new Error(
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
