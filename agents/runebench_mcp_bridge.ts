/**
 * One-shot, capability-scoped stdio bridge for Runebench's rs-agent MCP server.
 *
 * The Rust host clears the child's environment before invoking this file. This
 * bridge has no model credential or access to a general MCP configuration: its
 * only authority is the benchmark's fixed rs-agent server.
 */

import { createRequire } from "node:module";

const requireFromApp = createRequire("/app/package.json");
const { Client } = requireFromApp("@modelcontextprotocol/sdk/client/index.js");
const { StdioClientTransport } = requireFromApp(
  "@modelcontextprotocol/sdk/client/stdio.js",
);

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

async function withClient<T>(operation: (client: InstanceType<typeof Client>) => Promise<T>): Promise<T> {
  const transport = new StdioClientTransport({
    command: "bun",
    args: ["run", "/app/mcp/server.ts"],
    stderr: "pipe",
  });
  const client = new Client(
    { name: "runebench-pi-agent-core", version: "0.1.0" },
    { capabilities: {} },
  );
  await client.connect(transport);
  try {
    return await operation(client);
  } finally {
    await client.close().catch(() => undefined);
  }
}

async function docs(): Promise<void> {
  const text = await withClient(async (client) => {
    const resources = await client.listResources();
    const parts: string[] = [];
    for (const resource of resources.resources ?? []) {
      const result = await client.readResource({ uri: resource.uri });
      const content = contentToText(result.contents);
      if (content) parts.push(`### ${resource.name ?? resource.uri}\n${content.slice(0, 50_000)}`);
    }
    return parts.join("\n\n");
  });
  process.stdout.write(text);
}

async function callTool(name: string, argumentsJson: string): Promise<void> {
  const argumentsValue = JSON.parse(argumentsJson) as Record<string, unknown>;
  const result = await withClient((client) =>
    client.callTool({ name, arguments: argumentsValue }),
  );
  process.stdout.write(contentToText(result.content) || "(no output)");
  if (result.isError) process.exitCode = 2;
}

async function listResources(): Promise<void> {
  const resources = await withClient((client) => client.listResources());
  process.stdout.write(JSON.stringify(resources.resources ?? [], null, 2));
}

async function readResource(uri: string): Promise<void> {
  const result = await withClient((client) => client.readResource({ uri }));
  process.stdout.write(contentToText(result.contents));
}

async function main(): Promise<void> {
  const [operation, ...args] = process.argv.slice(2);
  switch (operation) {
    case "docs":
      if (args.length !== 0) throw new Error("docs takes no arguments");
      await docs();
      return;
    case "call":
      if (args.length !== 2) throw new Error("call requires tool name and JSON arguments");
      await callTool(args[0], args[1]);
      return;
    case "list-resources":
      if (args.length !== 0) throw new Error("list-resources takes no arguments");
      await listResources();
      return;
    case "read-resource":
      if (args.length !== 1) throw new Error("read-resource requires a URI");
      await readResource(args[0]);
      return;
    default:
      throw new Error("expected docs, call, list-resources, or read-resource");
  }
}

main().catch((error) => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
});
