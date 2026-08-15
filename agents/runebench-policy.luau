-- Runebench's task policy. This file declares model-visible tools and prompt
-- policy only; Rust owns the rs-agent MCP process and all side effects.
local granted_tools = {
    bash = true,
    edit = true,
    execute_code = true,
    list_bots = true,
    read = true,
    disconnect_bot = true,
    write = true,
}

-- These handlers run in a fresh hermetic Luau VM for each model tool call.
-- They can yield only an explicitly bound rs-agent capability. Rust validates
-- the method and the model-visible tool name against its MCP capability
-- manifest before it sends any JSON-RPC request to the world server.
local mcp_tool_call_handler = [[
return function(call)
    local result = coroutine.yield({
        kind = "capability",
        capability = "rs-agent",
        method = "tools.call",
        arguments_json = call.arguments_json,
    })
    return {
        content = result.content,
        details_json = result.details_json,
        is_error = result.is_error,
    }
end
]]

return {
    system_prompt_append = [[
## Runebench game tools
The rs-agent MCP server is bridged into native tools. Use execute_code to control the one Runebench bot; the bot name is fixed to "agent" and is added by the host. The complete Bot/SDK API reference is already included below as "Runebench API reference"; do not spend tool calls listing or reading MCP resources. Keep individual execute_code calls short while iterating, and verify state or XP after each meaningful action. Standalone Bun scripts do not receive the injected bot and sdk globals, so keep bot-control loops inside execute_code.
]],

    tools = {
        {
            name = "execute_code",
            description = "Execute TypeScript code on the RuneScape bot. The code runs in an async context with bot (BotActions) and sdk (BotSDK) globals.",
            capability = "rs-agent",
            execution_mode = "sequential",
            handler_source = mcp_tool_call_handler,
            schema_json = [[{"type":"object","properties":{"code":{"type":"string","description":"TypeScript code. Available globals are bot (BotActions) and sdk (BotSDK)."},"timeout_minutes":{"type":"number","description":"Optional execution timeout in minutes, from 0.1 to 60. Omit it for the host default; do not use seconds.","minimum":0.1,"maximum":60}},"required":["code"],"additionalProperties":false}]],
        },
        {
            name = "list_bots",
            description = "List connected RuneScape bots.",
            capability = "rs-agent",
            execution_mode = "sequential",
            handler_source = mcp_tool_call_handler,
            schema_json = [[{"type":"object","properties":{},"additionalProperties":false}]],
        },
        {
            name = "disconnect_bot",
            description = "Disconnect a connected RuneScape bot.",
            capability = "rs-agent",
            execution_mode = "sequential",
            handler_source = mcp_tool_call_handler,
            schema_json = [[{"type":"object","properties":{"name":{"type":"string","description":"Bot name to disconnect."}},"required":["name"],"additionalProperties":false}]],
        },
    },

    before_tool_call = function(call)
        if granted_tools[call.name] then
            return "allow"
        end
        return {
            action = "block",
            reason = "the Runebench policy did not grant this tool",
        }
    end,
}
