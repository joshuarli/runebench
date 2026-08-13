import { expect, test } from "bun:test";

import { isProviderErrorEvent } from "./run-pi-live.ts";

test("does not classify MCP timeout results or tool arguments as provider failures", () => {
  expect(
    isProviderErrorEvent({
      type: "message_end",
      message: {
        role: "toolResult",
        content: [{ type: "text", text: "MCP error -32001: Request timed out" }],
      },
    }),
  ).toBe(false);

  expect(
    isProviderErrorEvent({
      type: "message_end",
      message: {
        role: "assistant",
        stopReason: "toolUse",
        content: [{ type: "toolCall", arguments: { timeout: 2 } }],
      },
    }),
  ).toBe(false);
});

test("classifies explicit Pi/provider failures", () => {
  expect(
    isProviderErrorEvent({
      type: "message_end",
      message: { role: "assistant", stopReason: "error" },
    }),
  ).toBe(true);

  expect(
    isProviderErrorEvent({
      type: "provider_error",
      error: { status: 429, message: "rate limit exceeded" },
    }),
  ).toBe(true);
});
