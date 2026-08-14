//! Runebench's concrete world host for the provider-free Pi Rust core.
//!
//! This binary intentionally owns OpenRouter transport and the rs-agent MCP
//! process. `pi-agent-core` remains provider/world agnostic, while the adjacent
//! Luau policy only declares prompts and explicit tool-policy decisions.

use pi_agent_core::default_tools::CommandEnvironment;
use pi_agent_core::error::{HookError, ToolError};
use pi_agent_core::event::{AgentEvent, AgentEventKind, EventObserver, ObserverFuture};
use pi_agent_core::hooks::{AfterToolCall, BeforeToolCall, ContextEnvelope, HookSet, NextTurn};
use pi_agent_core::profile::PiDefaultCodingProfile;
use pi_agent_core::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use pi_agent_core::state::{
    AssistantToolCall, Message, ModelDescriptor, SerializedJson, StopReason, ToolCallId, Usage,
};
use pi_agent_core::tool::{
    AgentTool, ToolCall, ToolContext, ToolExecutionMode, ToolFuture, ToolResult, ToolUpdateSink,
};
use pi_agent_core::{Agent, DefaultCodingTools};
use pi_agent_luau::{LuaPolicy, LuaPolicyHookSet, PolicyTool};
use pi_agent_protocol::{JsonNumber, JsonValue};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

const OPENROUTER_COMPLETIONS_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const DEFAULT_SHELL_PATH: &str = "/root/.bun/bin:/usr/local/bin:/usr/bin:/bin";
const RS_AGENT_TOOL_NAMES: [&str; 5] = [
    "execute_code",
    "list_bots",
    "disconnect_bot",
    "rs_agent_list_resources",
    "rs_agent_read_resource",
];

struct Args {
    model: String,
    instruction: String,
    workspace: PathBuf,
    policy: PathBuf,
    mcp_bridge: PathBuf,
    log_jsonl: PathBuf,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut values = BTreeMap::<String, String>::new();
        let mut arguments = env::args().skip(1);
        while let Some(flag) = arguments.next() {
            if !flag.starts_with("--") {
                return Err(format!("unexpected positional argument {flag:?}"));
            }
            let value = arguments
                .next()
                .ok_or_else(|| format!("missing value for {flag}"))?;
            if values.insert(flag.clone(), value).is_some() {
                return Err(format!("argument {flag} must not repeat"));
            }
        }
        let required = |flag: &str| {
            values
                .get(flag)
                .filter(|value| !value.is_empty())
                .cloned()
                .ok_or_else(|| format!("missing required argument {flag}"))
        };
        Ok(Self {
            model: required("--model")?,
            instruction: required("--instruction")?,
            workspace: PathBuf::from(required("--workspace")?),
            policy: PathBuf::from(required("--policy")?),
            mcp_bridge: PathBuf::from(required("--mcp-bridge")?),
            log_jsonl: PathBuf::from(required("--log-jsonl")?),
        })
    }

    fn usage() -> &'static str {
        "usage: runebench-pi-agent --model <openrouter/model> --instruction <text> --workspace <dir> --policy <file.luau> --mcp-bridge <file.ts> --log-jsonl <file>"
    }
}

/// Provider protocol conversion stays outside the core and the Lua VM.
#[derive(Debug, Default)]
struct OpenAiContextHook;

impl HookSet for OpenAiContextHook {
    fn before_tool_call(&self, _call: &ToolCall) -> Result<BeforeToolCall, HookError> {
        Ok(BeforeToolCall::Allow)
    }

    fn after_tool_call(
        &self,
        _call: &ToolCall,
        _result: &ToolResult,
    ) -> Result<AfterToolCall, HookError> {
        Ok(AfterToolCall::default())
    }

    fn transform_context(&self, context: ContextEnvelope) -> Result<ContextEnvelope, HookError> {
        Ok(context)
    }

    fn convert_to_llm(&self, context: ContextEnvelope) -> Result<String, HookError> {
        let messages = context
            .messages
            .iter()
            .map(openai_message)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(format!("[{}]", messages.join(",")))
    }

    fn should_stop_after_turn(&self, _context: &ContextEnvelope) -> Result<bool, HookError> {
        Ok(false)
    }

    fn prepare_next_turn(&self, _context: ContextEnvelope) -> Result<NextTurn, HookError> {
        Ok(NextTurn::default())
    }
}

fn openai_message(message: &Message) -> Result<String, HookError> {
    match message {
        Message::User { content, .. } => Ok(format!(
            "{{\"role\":\"user\",\"content\":{}}}",
            json_string(content)
        )),
        Message::Assistant {
            content,
            tool_calls,
            ..
        } => {
            let calls = tool_calls
                .iter()
                .map(|call| {
                    format!(
                        "{{\"id\":{},\"type\":\"function\",\"function\":{{\"name\":{},\"arguments\":{}}}}}",
                        json_string(call.id.as_str()),
                        json_string(&call.name),
                        json_string(call.arguments.as_str()),
                    )
                })
                .collect::<Vec<_>>();
            let content = if content.is_empty() {
                "null".to_owned()
            } else {
                json_string(content)
            };
            Ok(format!(
                "{{\"role\":\"assistant\",\"content\":{content},\"tool_calls\":[{}]}}",
                calls.join(",")
            ))
        }
        Message::ToolResult {
            tool_call_id,
            content,
            ..
        } => Ok(format!(
            "{{\"role\":\"tool\",\"tool_call_id\":{},\"content\":{}}}",
            json_string(tool_call_id.as_str()),
            json_string(content),
        )),
    }
}

#[derive(Clone, Debug, Default)]
struct UsageTotals(Arc<Mutex<Usage>>);

impl UsageTotals {
    fn add(&self, usage: Usage) {
        let mut totals = self.0.lock().expect("usage totals lock poisoned");
        totals.input_tokens =
            Some(totals.input_tokens.unwrap_or(0) + usage.input_tokens.unwrap_or(0));
        totals.output_tokens =
            Some(totals.output_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0));
        totals.reasoning_tokens =
            Some(totals.reasoning_tokens.unwrap_or(0) + usage.reasoning_tokens.unwrap_or(0));
    }

    fn snapshot(&self) -> Usage {
        self.0.lock().expect("usage totals lock poisoned").clone()
    }
}

/// Blocking OpenRouter adapter owned by this Runebench executable.
struct OpenRouterProvider {
    api_key: String,
    model: String,
    usage: UsageTotals,
}

impl OpenRouterProvider {
    fn response_stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> ModelStream {
        if cancellation.is_cancelled() {
            return ModelStream {
                events: vec![ModelStreamEvent::End(StopReason::Cancelled)],
            };
        }
        match self.complete(request) {
            Ok((mut events, usage)) => {
                self.usage.add(usage.clone());
                let terminal = events
                    .pop()
                    .expect("OpenRouter parser always returns a terminal event");
                events.push(ModelStreamEvent::Usage(usage));
                events.push(terminal);
                ModelStream { events }
            }
            Err(message) => ModelStream {
                events: vec![ModelStreamEvent::Error { message }],
            },
        }
    }

    fn complete(&self, request: ModelRequest) -> Result<(Vec<ModelStreamEvent>, Usage), String> {
        let payload = openrouter_payload(&self.model, request)?;
        let config_path = write_curl_config(&self.api_key)?;
        let output_result = (|| {
            let mut child = Command::new("/usr/bin/curl")
                .arg("--silent")
                .arg("--show-error")
                .arg("--config")
                .arg(&config_path)
                .arg("--write-out")
                .arg("\n%{http_code}")
                // The provider key must not be inherited by curl descendants.
                .env_clear()
                .env("PATH", DEFAULT_SHELL_PATH)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|error| format!("could not start OpenRouter transport: {error}"))?;
            child
                .stdin
                .as_mut()
                .ok_or_else(|| "OpenRouter transport did not expose request stdin".to_owned())?
                .write_all(payload.as_bytes())
                .map_err(|error| format!("could not write OpenRouter request: {error}"))?;
            child
                .wait_with_output()
                .map_err(|error| format!("OpenRouter transport did not settle: {error}"))
        })();
        // The config carries the Authorization header. It is mode 0600 and is
        // removed before any provider body/error can reach an agent log.
        let _ = fs::remove_file(&config_path);
        let output = output_result?;
        if !output.status.success() {
            return Err(format!(
                "OpenRouter transport failed before a provider response: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let (body, status) = split_curl_status(&output.stdout)?;
        parse_openrouter_response(body, status)
    }
}

fn write_curl_config(api_key: &str) -> Result<PathBuf, String> {
    if api_key.contains(['\n', '\r']) {
        return Err("OPENROUTER_API_KEY must not contain line breaks".to_owned());
    }
    let quoted_key = api_key.replace('\\', "\\\\").replace('"', "\\\"");
    let path = env::temp_dir().join(format!(
        "pi-agent-core-openrouter-{}-{}.curl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| format!("cannot create OpenRouter transport config: {error}"))?
            .as_nanos(),
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&path)
        .map_err(|error| format!("cannot create private OpenRouter transport config: {error}"))?;
    write!(
        file,
        "connect-timeout = 10\nmax-time = 90\nrequest = \"POST\"\nurl = \"{OPENROUTER_COMPLETIONS_URL}\"\nheader = \"Content-Type: application/json\"\nheader = \"Authorization: Bearer {quoted_key}\"\ndata-binary = \"@-\"\n"
    )
    .map_err(|error| format!("cannot write OpenRouter transport config: {error}"))?;
    Ok(path)
}

impl ModelProvider for OpenRouterProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        let stream = self.response_stream(request, cancellation);
        Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
    }
}

fn openrouter_payload(model: &str, request: ModelRequest) -> Result<String, String> {
    let context = request.context.trim();
    let Some(inner) = context
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    else {
        return Err("OpenAI conversion did not produce a JSON message array".to_owned());
    };
    let system = format!(
        "{{\"role\":\"system\",\"content\":{}}}",
        json_string(&request.system_prompt)
    );
    let messages = if inner.trim().is_empty() {
        format!("[{system}]")
    } else {
        format!("[{system},{inner}]")
    };
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            let schema = tool
                .schema
                .to_json_string()
                .map_err(|error| format!("cannot serialize tool schema: {error}"))?;
            Ok(format!(
                "{{\"type\":\"function\",\"function\":{{\"name\":{},\"description\":{},\"parameters\":{schema}}}}}",
                json_string(&tool.name),
                json_string(&tool.description),
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut payload = format!(
        "{{\"model\":{},\"messages\":{messages},\"temperature\":0,\"max_tokens\":4096,\"stream\":false",
        json_string(model)
    );
    if !tools.is_empty() {
        payload.push_str(",\"tools\":[");
        payload.push_str(&tools.join(","));
        payload.push(']');
    }
    payload.push('}');
    Ok(payload)
}

fn split_curl_status(bytes: &[u8]) -> Result<(&[u8], u16), String> {
    let output = std::str::from_utf8(bytes)
        .map_err(|_| "OpenRouter transport returned non-UTF-8 output".to_owned())?;
    let (body, status) = output
        .rsplit_once('\n')
        .ok_or_else(|| "OpenRouter transport did not report an HTTP status".to_owned())?;
    let status = status
        .trim()
        .parse::<u16>()
        .map_err(|_| "OpenRouter transport reported an invalid HTTP status".to_owned())?;
    Ok((body.as_bytes(), status))
}

fn parse_openrouter_response(
    bytes: &[u8],
    http_status: u16,
) -> Result<(Vec<ModelStreamEvent>, Usage), String> {
    let response_text =
        std::str::from_utf8(bytes).map_err(|_| "OpenRouter returned non-UTF-8 JSON".to_owned())?;
    let response = JsonValue::parse(response_text)
        .map_err(|_| "OpenRouter returned a non-JSON response".to_owned())?;
    if let Some(error) = response.get("error") {
        return Err(openrouter_error(error, http_status));
    }
    if !(200..300).contains(&http_status) {
        return Err(format!(
            "OpenRouter returned HTTP {http_status} without a completion"
        ));
    }
    let choice = array_field(&response, "choices")?
        .first()
        .ok_or_else(|| "OpenRouter response did not contain a completion choice".to_owned())?;
    let message = object_field(choice, "message")?;
    let mut events = Vec::new();
    if let Some(content) = optional_string(message.get("content"))? {
        if !content.is_empty() {
            events.push(ModelStreamEvent::TextDelta(content.to_owned()));
        }
    }
    let mut has_tool_calls = false;
    if let Some(calls) = optional_array(message.get("tool_calls"))? {
        for (index, call) in calls.iter().enumerate() {
            let call_object = as_object(call, "OpenRouter tool call")?;
            let id = optional_string(call_object.get("id"))?
                .filter(|id| !id.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("openrouter-call-{index}"));
            let function = object_field(call, "function")?;
            let name = required_string(function.get("name"), "OpenRouter tool call name")?;
            let arguments = required_string(
                function.get("arguments"),
                "OpenRouter serialized tool arguments",
            )?;
            events.push(ModelStreamEvent::ToolCall(AssistantToolCall {
                id: ToolCallId::new(id)
                    .map_err(|_| "OpenRouter tool call omitted its identifier".to_owned())?,
                name: name.to_owned(),
                arguments: SerializedJson::new(arguments),
            }));
            has_tool_calls = true;
        }
    }
    let finish_reason =
        optional_string(as_object(choice, "OpenRouter choice")?.get("finish_reason"))?;
    let stop_reason = match finish_reason {
        Some("tool_calls" | "tool_call") if has_tool_calls => StopReason::ToolUse,
        Some("length") => StopReason::Length,
        _ if has_tool_calls => StopReason::ToolUse,
        _ => StopReason::EndTurn,
    };
    events.push(ModelStreamEvent::End(stop_reason));
    let usage = response.get("usage");
    let input_tokens = match usage {
        Some(usage) => number_field(usage, "prompt_tokens")?,
        None => None,
    };
    let output_tokens = match usage {
        Some(usage) => number_field(usage, "completion_tokens")?,
        None => None,
    };
    let reasoning_tokens = match usage {
        Some(usage) => usage.get_completion_reasoning_tokens()?,
        None => None,
    };
    Ok((
        events,
        Usage {
            input_tokens,
            output_tokens,
            reasoning_tokens,
        },
    ))
}

trait OpenRouterUsageExt {
    fn get_completion_reasoning_tokens(&self) -> Result<Option<u64>, String>;
}

impl OpenRouterUsageExt for JsonValue {
    fn get_completion_reasoning_tokens(&self) -> Result<Option<u64>, String> {
        let details = match self.get("completion_tokens_details") {
            None | Some(JsonValue::Null) => return Ok(None),
            Some(value) => value,
        };
        number_field(details, "reasoning_tokens")
    }
}

fn openrouter_error(error: &JsonValue, http_status: u16) -> String {
    let message = error
        .get("message")
        .and_then(|value| match value {
            JsonValue::String(value) => Some(value.as_str()),
            _ => None,
        })
        .unwrap_or("OpenRouter rejected the request");
    if http_status == 404 {
        format!(
            "OpenRouter rejected the model with HTTP 404: {message}. This key may be restricted to Zero Data Retention models; select a model OpenRouter marks as Zero Data Retention compatible."
        )
    } else {
        format!("OpenRouter rejected the request with HTTP {http_status}: {message}")
    }
}

fn as_object<'a>(
    value: &'a JsonValue,
    description: &str,
) -> Result<&'a BTreeMap<String, JsonValue>, String> {
    match value {
        JsonValue::Object(value) => Ok(value),
        _ => Err(format!("{description} was not a JSON object")),
    }
}

fn object_field<'a>(
    value: &'a JsonValue,
    name: &str,
) -> Result<&'a BTreeMap<String, JsonValue>, String> {
    let object = as_object(value, "OpenRouter JSON value")?;
    let value = object
        .get(name)
        .ok_or_else(|| format!("OpenRouter response omitted {name:?}"))?;
    as_object(value, name)
}

fn array_field<'a>(value: &'a JsonValue, name: &str) -> Result<&'a [JsonValue], String> {
    let object = as_object(value, "OpenRouter JSON value")?;
    let value = object
        .get(name)
        .ok_or_else(|| format!("OpenRouter response omitted {name:?}"))?;
    match value {
        JsonValue::Array(value) => Ok(value),
        _ => Err(format!(
            "OpenRouter response field {name:?} was not an array"
        )),
    }
}

fn optional_array(value: Option<&JsonValue>) -> Result<Option<&[JsonValue]>, String> {
    match value {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::Array(value)) => Ok(Some(value)),
        Some(_) => Err("OpenRouter tool_calls was not an array".to_owned()),
    }
}

fn optional_string(value: Option<&JsonValue>) -> Result<Option<&str>, String> {
    match value {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::String(value)) => Ok(Some(value)),
        Some(_) => Err("OpenRouter response field was not a string".to_owned()),
    }
}

fn required_string<'a>(value: Option<&'a JsonValue>, description: &str) -> Result<&'a str, String> {
    optional_string(value)?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{description} was missing or empty"))
}

fn number_field(value: &JsonValue, name: &str) -> Result<Option<u64>, String> {
    let object = as_object(value, "OpenRouter usage")?;
    match object.get(name) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::Number(JsonNumber::Unsigned(value))) => Ok(Some(*value)),
        Some(JsonValue::Number(JsonNumber::Signed(value))) if *value >= 0 => {
            Ok(Some(*value as u64))
        }
        Some(_) => Err(format!(
            "OpenRouter usage field {name:?} was not a non-negative integer"
        )),
    }
}

#[derive(Clone)]
struct McpBridge {
    script: PathBuf,
}

struct BridgeOutput {
    content: String,
    is_error: bool,
    status: Option<i32>,
}

impl McpBridge {
    fn docs(&self) -> Result<String, String> {
        let output = self.invoke(&["docs".to_owned()])?;
        if output.is_error {
            return Err(output.content);
        }
        Ok(output.content)
    }

    fn invoke(&self, arguments: &[String]) -> Result<BridgeOutput, String> {
        let output = Command::new("bun")
            .arg(&self.script)
            .args(arguments)
            .env_clear()
            .env("PATH", DEFAULT_SHELL_PATH)
            .env("HOME", "/root")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| format!("could not start rs-agent MCP bridge: {error}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let content = match (stdout.is_empty(), stderr.is_empty()) {
            (false, _) => stdout,
            (true, false) => stderr,
            (true, true) => "rs-agent MCP bridge returned no output".to_owned(),
        };
        Ok(BridgeOutput {
            content,
            is_error: !output.status.success(),
            status: output.status.code(),
        })
    }
}

#[derive(Clone)]
enum McpOperation {
    Call,
    ListResources,
    ReadResource,
}

#[derive(Clone)]
struct McpTool {
    name: String,
    description: String,
    schema: JsonValue,
    execution_mode: ToolExecutionMode,
    operation: McpOperation,
    bridge: Arc<McpBridge>,
}

impl McpTool {
    fn from_policy(tool: &PolicyTool, bridge: Arc<McpBridge>) -> Result<Self, String> {
        if tool.capability != "rs-agent" {
            return Err(format!(
                "Runebench policy tool {:?} requested unbound capability {:?}",
                tool.name, tool.capability
            ));
        }
        let operation = match tool.name.as_str() {
            "execute_code" | "list_bots" | "disconnect_bot" => McpOperation::Call,
            "rs_agent_list_resources" => McpOperation::ListResources,
            "rs_agent_read_resource" => McpOperation::ReadResource,
            _ => {
                return Err(format!(
                    "Runebench policy tool {:?} is not an rs-agent capability",
                    tool.name
                ));
            }
        };
        Ok(Self {
            name: tool.name.clone(),
            description: tool.description.clone(),
            schema: tool.schema.clone(),
            execution_mode: tool.execution_mode,
            operation,
            bridge,
        })
    }

    fn execute_bridge(&self, arguments: &str) -> Result<BridgeOutput, String> {
        let command = match self.operation {
            McpOperation::Call => vec!["call".to_owned(), self.name.clone(), arguments.to_owned()],
            McpOperation::ListResources => vec!["list-resources".to_owned()],
            McpOperation::ReadResource => {
                let value = JsonValue::parse(arguments).map_err(|_| {
                    "rs_agent_read_resource received invalid JSON arguments".to_owned()
                })?;
                let uri = required_string(value.get("uri"), "rs_agent_read_resource uri")?;
                vec!["read-resource".to_owned(), uri.to_owned()]
            }
        };
        self.bridge.invoke(&command)
    }
}

impl AgentTool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> &JsonValue {
        &self.schema
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        self.execution_mode
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        let tool = self.clone();
        let arguments = call.arguments.as_str().to_owned();
        let call_id = call.id;
        Box::pin(async move {
            if context.cancellation.is_cancelled() {
                return Err(ToolError::Cancelled { tool: tool.name });
            }
            let executing_tool = tool.clone();
            let result = smol::unblock(move || executing_tool.execute_bridge(&arguments)).await;
            if context.cancellation.is_cancelled() {
                return Err(ToolError::Cancelled { tool: tool.name });
            }
            let result = result.map_err(|message| ToolError::Execution {
                tool: tool.name.clone(),
                message,
            })?;
            Ok(ToolResult {
                tool_call_id: call_id,
                content: result.content,
                details: Some(SerializedJson::new(format!(
                    "{{\"mcp_exit_code\":{}}}",
                    result
                        .status
                        .map(|status| status.to_string())
                        .unwrap_or_else(|| "null".to_owned())
                ))),
                usage: None,
                added_tool_names: Vec::new(),
                terminate: false,
                is_error: result.is_error,
            })
        })
    }
}

struct JsonlObserver {
    file: Mutex<File>,
}

impl JsonlObserver {
    fn create(path: &PathBuf) -> Result<Self, String> {
        let parent = path
            .parent()
            .ok_or_else(|| "event-log path has no parent".to_owned())?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create event-log directory: {error}"))?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .map_err(|error| format!("cannot create event log: {error}"))?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    fn record(&self, event: &AgentEvent) {
        let line = event_json(event);
        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    }
}

impl EventObserver for JsonlObserver {
    fn observe<'a>(
        &'a self,
        event: &'a AgentEvent,
        _cancellation: CancellationToken,
    ) -> ObserverFuture<'a> {
        self.record(event);
        Box::pin(std::future::ready(Ok(())))
    }
}

fn event_json(event: &AgentEvent) -> String {
    match &event.kind {
        AgentEventKind::MessageStart { message } => message_json("message_start", message),
        AgentEventKind::MessageEnd { message } => message_json("message_end", message),
        AgentEventKind::ToolExecutionStart { tool_name, .. } => format!(
            "{{\"type\":\"tool_execution_start\",\"toolName\":{}}}",
            json_string(tool_name)
        ),
        AgentEventKind::ToolExecutionEnd {
            tool_name, result, ..
        } => format!(
            "{{\"type\":\"tool_execution_end\",\"toolName\":{},\"isError\":{}}}",
            json_string(tool_name),
            result.is_error,
        ),
        AgentEventKind::AgentStart => "{\"type\":\"agent_start\"}".to_owned(),
        AgentEventKind::AgentEnd { .. } => "{\"type\":\"agent_end\"}".to_owned(),
        AgentEventKind::TurnStart { .. } => "{\"type\":\"turn_start\"}".to_owned(),
        AgentEventKind::TurnEnd { reason, .. } => format!(
            "{{\"type\":\"turn_end\",\"stopReason\":{}}}",
            json_string(stop_reason_name(*reason))
        ),
        AgentEventKind::MessageUpdate { .. } => "{\"type\":\"message_update\"}".to_owned(),
        AgentEventKind::ToolExecutionUpdate { tool_name, .. } => format!(
            "{{\"type\":\"tool_execution_update\",\"toolName\":{}}}",
            json_string(tool_name)
        ),
    }
}

fn message_json(event_type: &str, message: &Message) -> String {
    match message {
        Message::User { content, .. } => format!(
            "{{\"type\":{},\"message\":{{\"role\":\"user\",\"content\":{}}}}}",
            json_string(event_type),
            json_string(content)
        ),
        Message::Assistant {
            content,
            stop_reason,
            error_message,
            ..
        } => {
            let stop_reason = stop_reason
                .map(stop_reason_name)
                .map(json_string)
                .unwrap_or_else(|| "null".to_owned());
            let error = error_message
                .as_deref()
                .map(json_string)
                .unwrap_or_else(|| "null".to_owned());
            format!(
                "{{\"type\":{},\"message\":{{\"role\":\"assistant\",\"content\":{},\"stopReason\":{stop_reason},\"errorMessage\":{error}}}}}",
                json_string(event_type),
                json_string(content),
            )
        }
        Message::ToolResult {
            tool_name,
            content,
            is_error,
            ..
        } => format!(
            "{{\"type\":{},\"message\":{{\"role\":\"tool\",\"toolName\":{},\"content\":{},\"isError\":{is_error}}}}}",
            json_string(event_type),
            json_string(tool_name),
            json_string(content),
        ),
    }
}

fn stop_reason_name(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::ToolUse => "tool_use",
        StopReason::Length => "length",
        StopReason::Aborted => "aborted",
        StopReason::Error => "error",
        StopReason::Cancelled => "cancelled",
    }
}

fn json_string(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            character if character.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(quoted, "\\u{:04x}", character as u32);
            }
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

fn append_section(prompt: &mut String, heading: Option<&str>, content: &str) {
    if content.trim().is_empty() {
        return;
    }
    prompt.push_str("\n\n");
    if let Some(heading) = heading {
        prompt.push_str(heading);
        prompt.push('\n');
    }
    prompt.push_str(content.trim());
}

fn write_audit(path: &PathBuf, docs_loaded: bool) -> Result<(), String> {
    let audit_path = path
        .parent()
        .ok_or_else(|| "event-log path has no parent".to_owned())?
        .join("runebench-pi-agent-core.json");
    fs::write(
        audit_path,
        format!(
            "{{\"policyLoaded\":true,\"docsLoaded\":{docs_loaded},\"mcpServer\":\"rs-agent\",\"tools\":[\"execute_code\",\"list_bots\",\"disconnect_bot\",\"rs_agent_list_resources\",\"rs_agent_read_resource\"]}}\n"
        ),
    )
    .map_err(|error| format!("cannot write agent-core audit: {error}"))
}

fn run(args: Args) -> Result<(), String> {
    let api_key = env::var("OPENROUTER_API_KEY").map_err(|_| {
        "OPENROUTER_API_KEY must be supplied by the caller's secret injector (for example: vault OPENROUTER_API_KEY -- …)".to_owned()
    })?;
    if api_key.trim().is_empty() {
        return Err("OPENROUTER_API_KEY was empty".to_owned());
    }
    let model = args
        .model
        .strip_prefix("openrouter/")
        .unwrap_or(&args.model)
        .to_owned();
    if model.trim().is_empty() {
        return Err("--model must identify an OpenRouter model".to_owned());
    }
    let policy_source = fs::read_to_string(&args.policy)
        .map_err(|error| format!("cannot read Luau policy {}: {error}", args.policy.display()))?;
    let policy = Arc::new(LuaPolicy::load(&policy_source).map_err(|error| error.to_string())?);
    let bridge = Arc::new(McpBridge {
        script: args.mcp_bridge,
    });
    let docs = bridge.docs();
    let docs_loaded = docs.is_ok();
    let docs = docs.unwrap_or_default();

    let default_tools = DefaultCodingTools::new(&args.workspace)
        .map_err(|error| format!("cannot construct workspace tools: {error}"))?
        .with_environment(CommandEnvironment::empty().with("PATH", DEFAULT_SHELL_PATH));
    let profile = PiDefaultCodingProfile::pinned_default()
        .map_err(|error| format!("cannot load pinned Pi default profile: {error}"))?;
    let mut tools = default_tools.registry();
    profile
        .validate_registry(&tools)
        .map_err(|error| format!("pinned Pi profile did not bind: {error}"))?;
    for declaration in policy.tools() {
        if !RS_AGENT_TOOL_NAMES.contains(&declaration.name.as_str()) {
            return Err(format!(
                "Runebench policy declared unsupported tool {:?}",
                declaration.name
            ));
        }
        tools.insert(Arc::new(McpTool::from_policy(
            declaration,
            Arc::clone(&bridge),
        )?));
    }

    let mut system_prompt =
        profile.system_prompt_for_workspace(default_tools.workspace().as_path());
    append_section(&mut system_prompt, None, policy.system_prompt_append());
    append_section(
        &mut system_prompt,
        Some("## Runebench API reference"),
        &docs,
    );
    write_audit(&args.log_jsonl, docs_loaded)?;

    let usage = UsageTotals::default();
    let provider: Arc<dyn ModelProvider> = Arc::new(OpenRouterProvider {
        api_key,
        model: model.clone(),
        usage: usage.clone(),
    });
    let host_hooks: Arc<dyn HookSet> = Arc::new(OpenAiContextHook);
    let hooks: Arc<dyn HookSet> = Arc::new(LuaPolicyHookSet::new(policy, host_hooks));
    let observer: Arc<dyn EventObserver> = Arc::new(JsonlObserver::create(&args.log_jsonl)?);
    let agent = Agent::builder()
        .model(ModelDescriptor {
            provider: "openrouter".to_owned(),
            model,
            revision: None,
        })
        .system_prompt(system_prompt)
        .tools(tools)
        .hooks(hooks)
        .model_provider(provider)
        .observer(observer)
        .build();
    let run = agent
        .start_prompt(args.instruction)
        .map_err(|error| format!("cannot start Runebench agent: {error}"))?;
    let result = smol::block_on(run.drive());
    let totals = usage.snapshot();
    eprintln!(
        "[pi-agent-core] usage input={} output={} reasoning={}",
        totals.input_tokens.unwrap_or(0),
        totals.output_tokens.unwrap_or(0),
        totals.reasoning_tokens.unwrap_or(0)
    );
    result.map_err(|error| format!("pi-agent-core run failed: {error}"))
}

fn main() {
    if env::args()
        .skip(1)
        .any(|argument| argument == "--help" || argument == "-h")
    {
        println!("{}", Args::usage());
        return;
    }
    if let Err(error) = Args::parse().and_then(run) {
        eprintln!("[pi-agent-core] {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{openrouter_error, parse_openrouter_response, write_curl_config};
    use pi_agent_core::scheduler::ModelStreamEvent;
    use pi_agent_protocol::JsonValue;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn parses_openrouter_tool_response_without_serde() {
        let response = br#"{
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {"content": null, "tool_calls": [{
                    "id": "call_1",
                    "function": {"name": "execute_code", "arguments": "{\\\"bot_name\\\":\\\"agent\\\",\\\"code\\\":\\\"await bot.chopTree()\\\"}"}
                }]}
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 20}
        }"#;
        let (events, usage) =
            parse_openrouter_response(response, 200).expect("response should parse");

        assert!(matches!(events[0], ModelStreamEvent::ToolCall(_)));
        assert!(matches!(events.last(), Some(ModelStreamEvent::End(_))));
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(20));
    }

    #[test]
    fn http_404_explains_zero_retention_key_restriction() {
        let error = JsonValue::parse(r#"{"message":"No endpoints found"}"#)
            .expect("fixture JSON should parse");
        let message = openrouter_error(&error, 404);

        assert!(message.contains("Zero Data Retention"));
        assert!(message.contains("HTTP 404"));
    }

    #[test]
    fn curl_config_is_private_and_quotes_the_provider_key() {
        let path = write_curl_config("key\\\"quote").expect("config should be created");
        let contents = std::fs::read_to_string(&path).expect("config should be readable");
        let mode = std::fs::metadata(&path)
            .expect("config metadata should be readable")
            .permissions()
            .mode();
        std::fs::remove_file(&path).expect("test config should be removed");

        assert!(contents.contains("request = \"POST\""));
        assert!(contents.contains("Authorization: Bearer key\\\\\\\"quote"));
        assert_eq!(mode & 0o777, 0o600);
    }
}
