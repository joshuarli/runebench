/**
 * Runebench bridge for Pi's current extension/custom-tool API.
 *
 * Pi deliberately does not ship built-in MCP. This extension keeps one MCP
 * client per Pi process and exposes the rs-agent server's tools as native Pi
 * tools. The Harbor adapter replaces RUNEBENCH_MCP_SERVERS with the task's
 * actual MCP configuration before uploading this file into the container.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { createRequire } from "node:module";
import { writeFile } from "node:fs/promises";
import { Type } from "typebox";

// Resolve from the rs-sdk application package rather than from Pi's global
// install tree. The benchmark image owns the MCP SDK dependency at /app and
// its package exports determine the concrete client module path.
const requireFromApp = createRequire("/app/package.json");
const { Client } = requireFromApp("@modelcontextprotocol/sdk/client/index.js");
const { StdioClientTransport } = requireFromApp(
  "@modelcontextprotocol/sdk/client/stdio.js",
);

const RUNEBENCH_MCP_SERVERS = __RUNEBENCH_MCP_SERVERS__;

type McpServerConfig = (typeof RUNEBENCH_MCP_SERVERS)[number];
type McpClient = InstanceType<typeof Client>;

let clientPromise: Promise<McpClient> | undefined;
let resourceDocsPromise: Promise<string> | undefined;

function getServer(): McpServerConfig {
  const server = RUNEBENCH_MCP_SERVERS.find((candidate) => candidate.name === "rs-agent");
  if (!server) {
    throw new Error("Runebench Pi bridge requires an MCP server named rs-agent");
  }
  return server;
}

async function connect(): Promise<McpClient> {
  if (clientPromise) return clientPromise;

  clientPromise = (async () => {
    const server = getServer();
    if (server.transport !== "stdio" || !server.command) {
      throw new Error("Runebench Pi currently supports only stdio rs-agent MCP");
    }

    const transport = new StdioClientTransport({
      command: server.command,
      args: server.args ?? [],
      stderr: "pipe",
    });
    const client = new Client(
      { name: "runebench-pi", version: "0.1.0" },
      { capabilities: {} },
    );
    await client.connect(transport);
    return client;
  })();

  try {
    return await clientPromise;
  } catch (error) {
    clientPromise = undefined;
    throw error;
  }
}

function contentToText(content: unknown): string {
  if (!Array.isArray(content)) return JSON.stringify(content, null, 2);

  return content
    .map((part: any) => {
      if (part?.type === "text") return part.text ?? "";
      if (part?.type === "resource" && part.resource?.text) return part.resource.text;
      return JSON.stringify(part, null, 2);
    })
    .filter(Boolean)
    .join("\n");
}

function toolResult(result: any) {
  const text = contentToText(result?.content);
  return {
    content: [{ type: "text" as const, text: text || "(no output)" }],
    isError: Boolean(result?.isError),
    details: {
      mcpIsError: Boolean(result?.isError),
    },
  };
}

async function loadResourceDocs(): Promise<string> {
  if (resourceDocsPromise) return resourceDocsPromise;

  resourceDocsPromise = (async () => {
    const resources = await (await connect()).listResources();
    const parts: string[] = [];
    for (const resource of resources.resources ?? []) {
      const result = await (await connect()).readResource({ uri: resource.uri });
      const text = contentToText(result.contents);
      if (text) {
        // Keep the initial system prompt bounded while making the API available
        // immediately to small models that may not proactively read resources.
        parts.push(`### ${resource.name ?? resource.uri}\n${text.slice(0, 50_000)}`);
      }
    }
    return parts.join("\n\n");
  })();

  try {
    return await resourceDocsPromise;
  } catch (error) {
    resourceDocsPromise = undefined;
    throw error;
  }
}

export default function (pi: ExtensionAPI) {
  const executeCodeTool = {
    name: "execute_code",
    label: "rs-agent execute_code",
    description:
      "Execute TypeScript code on the RuneScape bot. The code runs in an async context with bot (BotActions) and sdk (BotSDK) globals.",
    promptSnippet:
      "execute TypeScript against the RuneScape bot through the rs-agent MCP bridge",
    promptGuidelines: [
      "Use bot_name=agent for the benchmark bot.",
      "Start with a small execute_code call, verify the result, then iterate.",
      "Use rs_agent_list_resources and rs_agent_read_resource to read the Bot/SDK API documentation before relying on unfamiliar methods.",
    ],
    parameters: Type.Object({
      bot_name: Type.String({
        description: "Bot name; use agent for the Runebench bot.",
      }),
      code: Type.String({
        description:
          "TypeScript code. Available globals are bot (BotActions) and sdk (BotSDK).",
      }),
      timeout: Type.Optional(
        Type.Number({
          description: "Execution timeout in minutes, from 0.1 to 60; default 2.",
          minimum: 0.1,
          maximum: 60,
        }),
      ),
    }),
    executionMode: "sequential" as const,
    execute: async (
      _toolCallId: string,
      params: { bot_name: string; code: string; timeout?: number },
      _signal: AbortSignal | undefined,
    ) => {
      const client = await connect();
      const result = await client.callTool({
        name: "execute_code",
        arguments: params,
      });
      return toolResult(result);
    },
  };

  pi.registerTool(executeCodeTool);

  pi.registerTool({
    name: "list_bots",
    label: "rs-agent list_bots",
    description: "List connected RuneScape bots.",
    parameters: Type.Object({}),
    executionMode: "sequential",
    execute: async () => {
      const result = await (await connect()).callTool({
        name: "list_bots",
        arguments: {},
      });
      return toolResult(result);
    },
  });

  pi.registerTool({
    name: "disconnect_bot",
    label: "rs-agent disconnect_bot",
    description: "Disconnect a connected RuneScape bot.",
    parameters: Type.Object({
      name: Type.String({ description: "Bot name to disconnect." }),
    }),
    executionMode: "sequential",
    execute: async (_toolCallId: string, params: { name: string }) => {
      const result = await (await connect()).callTool({
        name: "disconnect_bot",
        arguments: params,
      });
      return toolResult(result);
    },
  });

  pi.registerTool({
    name: "rs_agent_list_resources",
    label: "rs-agent list resources",
    description: "List the RuneScape Bot/SDK API documentation resources.",
    parameters: Type.Object({}),
    executionMode: "sequential",
    execute: async () => {
      const result = await (await connect()).listResources();
      return {
        content: [
          {
            type: "text" as const,
            text: JSON.stringify(result.resources, null, 2),
          },
        ],
        details: {},
      };
    },
  });

  pi.registerTool({
    name: "rs_agent_read_resource",
    label: "rs-agent read resource",
    description: "Read a RuneScape Bot/SDK API documentation resource by URI.",
    parameters: Type.Object({
      uri: Type.String({ description: "Resource URI returned by list_resources." }),
    }),
    executionMode: "sequential",
    execute: async (_toolCallId: string, params: { uri: string }) => {
      const result = await (await connect()).readResource({ uri: params.uri });
      return {
        content: [
          {
            type: "text" as const,
            text: contentToText(result.contents),
          },
        ],
        details: {},
      };
    },
  });

  pi.on("before_agent_start", async (event) => {
    let docs = "";
    try {
      docs = await loadResourceDocs();
    } catch {
      // The explicit resource tools remain available if startup loading fails.
    }

    // This is a small, non-secret runtime audit artifact. Harbor downloads the
    // agent log directory after the trial, and the live wrapper can read this
    // file from the container while the run is active. It confirms that Pi
    // loaded this extension and ran the system-prompt hook; the task prompt is
    // audited separately from pi.txt because Pi does not serialize its final
    // system prompt into the session JSONL.
    try {
      await writeFile(
        "/logs/agent/runebench-pi-bridge.json",
        JSON.stringify(
          {
            extensionLoaded: true,
            systemPromptHookRan: true,
            docsLoaded: Boolean(docs),
            docsChars: docs.length,
            mcpServer: "rs-agent",
            tools: [
              "execute_code",
              "list_bots",
              "disconnect_bot",
              "rs_agent_list_resources",
              "rs_agent_read_resource",
            ],
            timestamp: new Date().toISOString(),
          },
          null,
          2,
        ),
      );
    } catch {
      // The bridge remains usable if the optional audit artifact cannot be written.
    }

    return {
      systemPrompt: `${event.systemPrompt}

## Runebench game tools
The rs-agent MCP server is bridged into native Pi tools. Use execute_code with bot_name "agent" to control the game. Read the API documentation with rs_agent_list_resources followed by rs_agent_read_resource before using unfamiliar bot or sdk methods. Keep individual execute_code calls short while iterating; write longer strategies to a .ts file and run them with bash when appropriate.${docs ? `\n\n## Runebench API reference\n${docs}` : ""}`,
    };
  });

  pi.on("session_shutdown", async () => {
    if (clientPromise) {
      try {
        await (await clientPromise).close();
      } catch {
        // Harbor will tear down the container; MCP shutdown is best effort.
      }
      clientPromise = undefined;
    }
  });
}
