-- Runebench's task policy. This file declares model-visible tools and prompt
-- policy only; Rust owns the rs-agent MCP process and all side effects.
local granted_tools = {
    bash = true,
    edit = true,
    execute_code = true,
    list_bots = true,
    read = true,
    disconnect_bot = true,
    rs_agent_list_resources = true,
    rs_agent_read_resource = true,
    write = true,
}

return {
    system_prompt_append = [[
## Runebench game tools
The rs-agent MCP server is bridged into native tools. Use execute_code with bot_name "agent" to control the game. Read the API documentation with rs_agent_list_resources followed by rs_agent_read_resource before using unfamiliar bot or sdk methods. Keep individual execute_code calls short while iterating; write longer strategies to a .ts file and run them with bash when appropriate.
]],

    tools = {
        {
            name = "execute_code",
            description = "Execute TypeScript code on the RuneScape bot. The code runs in an async context with bot (BotActions) and sdk (BotSDK) globals.",
            capability = "rs-agent",
            execution_mode = "sequential",
            schema_json = [[{"type":"object","properties":{"bot_name":{"type":"string","description":"Bot name; use agent for the Runebench bot."},"code":{"type":"string","description":"TypeScript code. Available globals are bot (BotActions) and sdk (BotSDK)."},"timeout":{"type":"number","description":"Execution timeout in minutes, from 0.1 to 60; default 2.","minimum":0.1,"maximum":60}},"required":["bot_name","code"],"additionalProperties":false}]],
        },
        {
            name = "list_bots",
            description = "List connected RuneScape bots.",
            capability = "rs-agent",
            execution_mode = "sequential",
            schema_json = [[{"type":"object","properties":{},"additionalProperties":false}]],
        },
        {
            name = "disconnect_bot",
            description = "Disconnect a connected RuneScape bot.",
            capability = "rs-agent",
            execution_mode = "sequential",
            schema_json = [[{"type":"object","properties":{"name":{"type":"string","description":"Bot name to disconnect."}},"required":["name"],"additionalProperties":false}]],
        },
        {
            name = "rs_agent_list_resources",
            description = "List the RuneScape Bot/SDK API documentation resources.",
            capability = "rs-agent",
            execution_mode = "sequential",
            schema_json = [[{"type":"object","properties":{},"additionalProperties":false}]],
        },
        {
            name = "rs_agent_read_resource",
            description = "Read a RuneScape Bot/SDK API documentation resource by URI.",
            capability = "rs-agent",
            execution_mode = "sequential",
            schema_json = [[{"type":"object","properties":{"uri":{"type":"string","description":"Resource URI returned by list_resources."}},"required":["uri"],"additionalProperties":false}]],
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
