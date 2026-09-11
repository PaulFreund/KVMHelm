import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import {
  toolSchemas,
  type ToolName,
  type ToolEnvelope,
} from "../shared/contracts.js";
import { type ToolResult, Core } from "./core.js";
import { type Identity } from "./auth.js";
export const imagePresentation =
  "When the user asks to see the screen or a visual result, explicitly display the returned image in your user-visible response using your client's supported image attachment/rendering mechanism. An image in a tool result may be visible only to you. Forward the actual image content; do not replace it with a description or invent a URL. If your client cannot display it, say so; never claim an image is shown unless you included it. Do not upload screenshots to an external service merely to display them.";
export const descriptions: Record<ToolName, string> = {
  list_computers: "List available computers and capabilities.",
  open_computer:
    "Open an observe or exclusive control session and get its screen; control requires request_id. " +
    imagePresentation,
  computer:
    "Run ordered actions; return the screen by default. The device applies a minimum delay after the last input before observing (default 30 ms); later observations do not restart it. Use wait(duration_ms) between actions when needed. On timing failures increase the relevant wait by 50 ms until visually stable; after 5 verified successes reduce by 25 ms toward 0. If a lower value fails, restore the last stable value and retry lowering after 5 successes. Sent/ACK is not visual success. Never replay uncertain input automatically.",
  computer_screenshot:
    "Get the current screen without input; max_age_ms allows a cached image. " +
    imagePresentation,
  computer_control:
    "Acquire, renew or release exclusive control of your session.",
  close_computer: "Close your session and release control.",
};
export const instructions =
  "Use list_computers, then open_computer. Observe, act in short batches, observe again. Coordinates are pixels in the returned image. Copy frame_id, view_id and input_revision from the latest frame into computer.reference. After acquiring control separately, get a new screenshot. Use a new request_id per operation; reuse it only for an identical retry. Never blindly repeat uncertain input. Close sessions when finished. " +
  imagePresentation;
export function pack(result: ToolResult): ToolEnvelope {
  const { images = [], ...body } = result;
  const structuredContent = {
    ...body,
    frames: images.length ? images.map((f) => f.info) : (body.frames ?? []),
  };
  return {
    isError: !result.ok,
    structuredContent,
    content: [
      { type: "text" as const, text: JSON.stringify(structuredContent) },
      ...images.map((f) => ({
        type: "image" as const,
        mimeType: f.info.mime_type,
        data: f.data.toString("base64"),
        annotations: {
          audience: ["user" as const, "assistant" as const],
          priority: 1,
        },
      })),
      ...(images.length
        ? [
            {
              type: "text" as const,
              text: imagePresentation,
              annotations: { audience: ["assistant" as const] },
            },
          ]
        : []),
    ],
  };
}
export function mcpServer(core: Core, identity: Identity) {
  const server = new McpServer(
    { name: "KVMHelm", version: "0.1.0" },
    { instructions },
  );
  for (const name of Object.keys(toolSchemas) as ToolName[])
    server.registerTool(
      name,
      {
        description: descriptions[name],
        inputSchema: toolSchemas[name],
        outputSchema: z
          .object({ ok: z.boolean(), frames: z.array(z.unknown()) })
          .passthrough(),
        annotations: {
          readOnlyHint: ["list_computers", "computer_screenshot"].includes(
            name,
          ),
          destructiveHint: name === "computer",
          idempotentHint: [
            "list_computers",
            "computer_screenshot",
            "computer",
            "computer_control",
          ].includes(name),
          openWorldHint: false,
        },
      },
      async (args: any) => pack(await core.tool(identity, name, args)),
    );
  return server;
}
