import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport, StreamableHTTPError } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import type { CallToolRequest } from "@modelcontextprotocol/sdk/types.js";

/** The stdio connection outlives disposable daemon transport sessions. */
export class McpUpstream {
  private connection?: { client: Client; transport: StreamableHTTPClientTransport };
  private connecting?: Promise<NonNullable<McpUpstream["connection"]>>;
  private closed = false;
  constructor(private url: URL, private token: () => Promise<string>) {}
  async connect() {
    if (this.closed) throw Error("Bridge closed");
    if (this.connection) return this.connection;
    if (this.connecting) return this.connecting;
    this.connecting = (async () => {
      const token = await this.token();
      const client = new Client({ name: "KVMHelm stdio bridge", version: "0.1.0" });
      const transport = new StreamableHTTPClientTransport(this.url, {
        requestInit: { headers: { Authorization: `Bearer ${token}` } },
        fetch: (url, init) => fetch(url, { ...init, signal: init?.signal ? AbortSignal.any([init.signal, AbortSignal.timeout(40000)]) : AbortSignal.timeout(40000) }),
      });
      try {
        await client.connect(transport, { timeout: 5000 });
        if (this.closed) throw Error("Bridge closed");
        return this.connection = { client, transport };
      } catch (e) { await client.close().catch(() => {}); throw e; }
    })();
    try { return await this.connecting; } finally { this.connecting = undefined; }
  }
  private async invalidate(c: NonNullable<McpUpstream["connection"]>) {
    if (this.connection === c) this.connection = undefined;
    await c.client.close().catch(() => {});
  }
  private recoverable(e: any) {
    return e instanceof StreamableHTTPError ? e.code === 404 || (e.code ?? 0) >= 500
      : e instanceof TypeError || e?.code === -32001 || e?.code === -32000 || e?.name === "AbortError" || e?.name === "TimeoutError";
  }
  async listTools() {
    const c = await this.connect();
    try { return await c.client.listTools(); }
    catch (e) {
      if (!this.recoverable(e)) throw e;
      await this.invalidate(c);
      return (await this.connect()).client.listTools();
    }
  }
  async callTool(params: CallToolRequest["params"]) {
    let c: Awaited<ReturnType<McpUpstream["connect"]>> | undefined;
    try {
      c = await this.connect();
      return await c.client.callTool(params);
    } catch (e) {
      if (!this.recoverable(e)) throw e;
      if (c) await this.invalidate(c);
      if (params.name === "list_computers") {
        try { return await (await this.connect()).client.callTool(params); }
        catch (retryError) { if (!this.recoverable(retryError)) throw retryError; }
      }
      const body = { ok: false, frames: [], error: {
        code: "MCP_CONNECTION_LOST",
        message: "Daemon connection lost. This operation was not automatically repeated; input may already have executed. Use list_computers, then open a new session and observe before deciding further input. Old sessions and references may be invalid.",
      } };
      return { isError: true, structuredContent: body, content: [{ type: "text" as const, text: JSON.stringify(body) }] };
    }
  }
  async close() {
    this.closed = true;
    const c = this.connection;
    this.connection = undefined;
    if (c) {
      await c.transport.terminateSession().catch(() => {});
      await c.client.close().catch(() => {});
    }
  }
}
