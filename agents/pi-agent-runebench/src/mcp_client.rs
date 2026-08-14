//! A small, capability-scoped MCP stdio client for the Runebench host.
//!
//! The client owns exactly one fixed child process.  It clears the child's
//! environment, grants only the host's deterministic `PATH`/`HOME`, and
//! exposes the narrow MCP operations that the Runebench world needs.  The
//! client deliberately does not expose a general MCP configuration loader,
//! network transport, or arbitrary process spawning.
//!
//! The stdout and stderr streams are private capture files rather than pipes.
//! This is important for Runebench: a world tool may intentionally leave a
//! detached game worker alive after the direct MCP child exits.  A pipe would
//! remain open through that descendant and make a synchronous client wait
//! forever.  The direct child is still polled and reaped on cancellation.

use pi_agent_core::scheduler::CancellationToken;
use pi_agent_protocol::{JsonNumber, JsonValue};
use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

const DEFAULT_PATH: &str = "/root/.bun/bin:/usr/local/bin:/usr/bin:/bin";
const DEFAULT_HOME: &str = "/root";
const MCP_PROTOCOL_VERSION: &str = "2025-03-26";
const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
static CAPTURE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

/// Configuration for the one MCP process owned by [`McpClient`].
///
/// [`Default`] is the Runebench production command.  Tests and other local
/// hosts can use [`Self::new`] with a fixture executable, while retaining the
/// same environment-clearing and process-lifetime rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpClientConfig {
    /// Executable to run.  Production uses `bun`.
    pub command: PathBuf,
    /// Arguments passed to the executable.  Production is `run
    /// /app/mcp/server.ts`.
    pub args: Vec<OsString>,
    /// The only `PATH` value inherited by the child after `env_clear`.
    pub path: OsString,
    /// Optional minimal `HOME` value.  Production uses `/root`.
    pub home: Option<OsString>,
}

impl McpClientConfig {
    /// Construct a command with the production environment policy.
    pub fn new(command: impl Into<PathBuf>, args: impl IntoIterator<Item = OsString>) -> Self {
        Self {
            command: command.into(),
            args: args.into_iter().collect(),
            path: OsString::from(DEFAULT_PATH),
            home: Some(OsString::from(DEFAULT_HOME)),
        }
    }

    /// Return the fixed Runebench rs-agent MCP server command.
    pub fn runebench() -> Self {
        Self::new(
            PathBuf::from("bun"),
            [OsString::from("run"), OsString::from("/app/mcp/server.ts")],
        )
    }
}

impl Default for McpClientConfig {
    fn default() -> Self {
        Self::runebench()
    }
}

/// A typed error from process startup, transport, protocol validation, or the
/// MCP server itself.
#[derive(Debug, Clone, PartialEq)]
pub enum McpError {
    /// The operation observed its caller's cancellation scope.
    Cancelled,
    /// The child could not be started or a stream operation failed.
    Io {
        /// Operation being performed.
        operation: String,
        /// Redacted standard-library diagnostic.
        message: String,
    },
    /// The direct MCP child exited before the expected response arrived.
    ProcessExited {
        /// Child exit code, if one was available.
        status: Option<i32>,
        /// Captured stderr, if any.
        stderr: String,
    },
    /// The server returned a JSON-RPC error object.
    Server {
        /// JSON-RPC error code.
        code: i64,
        /// Server-provided diagnostic.
        message: String,
        /// Optional server error data.
        data: Option<JsonValue>,
    },
    /// The wire message was not valid MCP JSON-RPC for the requested call.
    Protocol {
        /// Stable description of the violated response invariant.
        message: String,
    },
    /// A caller supplied an invalid MCP argument shape.
    InvalidArgument {
        /// Stable description of the invalid argument.
        message: String,
    },
}

impl fmt::Display for McpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("MCP operation cancelled"),
            Self::Io { operation, message } => {
                write!(formatter, "MCP {operation} failed: {message}")
            }
            Self::ProcessExited { status, stderr } => {
                write!(formatter, "MCP child exited with {status:?}")?;
                if !stderr.is_empty() {
                    write!(formatter, ": {stderr}")?;
                }
                Ok(())
            }
            Self::Server { code, message, .. } => {
                write!(formatter, "MCP server error {code}: {message}")
            }
            Self::Protocol { message } => write!(formatter, "MCP protocol error: {message}"),
            Self::InvalidArgument { message } => {
                write!(formatter, "invalid MCP argument: {message}")
            }
        }
    }
}

impl std::error::Error for McpError {}

/// The result returned by MCP `initialize`.
#[derive(Clone, Debug, PartialEq)]
pub struct InitializeResult {
    /// Protocol version selected by the server.
    pub protocol_version: String,
    /// Server capability object, retained as protocol JSON.
    pub capabilities: JsonValue,
    /// Server information object, retained as protocol JSON.
    pub server_info: JsonValue,
}

/// One MCP resource descriptor.
#[derive(Clone, Debug, PartialEq)]
pub struct Resource {
    /// Resource URI.
    pub uri: String,
    /// Optional human-readable name.
    pub name: Option<String>,
    /// Optional description.
    pub description: Option<String>,
    /// Optional MIME type.
    pub mime_type: Option<String>,
}

/// The result returned by MCP `resources/list`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ResourceList {
    /// Resources advertised by the server.
    pub resources: Vec<Resource>,
    /// Cursor for a subsequent page, if the server returned one.
    pub next_cursor: Option<String>,
}

/// The result returned by MCP `resources/read`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ResourceReadResult {
    /// Raw content entries from the MCP result.
    pub contents: Vec<JsonValue>,
}

impl ResourceReadResult {
    /// Convert text/resource content parts into the prompt-facing text used
    /// by the existing Runebench bridge.
    pub fn content_text(&self) -> Result<String, McpError> {
        content_parts_to_text(&self.contents)
    }
}

/// The result returned by MCP `tools/call`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ToolCallResult {
    /// Raw MCP content entries.
    pub content: Vec<JsonValue>,
    /// Whether the server marked the tool result as an MCP tool error.
    pub is_error: bool,
    /// Optional structured result supplied by the server.
    pub structured_content: Option<JsonValue>,
}

impl ToolCallResult {
    /// Convert text/resource content parts into a compact tool result string.
    pub fn content_text(&self) -> Result<String, McpError> {
        content_parts_to_text(&self.content)
    }
}

/// A synchronous, single-request-at-a-time MCP client.
///
/// The client is intentionally not `Clone` or `Sync`: one owner controls one
/// child's stdin, response sequence, and reaping boundary.  A host can wrap it
/// in its own serialized capability adapter when exposing it to Luau tools.
pub struct McpClient {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    stdout_offset: usize,
    next_id: u64,
    initialized: bool,
}

impl McpClient {
    /// Start the fixed Runebench MCP server and complete MCP initialization.
    pub fn connect_default(cancellation: &CancellationToken) -> Result<Self, McpError> {
        Self::connect(McpClientConfig::runebench(), cancellation)
    }

    /// Start a configured MCP server and complete MCP initialization.
    pub fn connect(
        config: McpClientConfig,
        cancellation: &CancellationToken,
    ) -> Result<Self, McpError> {
        let (stdout_path, stdout) = capture_file("mcp", "stdout")?;
        let (stderr_path, stderr) = match capture_file("mcp", "stderr") {
            Ok(capture) => capture,
            Err(error) => {
                let _ = fs::remove_file(&stdout_path);
                return Err(error);
            }
        };
        let child = {
            let mut command = Command::new(&config.command);
            command
                .args(&config.args)
                .env_clear()
                .env("PATH", &config.path)
                .stdin(Stdio::piped())
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr));
            if let Some(home) = &config.home {
                command.env("HOME", home);
            }
            command.spawn().map_err(|error| McpError::Io {
                operation: format!("start MCP command {:?}", config.command),
                message: error.to_string(),
            })
        };
        let mut child = match child {
            Ok(child) => child,
            Err(error) => {
                let _ = fs::remove_file(&stdout_path);
                let _ = fs::remove_file(&stderr_path);
                return Err(error);
            }
        };
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                terminate_child(&mut child);
                let _ = fs::remove_file(&stdout_path);
                let _ = fs::remove_file(&stderr_path);
                return Err(McpError::Protocol {
                    message: "MCP child did not expose stdin".to_owned(),
                });
            }
        };
        let mut client = Self {
            child,
            stdin,
            stdout_path,
            stderr_path,
            stdout_offset: 0,
            next_id: 1,
            initialized: false,
        };
        client.initialize(cancellation)?;
        Ok(client)
    }

    /// Complete the MCP `initialize`/`notifications/initialized` handshake.
    pub fn initialize(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<InitializeResult, McpError> {
        if cancellation.is_cancelled() {
            self.terminate();
            return Err(McpError::Cancelled);
        }
        let result = self.request(
            "initialize",
            JsonValue::object([
                ("protocolVersion", MCP_PROTOCOL_VERSION.into()),
                ("capabilities", empty_object()),
                (
                    "clientInfo",
                    JsonValue::object([
                        ("name", "pi-agent-core-runebench".into()),
                        ("version", "0.1.0".into()),
                    ]),
                ),
            ]),
            cancellation,
        )?;
        let protocol_version =
            required_string(result.get("protocolVersion"), "initialize.protocolVersion")?;
        let capabilities = result
            .get("capabilities")
            .cloned()
            .unwrap_or_else(empty_object);
        let server_info = result
            .get("serverInfo")
            .cloned()
            .unwrap_or_else(empty_object);
        self.notify("notifications/initialized", empty_object(), cancellation)?;
        self.initialized = true;
        Ok(InitializeResult {
            protocol_version: protocol_version.to_owned(),
            capabilities,
            server_info,
        })
    }

    /// Call one capability-scoped MCP tool.
    pub fn tools_call(
        &mut self,
        name: &str,
        arguments: &JsonValue,
        cancellation: &CancellationToken,
    ) -> Result<ToolCallResult, McpError> {
        if !matches!(arguments, JsonValue::Object(_)) {
            return Err(McpError::InvalidArgument {
                message: "tools/call arguments must be a JSON object".to_owned(),
            });
        }
        let result = self.request(
            "tools/call",
            JsonValue::object([("name", name.into()), ("arguments", arguments.clone())]),
            cancellation,
        )?;
        let content = match result.get("content") {
            None | Some(JsonValue::Null) => Vec::new(),
            Some(JsonValue::Array(values)) => values.clone(),
            Some(_) => {
                return Err(McpError::Protocol {
                    message: "tools/call.content was not an array".to_owned(),
                })
            }
        };
        let is_error = match result.get("isError") {
            None | Some(JsonValue::Null) => false,
            Some(JsonValue::Bool(value)) => *value,
            Some(_) => {
                return Err(McpError::Protocol {
                    message: "tools/call.isError was not a boolean".to_owned(),
                })
            }
        };
        Ok(ToolCallResult {
            content,
            is_error,
            structured_content: result.get("structuredContent").cloned(),
        })
    }

    /// List resources exposed by the capability-scoped server.
    pub fn resources_list(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<ResourceList, McpError> {
        let result = self.request("resources/list", empty_object(), cancellation)?;
        let resources = match result.get("resources") {
            None | Some(JsonValue::Null) => Vec::new(),
            Some(JsonValue::Array(values)) => values
                .iter()
                .map(parse_resource)
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => {
                return Err(McpError::Protocol {
                    message: "resources/list.resources was not an array".to_owned(),
                })
            }
        };
        let next_cursor = optional_string(result.get("nextCursor"))?.map(ToOwned::to_owned);
        Ok(ResourceList {
            resources,
            next_cursor,
        })
    }

    /// Read one resource URI exposed by the capability-scoped server.
    pub fn resources_read(
        &mut self,
        uri: &str,
        cancellation: &CancellationToken,
    ) -> Result<ResourceReadResult, McpError> {
        if uri.is_empty() {
            return Err(McpError::InvalidArgument {
                message: "resources/read uri must not be empty".to_owned(),
            });
        }
        let result = self.request(
            "resources/read",
            JsonValue::object([("uri", uri.into())]),
            cancellation,
        )?;
        let contents = match result.get("contents") {
            None | Some(JsonValue::Null) => Vec::new(),
            Some(JsonValue::Array(values)) => values.clone(),
            Some(_) => {
                return Err(McpError::Protocol {
                    message: "resources/read.contents was not an array".to_owned(),
                })
            }
        };
        Ok(ResourceReadResult { contents })
    }

    fn notify(
        &mut self,
        method: &str,
        params: JsonValue,
        cancellation: &CancellationToken,
    ) -> Result<(), McpError> {
        if cancellation.is_cancelled() {
            self.terminate();
            return Err(McpError::Cancelled);
        }
        let message = JsonValue::object([
            ("jsonrpc", "2.0".into()),
            ("method", method.into()),
            ("params", params),
        ]);
        self.write_message(&message)
    }

    fn request(
        &mut self,
        method: &str,
        params: JsonValue,
        cancellation: &CancellationToken,
    ) -> Result<JsonValue, McpError> {
        if !self.initialized && method != "initialize" {
            return Err(McpError::Protocol {
                message: "MCP request attempted before initialization".to_owned(),
            });
        }
        if cancellation.is_cancelled() {
            self.terminate();
            return Err(McpError::Cancelled);
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| McpError::Protocol {
                message: "MCP request identifier exhausted".to_owned(),
            })?;
        let message = JsonValue::object([
            ("jsonrpc", "2.0".into()),
            ("id", JsonValue::from(id)),
            ("method", method.into()),
            ("params", params),
        ]);
        self.write_message(&message)?;
        self.wait_for_response(id, cancellation)
    }

    fn write_message(&mut self, message: &JsonValue) -> Result<(), McpError> {
        let text = message
            .to_json_string()
            .map_err(|error| McpError::Protocol {
                message: format!("cannot encode request: {error}"),
            })?;
        self.stdin
            .write_all(text.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .map_err(|error| McpError::Io {
                operation: "write MCP request".to_owned(),
                message: error.to_string(),
            })
    }

    fn wait_for_response(
        &mut self,
        id: u64,
        cancellation: &CancellationToken,
    ) -> Result<JsonValue, McpError> {
        loop {
            if cancellation.is_cancelled() {
                self.terminate();
                return Err(McpError::Cancelled);
            }
            if let Some(response) = self.scan_responses(id)? {
                return Ok(response);
            }
            if let Some(status) = self.child.try_wait().map_err(|error| McpError::Io {
                operation: "poll MCP child".to_owned(),
                message: error.to_string(),
            })? {
                if let Some(response) = self.scan_responses(id)? {
                    return Ok(response);
                }
                return Err(McpError::ProcessExited {
                    status: status.code(),
                    stderr: self.read_stderr(),
                });
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn scan_responses(&mut self, expected_id: u64) -> Result<Option<JsonValue>, McpError> {
        let bytes = fs::read(&self.stdout_path).map_err(|error| McpError::Io {
            operation: "read MCP response capture".to_owned(),
            message: error.to_string(),
        })?;
        if bytes.len().saturating_sub(self.stdout_offset) > MAX_MESSAGE_BYTES {
            return Err(McpError::Protocol {
                message: format!("MCP response exceeded {MAX_MESSAGE_BYTES} bytes"),
            });
        }
        while self.stdout_offset < bytes.len() {
            let remaining = &bytes[self.stdout_offset..];
            let Some(newline) = remaining.iter().position(|byte| *byte == b'\n') else {
                break;
            };
            let line_end = self.stdout_offset + newline + 1;
            let line = std::str::from_utf8(&bytes[self.stdout_offset..line_end])
                .map_err(|_| McpError::Protocol {
                    message: "MCP response was not UTF-8".to_owned(),
                })?
                .trim();
            self.stdout_offset = line_end;
            if line.is_empty() {
                continue;
            }
            let response = JsonValue::parse(line).map_err(|error| McpError::Protocol {
                message: format!("MCP response was not JSON: {error}"),
            })?;
            let Some(response_id) = response_id(&response)? else {
                // Server notifications are legal between request and response.
                continue;
            };
            if response_id != expected_id {
                return Err(McpError::Protocol {
                    message: format!(
                        "MCP response id {response_id} did not match request {expected_id}"
                    ),
                });
            }
            if let Some(error) = response.get("error") {
                return Err(parse_server_error(error));
            }
            return response
                .get("result")
                .cloned()
                .map(Some)
                .ok_or_else(|| McpError::Protocol {
                    message: "MCP response omitted both result and error".to_owned(),
                });
        }
        Ok(None)
    }

    fn read_stderr(&self) -> String {
        fs::read_to_string(&self.stderr_path)
            .unwrap_or_default()
            .trim()
            .to_owned()
    }

    fn terminate(&mut self) {
        terminate_child(&mut self.child);
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.terminate();
        let _ = fs::remove_file(&self.stdout_path);
        let _ = fs::remove_file(&self.stderr_path);
    }
}

fn capture_file(operation: &str, stream: &str) -> Result<(PathBuf, File), McpError> {
    for _ in 0..16 {
        let sequence = CAPTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "pi-agent-runebench-{operation}-{}-{sequence}-{stream}",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(McpError::Io {
                    operation: format!("create private MCP {stream} capture"),
                    message: error.to_string(),
                })
            }
        }
    }
    Err(McpError::Io {
        operation: format!("create private MCP {stream} capture"),
        message: "could not allocate a unique capture path".to_owned(),
    })
}

/// Poll and reap only the direct child.  Detached world workers deliberately
/// survive this boundary, matching the host's existing cancellation policy.
fn terminate_child(child: &mut Child) {
    if let Ok(Some(_)) = child.try_wait() { return }
    let _ = child.kill();
    let _ = child.wait();
}

fn response_id(value: &JsonValue) -> Result<Option<u64>, McpError> {
    let Some(id) = value.get("id") else {
        return Ok(None);
    };
    match id {
        JsonValue::Number(JsonNumber::Unsigned(value)) => Ok(Some(*value)),
        JsonValue::Number(JsonNumber::Signed(value)) if *value >= 0 => Ok(Some(*value as u64)),
        JsonValue::Null => Ok(None),
        _ => Err(McpError::Protocol {
            message: "MCP response id was not an unsigned integer".to_owned(),
        }),
    }
}

fn parse_server_error(value: &JsonValue) -> McpError {
    let code = match value.get("code") {
        Some(JsonValue::Number(JsonNumber::Signed(value))) => *value,
        Some(JsonValue::Number(JsonNumber::Unsigned(value))) => {
            (*value).min(i64::MAX as u64) as i64
        }
        _ => 0,
    };
    let message = optional_string(value.get("message"))
        .ok()
        .flatten()
        .unwrap_or("MCP server rejected the request")
        .to_owned();
    McpError::Server {
        code,
        message,
        data: value.get("data").cloned(),
    }
}

fn parse_resource(value: &JsonValue) -> Result<Resource, McpError> {
    Ok(Resource {
        uri: required_string(value.get("uri"), "resource.uri")?.to_owned(),
        name: optional_string(value.get("name"))?.map(ToOwned::to_owned),
        description: optional_string(value.get("description"))?.map(ToOwned::to_owned),
        mime_type: optional_string(value.get("mimeType"))?.map(ToOwned::to_owned),
    })
}

fn content_parts_to_text(parts: &[JsonValue]) -> Result<String, McpError> {
    let mut output = Vec::new();
    for part in parts {
        let text = match part {
            JsonValue::Object(object) => match object.get("type") {
                Some(JsonValue::String(kind)) if kind == "text" => {
                    optional_string(object.get("text"))?
                        .unwrap_or_default()
                        .to_owned()
                }
                Some(JsonValue::String(kind)) if kind == "resource" => object
                    .get("resource")
                    .and_then(|resource| resource.get("text"))
                    .and_then(|value| match value {
                        JsonValue::String(text) => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| json_text(part)),
                Some(JsonValue::String(_)) | None => object
                    .get("text")
                    .and_then(|value| match value {
                        JsonValue::String(text) => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| json_text(part)),
                Some(_) => json_text(part),
            },
            _ => json_text(part),
        };
        if !text.is_empty() {
            output.push(text);
        }
    }
    Ok(output.join("\n"))
}

fn json_text(value: &JsonValue) -> String {
    value
        .to_json_string()
        .unwrap_or_else(|_| "<unserializable MCP content>".to_owned())
}

fn empty_object() -> JsonValue {
    JsonValue::Object(std::collections::BTreeMap::new())
}

fn optional_string(value: Option<&JsonValue>) -> Result<Option<&str>, McpError> {
    match value {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::String(value)) => Ok(Some(value)),
        Some(_) => Err(McpError::Protocol {
            message: "MCP response field was not a string".to_owned(),
        }),
    }
}

fn required_string<'a>(value: Option<&'a JsonValue>, field: &str) -> Result<&'a str, McpError> {
    optional_string(value)?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| McpError::Protocol {
            message: format!("MCP response omitted {field}"),
        })
}

#[cfg(all(test, unix))]
mod tests {
    use super::{McpClient, McpClientConfig, McpError};
    use pi_agent_core::scheduler::CancellationToken;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    fn fixture_script(body: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "pi-agent-mcp-fixture-{}-{}.sh",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock should be after epoch")
                .as_nanos()
        ));
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("fixture should be written");
        let mut permissions = fs::metadata(&path)
            .expect("fixture metadata should be readable")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).expect("fixture should be executable");
        path
    }

    fn fixture_config(path: PathBuf) -> McpClientConfig {
        McpClientConfig::new(path, std::iter::empty())
    }

    #[test]
    fn handshake_and_capability_calls_use_typed_results() {
        let script = fixture_script(
            r#"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{},"serverInfo":{"name":"fixture","version":"1"}}}' ;;
    *'"method":"tools/call"'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"tool ok"}],"isError":false}}' ;;
    *'"method":"resources/list"'*) printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"resources":[{"uri":"fixture://docs","name":"docs","mimeType":"text/plain"}]}}' ;;
    *'"method":"resources/read"'*) printf '%s\n' '{"jsonrpc":"2.0","id":4,"result":{"contents":[{"uri":"fixture://docs","mimeType":"text/plain","text":"resource ok"}]}}' ;;
  esac
done
"#,
        );
        let token = CancellationToken::new();
        let mut client = McpClient::connect(fixture_config(script.clone()), &token)
            .expect("fixture MCP server should initialize");
        let tool = client
            .tools_call("execute_code", &super::empty_object(), &token)
            .expect("tool call should succeed");
        assert_eq!(
            tool.content_text().expect("tool text should decode"),
            "tool ok"
        );
        assert!(!tool.is_error);
        let resources = client
            .resources_list(&token)
            .expect("resource list should succeed");
        assert_eq!(resources.resources[0].uri, "fixture://docs");
        let resource = client
            .resources_read("fixture://docs", &token)
            .expect("resource read should succeed");
        assert_eq!(
            resource
                .content_text()
                .expect("resource text should decode"),
            "resource ok"
        );
        drop(client);
        let _ = fs::remove_file(script);
    }

    #[test]
    fn cancellation_reaps_direct_server_promptly() {
        let script = fixture_script(
            r#"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{},"serverInfo":{}}}' ;;
    *'"method":"tools/call"'*) sleep 30 ;;
  esac
done
"#,
        );
        let token = CancellationToken::new();
        let client = McpClient::connect(fixture_config(script.clone()), &token)
            .expect("fixture MCP server should initialize");
        let started = Instant::now();
        let call_token = token.clone();
        let handle = std::thread::spawn(move || {
            let mut client = client;
            client
                .tools_call("execute_code", &super::empty_object(), &call_token)
                .expect_err("cancelled call should fail")
        });
        std::thread::sleep(Duration::from_millis(50));
        token.cancel();
        let error = handle.join().expect("MCP call thread should settle");
        assert_eq!(error, McpError::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(1));
        let _ = fs::remove_file(script);
    }
}
