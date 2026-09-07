# MCP recovery

## Incident, 2026-09-07

The existing native Codex task returned HTTP 404 `Unknown MCP session` after the gateway was restarted for the PiKVM/TLS fix. The stdio process remained alive, but forwarded its old Streamable HTTP session ID forever. The HTTP daemon itself was healthy and newly initialized clients worked. A one-hour idle transport expiry can trigger the same fault. This is distinct from a PiKVM being offline.

The ad hoc SDK fallback inside the task's node REPL also failed; its environment and initialization errors are not evidence that the gateway was down. Use the supported Node CLI bridge for this integration.

## Fix

`server/mcp-upstream.ts` keeps the host's stdio transport separate from the replaceable HTTP client. Unknown-session responses and connection failures invalidate the upstream client. Reconnection is shared across concurrent callers, bounded by an initialization timeout, and reads the protected PAT file again. `tools/list` and `list_computers` can retry once. Input, control, open/close, and screenshot operations are never automatically replayed after transport failure; the host receives structured `MCP_CONNECTION_LOST` instructions to list, reopen and observe. Device session IDs, leases and image references are deliberately not restored across daemon restarts. A response lost after input may mean input already executed.

The interop regression leaves the same stdio process connected while the HTTP server is restarted twice and temporarily unavailable. It verifies no automatic control replay, automatic read recovery, fresh images, and a surviving downstream connection. No real hardware inputs are used.

## Rollout and operation

Already running Node bridge processes cannot load an edited module automatically. The configuration was updated, but the existing Codex task was retested and still used its old process. Reload MCP connections in the host, or restart Codex once, to load this bridge update. A gateway restart alone does not replace the host-owned bridge. Subsequent gateway restarts are handled by the updated bridge.

The daemon is still a manually launched POC process, not an automatically restarted Windows service. Reconnection cannot bring an unavailable daemon back; once it is running again the bridge can recover. Before planned updates, finish active input batches and release control. After deployment, test an already-open MCP client across a server restart rather than testing only fresh clients.
