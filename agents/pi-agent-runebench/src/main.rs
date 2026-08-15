//! Runebench's concrete world host for the provider-free Pi Rust core.
//!
//! This binary intentionally owns provider selection and the rs-agent MCP
//! process. `pi-agent-core` remains provider/world agnostic, while the adjacent
//! Luau policy only declares prompts and explicit tool-policy decisions.

mod mcp_client;

use pi_agent_core::default_tools::CommandEnvironment;
use pi_agent_core::error::{CoreError, HookError};
use pi_agent_core::event::{AgentEvent, AgentEventKind, EventObserver, ObserverFuture};
use pi_agent_core::hooks::{AfterToolCall, BeforeToolCall, ContextEnvelope, HookSet, NextTurn};
use pi_agent_core::profile::PiDefaultCodingProfile;
use pi_agent_core::provider::commandcode::{
    CommandCodeConfig, CommandCodeHostContext, CommandCodeProvider,
};
use pi_agent_core::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use pi_agent_core::state::{
    AssistantToolCall, Message, ModelDescriptor, SerializedJson, StopReason, ToolCallId, Usage,
};
use pi_agent_core::tool::{ToolCall, ToolResult};
use pi_agent_core::{Agent, DefaultCodingTools};
use pi_agent_luau::capability::{
    CapabilityGrant, CapabilityManifest, CapabilityModule, CapabilityOperation,
    McpOperation as GrantedMcpOperation, WorldOperation,
};
use pi_agent_luau::tool_handler::{
    CapabilityBindings, CapabilityError as LuaCapabilityError,
    CapabilityFuture as LuaCapabilityFuture, CapabilityRequest as LuaCapabilityRequest,
    CapabilityResponse as LuaCapabilityResponse, LuaToolHandler, LuauCapability, ToolHandlerSpec,
};
use pi_agent_luau::{LuaPolicy, LuaPolicyHookSet};
use pi_agent_protocol::{JsonNumber, JsonValue};
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const OPENROUTER_COMPLETIONS_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
const DEFAULT_SHELL_PATH: &str = "/root/.bun/bin:/usr/local/bin:/usr/bin:/bin";
const MODEL_TOOL_RESULT_MAX_CHARS: usize = 12_000;
const MODEL_TOOL_DETAILS_MAX_CHARS: usize = 3_000;
const BOOTSTRAP_DOC_MAX_CHARS: usize = 24_000;
const REPEATED_TOOL_FAILURE_LIMIT: usize = 3;
const RS_AGENT_TOOL_NAMES: [&str; 5] = [
    "execute_code",
    "list_bots",
    "disconnect_bot",
    "rs_agent_list_resources",
    "rs_agent_read_resource",
];
static PROCESS_CAPTURE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

struct Args {
    model: String,
    instruction: String,
    workspace: PathBuf,
    policy: PathBuf,
    log_jsonl: PathBuf,
    run_deadline: Option<Duration>,
    commandcode_date: Option<String>,
    commandcode_environment: Option<String>,
    commandcode_thread_id: Option<String>,
    commandcode_project_slug: Option<String>,
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
        let run_deadline = values
            .get("--deadline-seconds")
            .map(|value| {
                value
                    .parse::<u64>()
                    .ok()
                    .filter(|seconds| *seconds > 0)
                    .map(Duration::from_secs)
                    .ok_or_else(|| {
                        "--deadline-seconds must be a positive whole number of seconds".to_owned()
                    })
            })
            .transpose()?;
        let commandcode_date = values.get("--commandcode-date").cloned();
        let commandcode_environment = values.get("--commandcode-environment").cloned();
        let commandcode_thread_id = values.get("--commandcode-thread-id").cloned();
        let commandcode_project_slug = values.get("--commandcode-project-slug").cloned();
        Ok(Self {
            model: required("--model")?,
            instruction: required("--instruction")?,
            workspace: PathBuf::from(required("--workspace")?),
            policy: PathBuf::from(required("--policy")?),
            log_jsonl: PathBuf::from(required("--log-jsonl")?),
            run_deadline,
            commandcode_date,
            commandcode_environment,
            commandcode_thread_id,
            commandcode_project_slug,
        })
    }

    fn usage() -> &'static str {
        "usage: runebench-pi-agent --model <openrouter/model|commandcode/model> --instruction <text> --workspace <dir> --policy <file.luau> --log-jsonl <file> [--deadline-seconds <positive-seconds>] [--commandcode-date <YYYY-MM-DD> --commandcode-environment <platform> --commandcode-thread-id <UUID> --commandcode-project-slug <slug>]"
    }
}

/// A host-owned deadline that requests structured core cancellation, then
/// joins before process exit. The core remains free of timers and threads;
/// this world host owns the benchmark-specific lifecycle boundary.
struct DeadlineGuard {
    disarmed: Arc<(Mutex<bool>, Condvar)>,
    expired: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl DeadlineGuard {
    fn arm(deadline: Duration, on_expire: impl FnOnce() + Send + 'static) -> Self {
        let disarmed = Arc::new((Mutex::new(false), Condvar::new()));
        let expired = Arc::new(AtomicBool::new(false));
        let worker_disarmed = Arc::clone(&disarmed);
        let worker_expired = Arc::clone(&expired);
        let worker = thread::spawn(move || {
            let (lock, wake) = &*worker_disarmed;
            let armed = lock.lock().expect("deadline mutex poisoned");
            let (armed, waited) = wake
                .wait_timeout_while(armed, deadline, |disarmed| !*disarmed)
                .expect("deadline condition variable poisoned");
            if !*armed && waited.timed_out() {
                worker_expired.store(true, Ordering::Release);
                on_expire();
            }
        });
        Self {
            disarmed,
            expired,
            worker: Some(worker),
        }
    }

    fn expired(&self) -> bool {
        self.expired.load(Ordering::Acquire)
    }

    fn disarm(&mut self) {
        let (lock, wake) = &*self.disarmed;
        *lock.lock().expect("deadline mutex poisoned") = true;
        wake.notify_all();
        if let Some(worker) = self.worker.take() {
            worker.join().expect("deadline worker panicked");
        }
    }
}

impl Drop for DeadlineGuard {
    fn drop(&mut self) {
        self.disarm();
    }
}

/// Provider protocol conversion stays outside the core and the Lua VM.
#[derive(Debug, Default)]
struct ToolFailureTracker {
    signature: Option<String>,
    consecutive: usize,
    terminal_reason: Option<String>,
}

#[derive(Debug, Default)]
struct OpenAiContextHook {
    failures: Mutex<ToolFailureTracker>,
    include_error_metadata: bool,
}

impl OpenAiContextHook {
    fn for_provider(provider: ProviderKind) -> Self {
        Self {
            failures: Mutex::new(ToolFailureTracker::default()),
            include_error_metadata: provider == ProviderKind::CommandCode,
        }
    }
}

impl HookSet for OpenAiContextHook {
    fn before_tool_call(&self, _call: &ToolCall) -> Result<BeforeToolCall, HookError> {
        let failures = self
            .failures
            .lock()
            .map_err(|_| HookError::new("before_tool_call", "tool failure tracker was poisoned"))?;
        if let Some(reason) = &failures.terminal_reason {
            return Ok(BeforeToolCall::Terminate {
                reason: reason.clone(),
            });
        }
        Ok(BeforeToolCall::Allow)
    }

    fn after_tool_call(
        &self,
        call: &ToolCall,
        result: &ToolResult,
    ) -> Result<AfterToolCall, HookError> {
        let mut failures = self
            .failures
            .lock()
            .map_err(|_| HookError::new("after_tool_call", "tool failure tracker was poisoned"))?;
        if !result.is_error {
            failures.signature = None;
            failures.consecutive = 0;
            return Ok(AfterToolCall::default());
        }

        let signature = format!("{}:{}", call.name, tool_failure_signature(&result.content));
        if failures.signature.as_deref() == Some(signature.as_str()) {
            failures.consecutive = failures.consecutive.saturating_add(1);
        } else {
            failures.signature = Some(signature);
            failures.consecutive = 1;
        }
        let terminate = tool_error_is_fatal(&result.content)
            || failures.consecutive >= REPEATED_TOOL_FAILURE_LIMIT;
        if terminate {
            failures.terminal_reason = Some(if tool_error_is_fatal(&result.content) {
                format!(
                    "terminal rs-agent capability failure: {}",
                    truncate_for_model(&result.content, 1_000)
                )
            } else {
                format!(
                    "repeated rs-agent tool failure after {} attempts: {}",
                    failures.consecutive,
                    truncate_for_model(&result.content, 1_000)
                )
            });
            return Ok(AfterToolCall {
                terminate: Some(true),
                ..AfterToolCall::default()
            });
        }
        Ok(AfterToolCall::default())
    }

    fn transform_context(&self, context: ContextEnvelope) -> Result<ContextEnvelope, HookError> {
        Ok(context)
    }

    fn convert_to_llm(&self, context: ContextEnvelope) -> Result<String, HookError> {
        let messages = context
            .messages
            .iter()
            .map(|message| openai_message(message, self.include_error_metadata))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(format!("[{}]", messages.join(",")))
    }

    fn should_stop_after_turn(&self, _context: &ContextEnvelope) -> Result<bool, HookError> {
        let failures = self.failures.lock().map_err(|_| {
            HookError::new(
                "should_stop_after_turn",
                "tool failure tracker was poisoned",
            )
        })?;
        Ok(failures.terminal_reason.is_some())
    }

    fn prepare_next_turn(&self, _context: ContextEnvelope) -> Result<NextTurn, HookError> {
        Ok(NextTurn::default())
    }
}

fn openai_message(message: &Message, include_error_metadata: bool) -> Result<String, HookError> {
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
            tool_name,
            content,
            details,
            is_error,
            ..
        } => {
            let error_metadata = if include_error_metadata {
                format!(",\"is_error\":{is_error}")
            } else {
                String::new()
            };
            Ok(format!(
                "{{\"role\":\"tool\",\"tool_call_id\":{},\"content\":{}{error_metadata}}}",
                json_string(tool_call_id.as_str()),
                json_string(&curate_tool_result(
                    tool_name,
                    content,
                    details.as_ref(),
                    *is_error,
                )),
            ))
        }
    }
}

fn curate_tool_result(
    tool_name: &str,
    content: &str,
    details: Option<&SerializedJson>,
    is_error: bool,
) -> String {
    let status = if is_error { "error" } else { "ok" };
    let content_limit = if is_error {
        MODEL_TOOL_RESULT_MAX_CHARS.saturating_sub(MODEL_TOOL_DETAILS_MAX_CHARS)
    } else {
        MODEL_TOOL_RESULT_MAX_CHARS
    };
    let mut output = format!("[{tool_name} result: {status}]\n");
    if content.is_empty() {
        output.push_str("(empty result)");
    } else {
        output.push_str(&truncate_for_model(content, content_limit));
    }
    if let Some(details) = details {
        output.push_str("\n[structured details]\n");
        output.push_str(&truncate_for_model(
            details.as_str(),
            MODEL_TOOL_DETAILS_MAX_CHARS,
        ));
    }
    if is_error && tool_error_is_fatal(content) {
        output.push_str("\n[terminal capability error: do not retry this tool]");
    }
    truncate_for_model(&output, MODEL_TOOL_RESULT_MAX_CHARS)
}

fn truncate_for_model(value: &str, limit: usize) -> String {
    let character_count = value.chars().count();
    if character_count <= limit {
        return value.to_owned();
    }
    if limit < 32 {
        return value.chars().take(limit).collect();
    }
    let marker = format!(
        "\n...[truncated {} characters]...\n",
        character_count.saturating_sub(limit)
    );
    let available = limit.saturating_sub(marker.chars().count());
    let head_count = available.saturating_mul(2) / 3;
    let tail_count = available.saturating_sub(head_count);
    let head = value.chars().take(head_count).collect::<String>();
    let tail = value
        .chars()
        .skip(character_count.saturating_sub(tail_count))
        .collect::<String>();
    format!("{head}{marker}{tail}")
}

fn tool_failure_signature(content: &str) -> String {
    let normalized = content.trim().to_ascii_lowercase();
    for marker in [
        "broken pipe",
        "mcp protocol error",
        "mcp child exited",
        "not connected",
        "invalid json",
        "waitforconnection timed out",
        "game state not ready within timeout",
        "operation aborted",
    ] {
        if normalized.contains(marker) {
            return marker.to_owned();
        }
    }
    normalized.chars().take(240).collect()
}

fn tool_error_is_fatal(content: &str) -> bool {
    let normalized = content.to_ascii_lowercase();
    [
        "broken pipe",
        "mcp protocol error",
        "mcp child exited",
        "not connected (state: disconnected)",
        "invalid json",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
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

/// The provider namespace is part of the benchmark invocation contract, not a
/// model-name heuristic. It selects both the expected secret and the exact
/// `ModelDescriptor` namespace seen by the Rust core.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderKind {
    OpenRouter,
    CommandCode,
}

impl ProviderKind {
    fn descriptor_name(self) -> &'static str {
        match self {
            Self::OpenRouter => "openrouter",
            Self::CommandCode => "command-code",
        }
    }

    fn api_key_name(self) -> &'static str {
        match self {
            Self::OpenRouter => "OPENROUTER_API_KEY",
            Self::CommandCode => "COMMANDCODE_API_KEY",
        }
    }
}

/// Host-owned Command Code facts. They are parsed at the command boundary and
/// passed into the core provider instead of being discovered by that library.
struct CommandCodeRequestContext {
    date: String,
    environment: String,
    thread_id: String,
    project_slug: String,
}

fn parse_model(model: &str) -> Result<(ProviderKind, String), String> {
    let (provider, model) = if let Some(model) = model.strip_prefix("openrouter/") {
        (ProviderKind::OpenRouter, model)
    } else if let Some(model) = model.strip_prefix("commandcode/") {
        (ProviderKind::CommandCode, model)
    } else {
        return Err(
            "--model must use the openrouter/<model> or commandcode/<model> namespace".into(),
        );
    };
    if model.trim().is_empty() {
        return Err("--model must include a provider model name".into());
    }
    Ok((provider, model.to_owned()))
}

fn commandcode_request_context(args: &Args) -> Result<CommandCodeRequestContext, String> {
    let required = |value: &Option<String>, flag: &str| {
        value
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| format!("commandcode models require {flag}"))
    };
    Ok(CommandCodeRequestContext {
        date: required(&args.commandcode_date, "--commandcode-date")?,
        environment: required(&args.commandcode_environment, "--commandcode-environment")?,
        thread_id: required(&args.commandcode_thread_id, "--commandcode-thread-id")?,
        project_slug: required(&args.commandcode_project_slug, "--commandcode-project-slug")?,
    })
}

enum UsageSource {
    OpenRouter(UsageTotals),
    CommandCode(Arc<CommandCodeProvider>),
}

impl UsageSource {
    fn snapshot(&self) -> Usage {
        match self {
            Self::OpenRouter(usage) => usage.snapshot(),
            Self::CommandCode(provider) => provider.usage_snapshot(),
        }
    }

    /// Command Code keeps remote diagnostics separate from agent state. The benchmark host owns
    /// this private stderr sink, so failed trials retain the provider's status/type/code and
    /// message without making arbitrary gateway text a model-visible tool result.
    fn commandcode_error_report(
        &self,
    ) -> Option<pi_agent_core::provider::commandcode::CommandCodeErrorReport> {
        match self {
            Self::OpenRouter(_) => None,
            Self::CommandCode(provider) => provider.last_error_report(),
        }
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
        match self.complete(request, &cancellation) {
            Ok((mut events, usage)) => {
                if cancellation.is_cancelled() {
                    return ModelStream {
                        events: vec![ModelStreamEvent::End(StopReason::Cancelled)],
                    };
                }
                self.usage.add(usage.clone());
                let terminal = events
                    .pop()
                    .expect("OpenRouter parser always returns a terminal event");
                events.push(ModelStreamEvent::Usage(usage));
                events.push(terminal);
                ModelStream { events }
            }
            Err(_message) if cancellation.is_cancelled() => ModelStream {
                events: vec![ModelStreamEvent::End(StopReason::Cancelled)],
            },
            Err(message) => ModelStream {
                events: vec![ModelStreamEvent::Error { message }],
            },
        }
    }

    fn complete(
        &self,
        request: ModelRequest,
        cancellation: &CancellationToken,
    ) -> Result<(Vec<ModelStreamEvent>, Usage), String> {
        let payload = openrouter_payload(&self.model, request)?;
        let config_path = write_curl_config(&self.api_key)?;
        let (stdout_path, stdout) = process_capture_file("openrouter", "stdout")?;
        let (stderr_path, stderr) = match process_capture_file("openrouter", "stderr") {
            Ok(capture) => capture,
            Err(error) => {
                let _ = fs::remove_file(&stdout_path);
                let _ = fs::remove_file(&config_path);
                return Err(error);
            }
        };
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
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr))
                .spawn()
                .map_err(|error| format!("could not start OpenRouter transport: {error}"))?;
            child
                .stdin
                .as_mut()
                .ok_or_else(|| "OpenRouter transport did not expose request stdin".to_owned())?
                .write_all(payload.as_bytes())
                .map_err(|error| format!("could not write OpenRouter request: {error}"))?;
            // Curl must observe EOF before it begins the request. `wait_with_output`
            // does this internally, but the cancellation-aware wait below owns the
            // child lifecycle explicitly.
            drop(child.stdin.take());
            let (status, cancelled) =
                wait_for_child_or_cancellation(&mut child, Some(cancellation))?;
            if cancelled {
                return Err("OpenRouter transport cancelled".to_owned());
            }
            let stdout = fs::read(&stdout_path)
                .map_err(|error| format!("cannot read OpenRouter response capture: {error}"))?;
            let stderr = fs::read(&stderr_path)
                .map_err(|error| format!("cannot read OpenRouter error capture: {error}"))?;
            Ok((status, stdout, stderr))
        })();
        // The config carries the Authorization header. It is mode 0600 and is
        // removed before any provider body/error can reach an agent log.
        let _ = fs::remove_file(&config_path);
        let _ = fs::remove_file(&stdout_path);
        let _ = fs::remove_file(&stderr_path);
        let (status, stdout, stderr) = output_result?;
        if cancellation.is_cancelled() {
            return Err("OpenRouter transport cancelled".to_owned());
        }
        // A nonzero curl exit is a transport failure. HTTP status itself is
        // emitted into stdout by --write-out and parsed below.
        if !status.success() {
            return Err(format!(
                "OpenRouter transport failed before a provider response: {}",
                String::from_utf8_lossy(&stderr).trim()
            ));
        }
        let (body, status) = split_curl_status(&stdout)?;
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

/// Create a private capture file for a child process owned by this host.
///
/// Captures avoid the pipe-lifetime trap where a descendant inherited stdout
/// or stderr after its direct parent exited. The file is unlinked immediately
/// after the direct child has settled and its snapshot has been read.
fn process_capture_file(operation: &str, stream: &str) -> Result<(PathBuf, File), String> {
    for _ in 0..16 {
        let sequence = PROCESS_CAPTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "pi-agent-runebench-{operation}-{}-{sequence}-{stream}",
            std::process::id(),
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "cannot create private {operation} capture: {error}"
                ))
            }
        }
    }
    Err(format!(
        "cannot allocate a unique private {operation} capture after 16 attempts"
    ))
}

/// Poll one direct child and reap it when the run cancellation scope fires.
/// Detached descendants deliberately keep running: the benchmark policy may
/// create a background game worker whose lifetime exceeds one agent turn.
fn wait_for_child_or_cancellation(
    child: &mut Child,
    cancellation: Option<&CancellationToken>,
) -> Result<(ExitStatus, bool), String> {
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("child status could not be read: {error}"))?
        {
            return Ok((status, false));
        }
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            if let Err(error) = child.kill() {
                if error.kind() != std::io::ErrorKind::InvalidInput {
                    return Err(format!("cancelled child could not be killed: {error}"));
                }
            }
            let status = child
                .wait()
                .map_err(|error| format!("cancelled child could not be reaped: {error}"))?;
            return Ok((status, true));
        }
        thread::sleep(Duration::from_millis(10));
    }
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
            ..Usage::default()
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

/// The fixed Runebench rs-agent MCP capability made available to Luau tool
/// handlers. The manifest is checked on every call, not only during policy
/// construction, so neither an altered handler nor model text can expand the
/// host's authority.
#[derive(Clone)]
struct RunebenchMcpCapability {
    client: Arc<Mutex<mcp_client::McpClient>>,
    manifest: CapabilityManifest,
}

impl RunebenchMcpCapability {
    fn new(client: Arc<Mutex<mcp_client::McpClient>>) -> Result<Self, String> {
        let call_tools = ["execute_code", "list_bots", "disconnect_bot"];
        let mut operations = call_tools
            .into_iter()
            .map(|tool| {
                WorldOperation::mcp("rs-agent", GrantedMcpOperation::Call, Some(tool))
                    .map(CapabilityOperation::World)
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        operations.extend([
            WorldOperation::mcp(
                "rs-agent",
                GrantedMcpOperation::ListResources,
                Some("rs_agent_list_resources"),
            )
            .map(CapabilityOperation::World)
            .map_err(|error| error.to_string())?,
            WorldOperation::mcp(
                "rs-agent",
                GrantedMcpOperation::ReadResource,
                Some("rs_agent_read_resource"),
            )
            .map(CapabilityOperation::World)
            .map_err(|error| error.to_string())?,
        ]);
        let grant = CapabilityGrant::new(CapabilityModule::World, operations)
            .map_err(|error| error.to_string())?;
        let manifest = CapabilityManifest::new([grant]).map_err(|error| error.to_string())?;
        Ok(Self { client, manifest })
    }

    fn operation_for(
        request: &LuaCapabilityRequest,
    ) -> Result<CapabilityOperation, LuaCapabilityError> {
        let operation = match request.method.as_str() {
            "tools.call" => GrantedMcpOperation::Call,
            "resources.list" => GrantedMcpOperation::ListResources,
            "resources.read" => GrantedMcpOperation::ReadResource,
            method => {
                return Err(LuaCapabilityError::MethodDenied {
                    capability: request.capability.clone(),
                    method: method.to_owned(),
                });
            }
        };
        WorldOperation::mcp("rs-agent", operation, Some(request.tool_name.as_str()))
            .map(CapabilityOperation::World)
            .map_err(|error| LuaCapabilityError::Execution {
                message: error.to_string(),
            })
    }

    fn normalize_tool_arguments(
        tool_name: &str,
        arguments: &JsonValue,
    ) -> Result<JsonValue, String> {
        let mut arguments = arguments
            .as_object()
            .cloned()
            .ok_or_else(|| "rs-agent tool arguments must be a JSON object".to_owned())?;
        if tool_name == "execute_code" {
            arguments.insert("bot_name".to_owned(), JsonValue::String("agent".to_owned()));
            if let Some(timeout) = arguments.remove("timeout_minutes") {
                arguments.insert("timeout".to_owned(), timeout);
            }
        }
        Ok(JsonValue::Object(arguments))
    }

    fn invoke_blocking(
        client: &mut mcp_client::McpClient,
        manifest: &CapabilityManifest,
        request: LuaCapabilityRequest,
        cancellation: &CancellationToken,
    ) -> Result<LuaCapabilityResponse, LuaCapabilityError> {
        if cancellation.is_cancelled() {
            return Err(LuaCapabilityError::Cancelled);
        }
        let operation = Self::operation_for(&request)?;
        manifest
            .check(&pi_agent_luau::capability::CapabilityRequest::new(
                operation,
                request.arguments.clone(),
            ))
            .map_err(|error| LuaCapabilityError::MethodDenied {
                capability: request.capability.clone(),
                method: error.to_string(),
            })?;
        let response = match request.method.as_str() {
            "tools.call" => {
                if !matches!(request.arguments, JsonValue::Object(_)) {
                    return Err(LuaCapabilityError::InvalidArguments {
                        message: "rs-agent tools.call arguments must be a JSON object".to_owned(),
                    });
                }
                let arguments =
                    Self::normalize_tool_arguments(&request.tool_name, &request.arguments)
                        .map_err(|message| LuaCapabilityError::InvalidArguments { message })?;
                let result = client
                    .tools_call(&request.tool_name, &arguments, cancellation)
                    .map_err(map_mcp_error)?;
                let details_json = result
                    .structured_content
                    .as_ref()
                    .map(JsonValue::to_json_string)
                    .transpose()
                    .map_err(|error| LuaCapabilityError::Execution {
                        message: format!("cannot encode MCP structured content: {error}"),
                    })?;
                JsonValue::object([
                    (
                        "content",
                        JsonValue::String(result.content_text().map_err(map_mcp_error)?),
                    ),
                    ("is_error", JsonValue::Bool(result.is_error)),
                    (
                        "details_json",
                        details_json
                            .map(JsonValue::String)
                            .unwrap_or(JsonValue::Null),
                    ),
                ])
            }
            "resources.list" => {
                let result = client.resources_list(cancellation).map_err(map_mcp_error)?;
                let resources = result
                    .resources
                    .into_iter()
                    .map(|resource| {
                        JsonValue::object([
                            ("uri", JsonValue::String(resource.uri)),
                            (
                                "name",
                                resource
                                    .name
                                    .map(JsonValue::String)
                                    .unwrap_or(JsonValue::Null),
                            ),
                            (
                                "description",
                                resource
                                    .description
                                    .map(JsonValue::String)
                                    .unwrap_or(JsonValue::Null),
                            ),
                            (
                                "mimeType",
                                resource
                                    .mime_type
                                    .map(JsonValue::String)
                                    .unwrap_or(JsonValue::Null),
                            ),
                        ])
                    })
                    .collect::<Vec<_>>();
                JsonValue::object([
                    (
                        "content",
                        JsonValue::Array(resources)
                            .to_json_string()
                            .map(JsonValue::String)
                            .map_err(|error| LuaCapabilityError::Execution {
                                message: format!("cannot encode MCP resource list: {error}"),
                            })?,
                    ),
                    ("is_error", JsonValue::Bool(false)),
                ])
            }
            "resources.read" => {
                let uri = request
                    .arguments
                    .get("uri")
                    .and_then(|value| match value {
                        JsonValue::String(value) if !value.is_empty() => Some(value.as_str()),
                        _ => None,
                    })
                    .ok_or_else(|| LuaCapabilityError::InvalidArguments {
                        message: "rs-agent resources.read requires a non-empty uri string"
                            .to_owned(),
                    })?;
                let result = client
                    .resources_read(uri, cancellation)
                    .map_err(map_mcp_error)?;
                JsonValue::object([
                    (
                        "content",
                        JsonValue::String(result.content_text().map_err(map_mcp_error)?),
                    ),
                    ("is_error", JsonValue::Bool(false)),
                ])
            }
            _ => unreachable!("operation_for validated the MCP method"),
        };
        Ok(LuaCapabilityResponse { value: response })
    }
}

impl LuauCapability for RunebenchMcpCapability {
    fn invoke(
        &self,
        request: LuaCapabilityRequest,
        cancellation: CancellationToken,
    ) -> LuaCapabilityFuture {
        let client = Arc::clone(&self.client);
        let manifest = self.manifest.clone();
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(LuaCapabilityError::Cancelled);
            }
            let worker_cancellation = cancellation.clone();
            let result = smol::unblock(move || {
                let mut client = client.lock().map_err(|_| LuaCapabilityError::Execution {
                    message: "rs-agent MCP client lock was poisoned".to_owned(),
                })?;
                Self::invoke_blocking(&mut client, &manifest, request, &worker_cancellation)
            })
            .await;
            if cancellation.is_cancelled() {
                return Err(LuaCapabilityError::Cancelled);
            }
            result
        })
    }
}

fn map_mcp_error(error: mcp_client::McpError) -> LuaCapabilityError {
    match error {
        mcp_client::McpError::Cancelled => LuaCapabilityError::Cancelled,
        error => LuaCapabilityError::Execution {
            message: error.to_string(),
        },
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
        AgentEventKind::CompactionStart { .. }
        | AgentEventKind::CompactionResult { .. }
        | AgentEventKind::CompactionEnd { .. } => "{\"type\":\"compaction\"}".to_owned(),
        AgentEventKind::ModelTurnUsage { .. } => "{\"type\":\"model_turn_usage\"}".to_owned(),
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
            "{{\"policyLoaded\":true,\"docsLoaded\":{docs_loaded},\"mcpServer\":\"rs-agent\",\"tools\":[\"execute_code\",\"list_bots\",\"disconnect_bot\"],\"bootstrapResources\":[\"rs_agent_list_resources\",\"rs_agent_read_resource\"]}}\n"
        ),
    )
    .map_err(|error| format!("cannot write agent-core audit: {error}"))
}

fn load_mcp_docs(
    client: &Arc<Mutex<mcp_client::McpClient>>,
    cancellation: &CancellationToken,
) -> Result<String, String> {
    let mut client = client
        .lock()
        .map_err(|_| "rs-agent MCP client lock was poisoned".to_owned())?;
    let resources = client
        .resources_list(cancellation)
        .map_err(|error| format!("cannot list rs-agent MCP resources: {error}"))?;
    let mut parts = Vec::new();
    for resource in resources.resources {
        let content = client
            .resources_read(&resource.uri, cancellation)
            .and_then(|result| result.content_text())
            .map_err(|error| {
                format!(
                    "cannot read rs-agent MCP resource {:?}: {error}",
                    resource.uri
                )
            })?;
        if !content.is_empty() {
            let excerpt = truncate_for_model(&content, BOOTSTRAP_DOC_MAX_CHARS);
            parts.push(format!(
                "### {}\n{excerpt}",
                resource.name.unwrap_or(resource.uri)
            ));
        }
    }
    Ok(parts.join("\n\n"))
}

fn run(args: Args) -> Result<(), String> {
    let (provider_kind, model) = parse_model(&args.model)?;
    let commandcode_context = match provider_kind {
        ProviderKind::OpenRouter => None,
        ProviderKind::CommandCode => Some(commandcode_request_context(&args)?),
    };
    let api_key_name = provider_kind.api_key_name();
    let api_key = env::var(api_key_name)
        .map_err(|_| format!("{api_key_name} must be supplied by the caller's secret injector"))?;
    if api_key.trim().is_empty() {
        return Err(format!("{api_key_name} was empty"));
    }
    let policy_source = fs::read_to_string(&args.policy)
        .map_err(|error| format!("cannot read Luau policy {}: {error}", args.policy.display()))?;
    let policy = Arc::new(LuaPolicy::load(&policy_source).map_err(|error| error.to_string())?);
    let bootstrap_cancellation = CancellationToken::new();
    let mcp_client = Arc::new(Mutex::new(
        mcp_client::McpClient::connect_default(&bootstrap_cancellation)
            .map_err(|error| format!("cannot start rs-agent MCP client: {error}"))?,
    ));
    let docs = load_mcp_docs(&mcp_client, &bootstrap_cancellation);
    let docs_loaded = docs.is_ok();
    let docs = docs.unwrap_or_default();
    let mcp_capability = Arc::new(RunebenchMcpCapability::new(Arc::clone(&mcp_client))?);
    let mut capability_bindings = CapabilityBindings::new();
    capability_bindings
        .insert("rs-agent", mcp_capability)
        .map_err(|error| format!("cannot bind rs-agent MCP capability: {error}"))?;

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
        let handler_source = declaration.handler_source.as_deref().ok_or_else(|| {
            format!(
                "Runebench policy tool {:?} must declare a coroutine handler_source",
                declaration.name
            )
        })?;
        tools.insert(Arc::new(
            LuaToolHandler::new(
                handler_source,
                ToolHandlerSpec {
                    name: declaration.name.clone(),
                    description: declaration.description.clone(),
                    schema: declaration.schema.clone(),
                    capability: declaration.capability.clone(),
                    execution_mode: declaration.execution_mode,
                },
                capability_bindings.clone(),
            )
            .map_err(|error| {
                format!(
                    "cannot construct Luau handler for Runebench tool {:?}: {error}",
                    declaration.name
                )
            })?,
        ));
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

    let (provider, usage): (Arc<dyn ModelProvider>, UsageSource) = match provider_kind {
        ProviderKind::OpenRouter => {
            let usage = UsageTotals::default();
            let provider: Arc<dyn ModelProvider> = Arc::new(OpenRouterProvider {
                api_key,
                model: model.clone(),
                usage: usage.clone(),
            });
            (provider, UsageSource::OpenRouter(usage))
        }
        ProviderKind::CommandCode => {
            let context = commandcode_context
                .expect("Command Code context was validated with its provider namespace");
            let host = CommandCodeHostContext::new(
                default_tools
                    .workspace()
                    .as_path()
                    .to_string_lossy()
                    .to_string(),
                context.date,
                context.environment,
            )
            .map_err(|error| format!("invalid Command Code host context: {error}"))?;
            let config = CommandCodeConfig::new(api_key, model.clone(), host)
                .map_err(|error| format!("invalid Command Code configuration: {error}"))?
                .with_thread_id(context.thread_id)
                .and_then(|config| config.with_project_slug(context.project_slug))
                .map_err(|error| format!("invalid Command Code configuration: {error}"))?;
            let provider = Arc::new(CommandCodeProvider::new(config));
            (
                provider.clone() as Arc<dyn ModelProvider>,
                UsageSource::CommandCode(provider),
            )
        }
    };
    let host_hooks: Arc<dyn HookSet> = Arc::new(OpenAiContextHook::for_provider(provider_kind));
    let hooks: Arc<dyn HookSet> = Arc::new(LuaPolicyHookSet::new(policy, host_hooks));
    let observer: Arc<dyn EventObserver> = Arc::new(JsonlObserver::create(&args.log_jsonl)?);
    let agent = Agent::builder()
        .model(ModelDescriptor {
            provider: provider_kind.descriptor_name().to_owned(),
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
    let mut deadline = args.run_deadline.map(|duration| {
        let agent = agent.clone();
        DeadlineGuard::arm(duration, move || {
            eprintln!("[pi-agent-core] Runebench deadline elapsed; requesting cancellation");
            agent.abort();
        })
    });
    let result = smol::block_on(run.drive());
    let deadline_expired = deadline.as_ref().is_some_and(DeadlineGuard::expired);
    if let Some(deadline) = &mut deadline {
        deadline.disarm();
    }
    let totals = usage.snapshot();
    eprintln!(
        "[pi-agent-core] usage input={} output={} reasoning={}",
        totals.input_tokens.unwrap_or(0),
        totals.output_tokens.unwrap_or(0),
        totals.reasoning_tokens.unwrap_or(0)
    );
    if result.is_err() {
        if let Some(report) = usage.commandcode_error_report() {
            eprintln!("[pi-agent-core] commandcode_error {report}");
        }
    }
    match result {
        // Reaching the explicit host deadline is a normal Runebench outcome:
        // the core emitted its cancellation terminal events and the host exits
        // cleanly so Harbor can collect the world/verifier result.
        Err(CoreError::Cancelled) if deadline_expired => Ok(()),
        Ok(()) => Ok(()),
        Err(error) => Err(format!("pi-agent-core run failed: {error}")),
    }
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
    use super::{
        commandcode_request_context, curate_tool_result, mcp_client, openrouter_error, parse_model,
        parse_openrouter_response, wait_for_child_or_cancellation, write_curl_config, Args,
        DeadlineGuard, OpenAiContextHook, ProviderKind, RunebenchMcpCapability,
    };
    use pi_agent_core::hooks::HookSet;
    use pi_agent_core::scheduler::{CancellationToken, ModelStreamEvent};
    use pi_agent_core::state::{SerializedJson, ToolCallId};
    use pi_agent_core::tool::{
        AgentTool, ToolCall, ToolContext, ToolExecutionMode, ToolResult, ToolUpdateSink,
    };
    use pi_agent_luau::tool_handler::{CapabilityBindings, LuaToolHandler, ToolHandlerSpec};
    use pi_agent_luau::LuaPolicy;
    use pi_agent_protocol::JsonValue;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn commandcode_namespace_requires_explicit_host_context() {
        let (provider, model) =
            parse_model("commandcode/poolside/laguna-s-2.1-free").expect("model parses");
        assert_eq!(provider, ProviderKind::CommandCode);
        assert_eq!(model, "poolside/laguna-s-2.1-free");
        assert!(parse_model("poolside/laguna-s-2.1-free").is_err());

        let args = Args {
            model: "commandcode/poolside/laguna-s-2.1-free".into(),
            instruction: "test".into(),
            workspace: PathBuf::from("/app"),
            policy: PathBuf::from("/policy.luau"),
            log_jsonl: PathBuf::from("/logs/agent.jsonl"),
            run_deadline: None,
            commandcode_date: Some("2026-08-14".into()),
            commandcode_environment: Some("linux".into()),
            commandcode_thread_id: Some("b51a3243-2dd9-4c81-b659-a039645b7d4e".into()),
            commandcode_project_slug: Some("runebench".into()),
        };
        let context = commandcode_request_context(&args).expect("explicit context parses");
        assert_eq!(context.date, "2026-08-14");
        assert_eq!(context.environment, "linux");
        assert_eq!(context.project_slug, "runebench");
    }

    #[cfg(unix)]
    fn mcp_fixture_script(body: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "pi-agent-runebench-luau-mcp-{}-{}.sh",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after epoch")
                .as_nanos()
        ));
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("fixture script should be written");
        let mut permissions = fs::metadata(&path)
            .expect("fixture script metadata should be readable")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).expect("fixture script should be executable");
        path
    }

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
    fn execute_code_arguments_use_the_fixed_bot_and_host_timeout_name() {
        let arguments = JsonValue::parse(r#"{"code":"return sdk.getState()","timeout_minutes":5}"#)
            .expect("fixture arguments should parse");
        let normalized =
            RunebenchMcpCapability::normalize_tool_arguments("execute_code", &arguments)
                .expect("execute_code arguments should normalize");

        assert_eq!(
            normalized.get("bot_name").and_then(JsonValue::as_str),
            Some("agent")
        );
        assert_eq!(
            normalized.get("timeout").and_then(JsonValue::as_f64),
            Some(5.0)
        );
        assert!(normalized.get("timeout_minutes").is_none());
    }

    #[test]
    fn runebench_policy_exposes_only_the_compact_model_tool_set() {
        let policy = LuaPolicy::load(include_str!("../../../agents/runebench-policy.luau"))
            .expect("Runebench policy should load");
        let names = policy
            .tools()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["execute_code", "list_bots", "disconnect_bot"]);
        assert!(policy.system_prompt_append().contains("already included"));
    }

    #[test]
    fn model_tool_result_projection_includes_details_and_bounds_output() {
        let content = format!("head {} tail", "x".repeat(20_000));
        let details = SerializedJson::new(r#"{"retryable":false,"source":"mcp"}"#);
        let projected = curate_tool_result("execute_code", &content, Some(&details), true);

        assert!(projected.chars().count() <= super::MODEL_TOOL_RESULT_MAX_CHARS);
        assert!(projected.contains("[execute_code result: error]"));
        assert!(projected.contains("[structured details]"));
        assert!(projected.contains("[truncated"));
        assert!(projected.contains("tail"));
    }

    #[test]
    fn repeated_tool_failures_trip_the_host_circuit_breaker() {
        let hook = OpenAiContextHook::default();
        let call = ToolCall {
            id: ToolCallId::new("failure-call").expect("test call ID should be valid"),
            name: "execute_code".to_owned(),
            arguments: SerializedJson::new(r#"{"code":"return 1"}"#),
        };
        let result = ToolResult {
            tool_call_id: call.id.clone(),
            content: "Error: Game state not ready within timeout".to_owned(),
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: true,
        };

        assert_eq!(
            hook.after_tool_call(&call, &result)
                .expect("first failure should be handled")
                .terminate,
            None
        );
        assert_eq!(
            hook.after_tool_call(&call, &result)
                .expect("second failure should be handled")
                .terminate,
            None
        );
        assert_eq!(
            hook.after_tool_call(&call, &result)
                .expect("third failure should terminate")
                .terminate,
            Some(true)
        );
        assert!(matches!(
            hook.before_tool_call(&call),
            Ok(pi_agent_core::hooks::BeforeToolCall::Terminate { .. })
        ));
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

    #[test]
    fn deadline_invokes_its_expiration_callback_once() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut deadline = DeadlineGuard::arm(Duration::from_millis(5), move || {
            sender.send(()).expect("test receiver must remain open");
        });

        receiver
            .recv_timeout(Duration::from_millis(250))
            .expect("deadline should invoke its callback");
        assert!(deadline.expired());
        deadline.disarm();
    }

    #[test]
    fn disarmed_deadline_never_invokes_its_callback() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut deadline = DeadlineGuard::arm(Duration::from_millis(50), move || {
            sender.send(()).expect("test receiver must remain open");
        });

        deadline.disarm();
        assert!(!deadline.expired());
        assert!(receiver.recv_timeout(Duration::from_millis(75)).is_err());
    }

    #[test]
    fn cancellation_reaps_an_inflight_host_child() {
        let cancellation = CancellationToken::new();
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .expect("fixture child should start");
        cancellation.cancel();

        let (status, cancelled) = wait_for_child_or_cancellation(&mut child, Some(&cancellation))
            .expect("cancelled child should be reaped");

        assert!(cancelled);
        assert!(!status.success());
    }

    #[cfg(unix)]
    #[test]
    fn luau_tool_handler_reaches_only_the_bound_rust_mcp_capability() {
        let script = mcp_fixture_script(
            r#"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{},"serverInfo":{}}}' ;;
    *'"method":"tools/call"'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"handler ok"}],"isError":false}}' ;;
  esac
done
"#,
        );
        let cancellation = CancellationToken::new();
        let client = mcp_client::McpClient::connect(
            mcp_client::McpClientConfig::new(script.clone(), std::iter::empty()),
            &cancellation,
        )
        .expect("fixture MCP client should initialize");
        let capability = Arc::new(
            RunebenchMcpCapability::new(Arc::new(Mutex::new(client)))
                .expect("fixed rs-agent manifest should be valid"),
        );
        let mut bindings = CapabilityBindings::new();
        bindings
            .insert("rs-agent", capability)
            .expect("rs-agent capability binding should be unique");
        let handler = LuaToolHandler::new(
            r#"
                return function(call)
                    local result = coroutine.yield({
                        kind = "capability",
                        capability = "rs-agent",
                        method = "tools.call",
                        arguments_json = call.arguments_json,
                    })
                    return { content = result.content, is_error = result.is_error }
                end
            "#,
            ToolHandlerSpec {
                name: "execute_code".to_owned(),
                description: "fixture".to_owned(),
                schema: JsonValue::object([("type", "object".into())]),
                capability: "rs-agent".to_owned(),
                execution_mode: ToolExecutionMode::Sequential,
            },
            bindings,
        )
        .expect("handler should be valid before it reaches the registry");
        let result = smol::block_on(handler.execute(
            ToolCall {
                id: ToolCallId::new("fixture-call").expect("fixture call id should be valid"),
                name: "execute_code".to_owned(),
                arguments: SerializedJson::new(r#"{"bot_name":"agent","code":"noop"}"#),
            },
            ToolContext {
                cancellation,
                metadata: None,
            },
            ToolUpdateSink::disabled(),
        ))
        .expect("Luau handler should receive the MCP text result");
        assert_eq!(result.content, "handler ok");
        assert!(!result.is_error);
        drop(handler);
        let _ = fs::remove_file(script);
    }
}
