// Rust 1.97 | Windows x64 | KimiChatCLI Python-backed swarm MCP server.
use serde_json::{Value, json};
use std::{
    env,
    io::Write,
    process::{Command, Stdio},
    sync::{
        Mutex, OnceLock,
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const MAX_CHILDREN: usize = 10;
const DEFAULT_CHILDREN: usize = 3;
const DEFAULT_TIMEOUT_SECONDS: u64 = 180;
const MAX_TIMEOUT_SECONDS: u64 = 300;
const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const CREATE_NO_WINDOW: u32 = 0x08000000;

static PYTHON_PROGRAM: OnceLock<String> = OnceLock::new();
static PYTHON_HELPERS: OnceLock<Mutex<Vec<u32>>> = OnceLock::new();

// The deployed artifact remains one EXE. The Python worker pool is embedded
// into that EXE and passed to the installed Python runtime through -c.
const PYTHON_BACKEND_SOURCE: &str = include_str!("../python/swarm_backend.py");

pub fn stop_children() {
    let ids = PYTHON_HELPERS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .map(|mut helpers| std::mem::take(&mut *helpers))
        .unwrap_or_default();
    for process_id in ids {
        terminate_process_tree(process_id);
    }
}

pub fn handle_request(request: &Value) -> Option<Value> {
    if let Value::Array(items) = request {
        let responses = items.iter().filter_map(handle_one).collect::<Vec<_>>();
        return (!responses.is_empty()).then_some(Value::Array(responses));
    }
    handle_one(request)
}

fn handle_one(request: &Value) -> Option<Value> {
    let Some(object) = request.as_object() else {
        return Some(error_response(Value::Null, -32600, "Invalid Request."));
    };
    let request_id = object.get("id").cloned().unwrap_or(Value::Null);
    let has_id = object.contains_key("id");
    let method = object
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();

    if method.starts_with("notifications/") {
        return None;
    }
    if method == "initialize" {
        return has_id.then(|| {
            let protocol_version = object
                .get("params")
                .and_then(Value::as_object)
                .and_then(|params| params.get("protocolVersion"))
                .and_then(Value::as_str)
                .unwrap_or("2024-11-05");
            json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "protocolVersion": protocol_version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "kimichatcli-swarm", "version": "0.2.0"}
                }
            })
        });
    }
    if method == "ping" {
        return has_id.then(|| json!({"jsonrpc": "2.0", "id": request_id, "result": {}}));
    }
    if method == "tools/list" {
        return has_id.then(|| {
            json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {"tools": tool_catalog()}
            })
        });
    }
    if method == "tools/call" {
        return handle_tool_call(request_id, has_id, object.get("params"));
    }
    has_id.then(|| error_response(request_id, -32601, format!("Method '{method}' not found.")))
}

fn handle_tool_call(request_id: Value, has_id: bool, params: Option<&Value>) -> Option<Value> {
    let Some(params) = params.and_then(Value::as_object) else {
        return has_id
            .then(|| error_response(request_id, -32602, "tools/call params must be an object."));
    };
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return has_id
            .then(|| error_response(request_id, -32602, "tools/call requires a tool name."));
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let result = match name {
        "kimi_swarm_capabilities" => Ok(capabilities_result()),
        "kimi_swarm_spawn" => spawn_fanout(&arguments),
        _ => Err(format!("Unknown tool '{name}'.")),
    };
    let response = match result {
        Ok(value) => tool_result(value, false),
        Err(error) => tool_result(json!({"status": "error", "error": error}), true),
    };
    has_id.then(|| json!({"jsonrpc": "2.0", "id": request_id, "result": response}))
}

fn tool_catalog() -> Value {
    json!([
        {
            "name": "kimi_swarm_spawn",
            "description": "Fan out one task through the Python-backed Kimi CLI worker pool. Each child is a real local Kimi CLI session using the coder, explore, or plan profile. The result contains every child report, exit status, timing, and stderr. Children are instructed not to call MCP tools or create nested workers.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "The task sent independently to every child agent."},
                    "count": {"type": "integer", "minimum": 1, "maximum": 10, "default": 3, "description": "Number of parallel child sessions."},
                    "agent": {"type": "string", "enum": ["explore", "coder", "plan"], "default": "explore", "description": "Kimi's built-in subagent profile."},
                    "model": {"type": "string", "description": "Optional Kimi model id. Omit it to inherit the active Kimi CLI default."},
                    "working_directory": {"type": "string", "description": "Optional existing directory used as each child session's workspace."},
                    "timeout_seconds": {"type": "integer", "minimum": 15, "maximum": 300, "default": 180, "description": "Per-child wall-clock timeout."}
                },
                "required": ["prompt"],
                "additionalProperties": false
            }
        },
        {
            "name": "kimi_swarm_capabilities",
            "description": "Return the Python-backed Kimi CLI worker profiles and fan-out limits available through this controller.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }
    ])
}

#[allow(dead_code)]
pub fn model_tool_definitions() -> Vec<Value> {
    tool_catalog()
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            let name = tool.get("name").and_then(Value::as_str)?;
            let description = tool.get("description").and_then(Value::as_str)?;
            let parameters = tool.get("inputSchema")?.clone();
            Some(json!({
                "type": "function",
                "function": {
                    "name": format!("mcp__kimichatcli-swarm__{name}"),
                    "description": description,
                    "parameters": parameters
                }
            }))
        })
        .collect()
}

fn capabilities_result() -> Value {
    json!({
        "status": "ready",
        "backend": "KimiChatCLI Rust controller + embedded Python worker pool",
        "execution": "parallel local kimi CLI child processes coordinated by Python",
        "native_profiles": ["coder", "explore", "plan"],
        "max_parallel_children": MAX_CHILDREN,
        "default_parallel_children": DEFAULT_CHILDREN,
        "default_timeout_seconds": DEFAULT_TIMEOUT_SECONDS,
        "model_selection": "Omit model to inherit Kimi CLI's configured default; pass an exact model id to pin every child.",
        "recursion": "Children receive explicit no-MCP and no-nested-worker instructions.",
        "lifecycle": "The controller owns every Python helper PID and kills its process tree on shutdown."
    })
}

fn tool_result(value: Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": value,
        "isError": is_error
    })
}

fn spawn_fanout(arguments: &Value) -> Result<Value, String> {
    let object = arguments
        .as_object()
        .ok_or_else(|| "kimi_swarm_spawn arguments must be an object.".to_owned())?;
    let prompt = object
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or_else(|| "kimi_swarm_spawn requires a string prompt.".to_owned())?
        .trim();
    if prompt.is_empty() {
        return Err("kimi_swarm_spawn prompt cannot be empty.".to_owned());
    }
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(format!("prompt exceeds the {MAX_PROMPT_BYTES}-byte limit."));
    }

    let count = object
        .get("count")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_CHILDREN as u64) as usize;
    if !(1..=MAX_CHILDREN).contains(&count) {
        return Err(format!("count must be between 1 and {MAX_CHILDREN}."));
    }

    let timeout_seconds = object
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS);
    if !(15..=MAX_TIMEOUT_SECONDS).contains(&timeout_seconds) {
        return Err(format!(
            "timeout_seconds must be between 15 and {MAX_TIMEOUT_SECONDS}."
        ));
    }

    let request =
        serde_json::to_vec(arguments).map_err(|error| format!("encode request: {error}"))?;
    let python = PYTHON_PROGRAM.get_or_init(resolve_python_program).clone();
    let mut command = Command::new(&python);
    command
        .args(["-c", PYTHON_BACKEND_SOURCE])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("PYTHONUNBUFFERED", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .env("KIMICHATCLI_SWARM_HELPER", "1");
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);

    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn Python swarm backend '{python}': {error}"))?;
    let process_id = child.id();
    register_helper(process_id);

    let write_result = child
        .stdin
        .take()
        .ok_or_else(|| "Python swarm backend stdin was unavailable.".to_owned())
        .and_then(|mut stdin| {
            stdin
                .write_all(&request)
                .and_then(|_| stdin.write_all(b"\n"))
                .map_err(|error| format!("write Python swarm request: {error}"))
        });
    if let Err(error) = write_result {
        terminate_process_tree(process_id);
        unregister_helper(process_id);
        return Err(error);
    }

    let started = Instant::now();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(child.wait_with_output());
    });

    let response = match receiver.recv_timeout(Duration::from_secs(timeout_seconds + 30)) {
        Ok(Ok(output)) => parse_backend_output(output),
        Ok(Err(error)) => Err(format!("collect Python swarm backend output: {error}")),
        Err(RecvTimeoutError::Timeout) => {
            terminate_process_tree(process_id);
            match receiver.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(output)) => parse_backend_output(output),
                Ok(Err(error)) => Err(format!("collect timed-out Python backend: {error}")),
                Err(_) => Err(format!(
                    "Python swarm backend timed out after {} seconds.",
                    timeout_seconds + 30
                )),
            }
        }
        Err(RecvTimeoutError::Disconnected) => {
            Err("Python swarm backend output collector disconnected.".to_owned())
        }
    };
    unregister_helper(process_id);

    response.map(|mut value| {
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "controller_elapsed_ms".to_owned(),
                json!(started.elapsed().as_millis()),
            );
        }
        value
    })
}

fn parse_backend_output(output: std::process::Output) -> Result<Value, String> {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = truncate_text(&String::from_utf8_lossy(&output.stderr));
    if stdout.is_empty() {
        return Err(if stderr.is_empty() {
            format!(
                "Python swarm backend exited with status {} and no response.",
                output.status
            )
        } else {
            format!("Python swarm backend produced no response: {stderr}")
        });
    }
    let value: Value = serde_json::from_str(&stdout).map_err(|error| {
        format!("decode Python swarm backend response: {error}; stderr: {stderr}")
    })?;
    if value.get("status").and_then(Value::as_str) == Some("error") {
        return Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Python swarm backend rejected the request.")
            .to_owned());
    }
    if !output.status.success() {
        return Err(format!(
            "Python swarm backend exited with status {}; response: {}; stderr: {}",
            output.status, stdout, stderr
        ));
    }
    Ok(value)
}

fn register_helper(process_id: u32) {
    if let Ok(mut helpers) = PYTHON_HELPERS.get_or_init(|| Mutex::new(Vec::new())).lock()
        && !helpers.contains(&process_id)
    {
        helpers.push(process_id);
    }
}

fn unregister_helper(process_id: u32) {
    if let Ok(mut helpers) = PYTHON_HELPERS.get_or_init(|| Mutex::new(Vec::new())).lock() {
        helpers.retain(|id| *id != process_id);
    }
}

fn terminate_process_tree(process_id: u32) {
    let _ = Command::new("taskkill.exe")
        .args(["/PID", &process_id.to_string(), "/T", "/F"])
        .output();
}

fn truncate_text(value: &str) -> String {
    if value.len() <= MAX_OUTPUT_BYTES {
        return value.trim().to_owned();
    }
    let mut end = MAX_OUTPUT_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[output truncated at {MAX_OUTPUT_BYTES} bytes]",
        value[..end].trim()
    )
}

fn resolve_python_program() -> String {
    if let Some(path) = env::var_os("KIMICHATCLI_PYTHON").filter(|value| !value.is_empty()) {
        return path.to_string_lossy().into_owned();
    }
    if let Ok(output) = Command::new("where.exe").arg("python.exe").output()
        && output.status.success()
        && let Some(path) = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.contains("WindowsApps"))
    {
        return path.to_owned();
    }
    "python.exe".to_owned()
}

fn error_response(id: Value, code: i32, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

#[cfg(test)]
mod tests {
    use super::{handle_request, tool_catalog};
    use serde_json::json;

    #[test]
    fn initialize_preserves_supported_protocol_version() {
        let response = handle_request(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05"}
        }))
        .expect("initialize response");
        assert_eq!(response["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(
            response["result"]["serverInfo"]["name"],
            "kimichatcli-swarm"
        );
    }

    #[test]
    fn catalog_exposes_python_backend_and_bounded_fanout() {
        let catalog = tool_catalog();
        assert_eq!(catalog.as_array().map(Vec::len), Some(2));
        assert!(catalog.to_string().contains("Python-backed"));
        assert_eq!(
            catalog[0]["inputSchema"]["properties"]["count"]["maximum"],
            10
        );
    }

    #[test]
    fn notifications_do_not_create_responses() {
        assert!(
            handle_request(&json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }))
            .is_none()
        );
    }
}
