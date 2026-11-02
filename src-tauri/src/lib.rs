use serde::{Deserialize, Serialize};
use std::{
    env,
    io::{BufRead, Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{atomic::{AtomicU64, Ordering}, Arc, Mutex},
};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::{io::{AsyncBufReadExt, AsyncWriteExt, BufReader}, net::TcpListener, process::Command, sync::oneshot};

static PERMISSION_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Serialize)]
struct CliStatus {
    available: bool,
    path: Option<String>,
    version: Option<String>,
    message: String,
}

#[derive(Default)]
struct AppState {
    active_process: Arc<Mutex<Option<(String, i32)>>>,
    pending_permissions: Arc<Mutex<std::collections::HashMap<String, (String, oneshot::Sender<PermissionDecision>)>>>,
}

#[derive(Clone, Copy)]
enum PermissionDecision { Allow, Deny }

#[derive(Serialize, Clone)]
struct PermissionRequest {
    permission_id: String,
    turn_id: String,
    tool_name: String,
    input: serde_json::Value,
}

#[derive(Serialize)]
struct AuthStatus {
    logged_in: bool,
    auth_method: Option<String>,
    message: String,
}

#[derive(Serialize, Clone)]
struct GitFileStatus {
    path: String,
    index_status: String,
    worktree_status: String,
}

#[derive(Serialize)]
struct GitStatus {
    available: bool,
    branch: Option<String>,
    files: Vec<GitFileStatus>,
    message: String,
}

#[derive(Serialize, Clone)]
struct TurnEvent {
    turn_id: String,
    kind: String,
    text: Option<String>,
    session_id: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct ProviderConfig {
    id: String,
    name: String,
    base_url: String,
    models: Vec<String>,
    default_model: String,
    has_api_key: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderInput {
    id: String,
    name: String,
    base_url: String,
    models: Vec<String>,
    default_model: String,
    api_key: String,
}

fn providers_file(app: &AppHandle) -> Result<PathBuf, String> {
    let directory = app.path().app_config_dir().map_err(|error| format!("Could not locate AgentDeck settings: {error}"))?;
    std::fs::create_dir_all(&directory).map_err(|error| format!("Could not create AgentDeck settings folder: {error}"))?;
    Ok(directory.join("providers.json"))
}

fn read_providers(app: &AppHandle) -> Result<Vec<ProviderConfig>, String> {
    let path = providers_file(app)?;
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| format!("Could not read provider settings: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!("Could not read provider settings: {error}")),
    }
}

fn write_providers(app: &AppHandle, providers: &[ProviderConfig]) -> Result<(), String> {
    let path = providers_file(app)?;
    let bytes = serde_json::to_vec_pretty(providers).map_err(|error| format!("Could not encode provider settings: {error}"))?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes).map_err(|error| format!("Could not save provider settings: {error}"))?;
    std::fs::rename(temporary, path).map_err(|error| format!("Could not finish saving provider settings: {error}"))
}

fn secret_entry(provider_id: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new("local.agentdeck.app", &format!("provider:{provider_id}"))
        .map_err(|error| format!("Could not access the desktop keyring: {error}"))
}

fn validate_base_url(value: &str) -> Result<String, String> {
    let parsed = url::Url::parse(value.trim()).map_err(|_| "Enter a valid provider base URL.".to_string())?;
    if !matches!(parsed.scheme(), "https" | "http") || parsed.username() != "" || parsed.password().is_some() || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("Use an http(s) URL without embedded credentials, query, or fragment.".into());
    }
    let host = parsed.host_str().unwrap_or_default();
    let local_http = host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|address| address.is_loopback());
    if parsed.scheme() != "https" && !local_http {
        return Err("Remote provider URLs must use HTTPS. HTTP is allowed only for localhost.".into());
    }
    Ok(value.trim().trim_end_matches('/').to_string())
}

#[tauri::command]
fn get_providers(app: AppHandle) -> Result<Vec<ProviderConfig>, String> {
    read_providers(&app)
}

#[tauri::command]
fn save_provider(app: AppHandle, input: ProviderInput) -> Result<Vec<ProviderConfig>, String> {
    if input.id.is_empty() || input.id.len() > 80 || !input.id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_') {
        return Err("Invalid provider identifier.".into());
    }
    let name = input.name.trim();
    if name.is_empty() || name.len() > 80 { return Err("Provider name must be 1 to 80 characters.".into()); }
    let base_url = validate_base_url(&input.base_url)?;
    let mut models: Vec<String> = input.models.into_iter().map(|model| model.trim().to_string()).filter(|model| !model.is_empty()).collect();
    models.dedup();
    if models.is_empty() || models.len() > 50 || models.iter().any(|model| model.len() > 200) {
        return Err("Add between 1 and 50 model IDs (each no longer than 200 characters).".into());
    }
    let default_model = input.default_model.trim().to_string();
    if !models.iter().any(|model| model == &default_model) {
        return Err("Choose the default model from the model list.".into());
    }

    let mut providers = read_providers(&app)?;
    let existing = providers.iter().find(|provider| provider.id == input.id).cloned();
    let key = input.api_key.trim();
    let has_api_key = if !key.is_empty() {
        secret_entry(&input.id)?.set_password(key).map_err(|error| format!("Could not save the API key to the desktop keyring: {error}"))?;
        true
    } else {
        existing.as_ref().is_some_and(|provider| provider.has_api_key)
    };
    if !has_api_key { return Err("Enter an API key for this provider.".into()); }

    let provider = ProviderConfig {
        id: input.id,
        name: name.to_string(),
        base_url,
        models,
        default_model,
        has_api_key,
    };
    if let Some(index) = providers.iter().position(|item| item.id == provider.id) {
        providers[index] = provider;
    } else {
        providers.push(provider);
    }
    write_providers(&app, &providers)?;
    Ok(providers)
}

#[tauri::command]
fn delete_provider(app: AppHandle, provider_id: String) -> Result<Vec<ProviderConfig>, String> {
    let mut providers = read_providers(&app)?;
    let removed = providers.iter().any(|provider| provider.id == provider_id);
    providers.retain(|provider| provider.id != provider_id);
    write_providers(&app, &providers)?;
    if removed { let _ = secret_entry(&provider_id).and_then(|entry| entry.delete_credential().map_err(|error| error.to_string())); }
    Ok(providers)
}

fn provider_api_key(input: &ProviderInput) -> Result<String, String> {
    let entered = input.api_key.trim();
    let key = if !entered.is_empty() { entered.to_string() } else {
        secret_entry(&input.id)?.get_password()
            .map_err(|_| "Enter an API key, or save this provider first so AgentDeck can read its key from the desktop keyring.".to_string())?
    };
    if key.contains('\r') || key.contains('\n') { return Err("API key cannot contain line breaks.".into()); }
    Ok(key)
}

fn provider_endpoint(base_url: &str, resource: &str) -> Result<url::Url, String> {
    let mut url = url::Url::parse(&validate_base_url(base_url)?).map_err(|_| "Invalid provider URL.".to_string())?;
    let path = url.path().trim_end_matches('/');
    let next_path = if path.ends_with("/v1") { format!("{path}/{resource}") } else { format!("{path}/v1/{resource}") };
    url.set_path(&next_path);
    Ok(url)
}

fn curl_config_value(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"").replace('\r', "\\r").replace('\n', "\\n"))
}

async fn provider_request(input: &ProviderInput, resource: &str, body: Option<serde_json::Value>) -> Result<(u16, Vec<u8>), String> {
    let base_url = validate_base_url(&input.base_url)?;
    let key = provider_api_key(input)?;
    let endpoint = provider_endpoint(&base_url, resource)?;
    // Keep the key on curl's stdin config pipe; never expose it in process arguments or logs.
    let config = format!(
        "header = {}\nheader = {}\n",
        curl_config_value(&format!("x-api-key: {key}")),
        curl_config_value("anthropic-version: 2023-06-01"),
    );
    let mut command = Command::new("curl");
    command
        .arg("-q")
        .args(["--silent", "--show-error", "--max-time", "15", "--write-out", "\n__AGENTDECK_HTTP_STATUS__%{http_code}"])
        .arg("--config").arg("-")
        .arg(endpoint.as_str());
    if let Some(body) = body {
        command.args(["--header", "content-type: application/json"]).arg("--data-binary").arg(body.to_string());
    }
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| format!("Could not run curl for provider check: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(config.as_bytes()).await.map_err(|_| "Could not pass provider settings to curl.".to_string())?;
    }
    let output = child.wait_with_output().await.map_err(|_| "Provider request did not finish.".to_string())?;
    if !output.status.success() {
        return Err("Provider request failed. Check your network, base URL, and curl installation.".into());
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let (response_body, status) = raw.rsplit_once("__AGENTDECK_HTTP_STATUS__")
        .ok_or_else(|| "Provider returned an incomplete response.".to_string())?;
    let status = status.trim().parse::<u16>().map_err(|_| "Could not read provider response status.".to_string())?;
    Ok((status, response_body.trim_end_matches('\n').as_bytes().to_vec()))
}

#[tauri::command]
async fn test_provider(input: ProviderInput) -> Result<String, String> {
    let model = input.default_model.trim();
    if model.is_empty() { return Err("Choose a default model before testing this provider.".into()); }
    let (status, _) = provider_request(&input, "messages", Some(serde_json::json!({
            "model": model,
            "max_tokens": 8,
            "messages": [{"role": "user", "content": "Reply with OK."}]
        }))).await?;
    if !(200..300).contains(&status) {
        return Err(format!("Provider returned HTTP {status}. Check its URL, API key, and model ID."));
    }
    Ok(format!("Connection successful. Model `{model}` responded."))
}

#[tauri::command]
async fn discover_provider_models(input: ProviderInput) -> Result<Vec<String>, String> {
    let (status, bytes) = provider_request(&input, "models", None).await?;
    if !(200..300).contains(&status) {
        return Err(format!("Model discovery returned HTTP {status}. You can still enter model IDs manually."));
    }
    let body: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| "Provider returned an invalid model list.".to_string())?;
    let mut models: Vec<String> = body.get("data").and_then(|value| value.as_array()).into_iter().flatten()
        .filter_map(|item| item.get("id").and_then(|value| value.as_str()).map(str::to_owned)).collect();
    models.sort();
    models.dedup();
    models.truncate(50);
    if models.is_empty() { return Err("Provider did not return any model IDs. Add them manually.".into()); }
    Ok(models)
}

#[derive(Serialize, Clone)]
struct ProjectEntry {
    name: String,
    path: String,
    is_dir: bool,
    children: Vec<ProjectEntry>,
}

#[derive(Serialize)]
struct FilePreview {
    path: String,
    content: String,
    truncated: bool,
}

#[tauri::command]
fn read_project_file(project_path: String, file_path: String) -> Result<FilePreview, String> {
    let project = std::fs::canonicalize(&project_path).map_err(|_| "Choose an existing project folder first.".to_string())?;
    let file = std::fs::canonicalize(&file_path).map_err(|_| "That project file could not be opened.".to_string())?;
    if !file.starts_with(&project) || !file.is_file() {
        return Err("Only files inside the selected project can be previewed.".into());
    }
    let handle = std::fs::File::open(&file).map_err(|_| "Could not read this file.".to_string())?;
    let mut bytes = Vec::new();
    std::io::Read::take(handle, 512 * 1024 + 1).read_to_end(&mut bytes).map_err(|_| "Could not read this file.".to_string())?;
    let truncated = bytes.len() > 512 * 1024;
    if truncated { bytes.truncate(512 * 1024); }
    if bytes.contains(&0) { return Err("This file appears to be binary and cannot be shown as text.".into()); }
    let content = String::from_utf8(bytes).map_err(|_| "This file is not UTF-8 text and cannot be previewed.".to_string())?;
    Ok(FilePreview { path: file.to_string_lossy().into_owned(), content, truncated })
}

fn collect_project_entries(path: &Path, depth: usize, remaining: &mut usize) -> Vec<ProjectEntry> {
    if depth == 0 || *remaining == 0 {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(path) else { return Vec::new() };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| {
        let is_file = !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        (is_file, entry.file_name().to_string_lossy().to_lowercase())
    });

    let mut result = Vec::new();
    for entry in entries {
        if *remaining == 0 { break; }
        let name = entry.file_name().to_string_lossy().into_owned();
        if matches!(name.as_str(), ".git" | "node_modules" | "target" | "dist" | "build" | ".next") {
            continue;
        }
        let Ok(kind) = entry.file_type() else { continue };
        *remaining -= 1;
        let entry_path = entry.path();
        result.push(ProjectEntry {
            name,
            path: entry_path.to_string_lossy().into_owned(),
            is_dir: kind.is_dir(),
            children: if kind.is_dir() { collect_project_entries(&entry_path, depth - 1, remaining) } else { Vec::new() },
        });
    }
    result
}

#[tauri::command]
fn list_project_files(project_path: String) -> Result<Vec<ProjectEntry>, String> {
    let project = std::fs::canonicalize(project_path)
        .map_err(|_| "Choose an existing project folder first.".to_string())?;
    if !project.is_dir() {
        return Err("The selected project path is not a folder.".into());
    }
    let mut remaining = 600;
    Ok(collect_project_entries(&project, 5, &mut remaining))
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let mut directories: Vec<PathBuf> = env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect())
        .unwrap_or_default();
    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        directories.extend([home.join(".local/bin"), home.join(".cargo/bin"), home.join(".npm-global/bin")]);
    }
    directories.extend([PathBuf::from("/usr/local/bin"), PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
    for directory in directories {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[tauri::command]
fn detect_claude() -> CliStatus {
    let binary = find_executable("claude");
    let version = binary.as_ref().and_then(|path| {
        std::process::Command::new(path)
            .arg("--version")
            .output()
            .ok()
            .filter(|result| result.status.success())
            .map(|result| String::from_utf8_lossy(&result.stdout).trim().to_owned())
    });
    let message = match binary.is_some() {
        true => "Claude Code CLI found. It will use the existing Claude Code configuration.".into(),
        false => "Claude Code CLI was not found. Install it and make sure `claude` is on PATH.".into(),
    };
    CliStatus {
        available: binary.is_some(),
        path: binary.map(|path| path.to_string_lossy().into_owned()),
        version,
        message,
    }
}

#[tauri::command]
fn check_auth_status() -> Result<AuthStatus, String> {
    let binary = find_executable("claude").ok_or_else(|| "Claude Code CLI was not found. Install it separately, then refresh this check.".to_string())?;
    let output = std::process::Command::new(binary).args(["auth", "status"]).output()
        .map_err(|_| "Could not run `claude auth status`. Check the CLI installation.".to_string())?;
    let body: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| "Claude Code returned an unreadable authentication status. Update Claude Code and retry.".to_string())?;
    let logged_in = body.get("loggedIn").and_then(|value| value.as_bool()).unwrap_or(false);
    let auth_method = body.get("authMethod").and_then(|value| value.as_str()).map(str::to_owned);
    Ok(AuthStatus {
        logged_in,
        auth_method,
        message: if logged_in { "Claude Code reports an active login.".into() } else { "Claude Code is installed, but no active login was reported.".into() },
    })
}

#[tauri::command]
fn open_auth_login() -> Result<(), String> {
    let claude = find_executable("claude").ok_or_else(|| "Claude Code CLI was not found. Install it separately first.".to_string())?;
    let candidates = ["x-terminal-emulator", "gnome-terminal", "konsole", "xfce4-terminal", "xterm"];
    let terminal = candidates.iter().find_map(|name| find_executable(name).map(|path| ((*name).to_string(), path)))
        .ok_or_else(|| "No supported terminal app was found. Open your terminal and run `claude auth login`.".to_string())?;
    let mut command = std::process::Command::new(terminal.1);
    match terminal.0.as_str() {
        "gnome-terminal" | "konsole" => { command.arg("--").arg(claude).args(["auth", "login"]); }
        "xfce4-terminal" => { command.arg("--command").arg(format!("{} auth login", claude.display())); }
        _ => { command.arg("-e").arg(claude).args(["auth", "login"]); }
    }
    command.spawn().map_err(|_| "Could not open a terminal for Claude Code login. Open a terminal and run `claude auth login`.".to_string())?;
    Ok(())
}

#[tauri::command]
fn get_git_status(project_path: String) -> Result<GitStatus, String> {
    let project = std::fs::canonicalize(&project_path).map_err(|_| "Choose an existing project folder first.".to_string())?;
    if !project.is_dir() { return Err("The selected project path is not a folder.".into()); }
    let Some(git) = find_executable("git") else {
        return Ok(GitStatus { available: false, branch: None, files: Vec::new(), message: "Git is not installed.".into() });
    };
    let output = std::process::Command::new(git).args(["-C", project.to_string_lossy().as_ref(), "status", "--short", "--branch", "--untracked-files=normal"]).output()
        .map_err(|_| "Could not read Git status.".to_string())?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Ok(GitStatus { available: false, branch: None, files: Vec::new(), message: if message.is_empty() { "This folder is not a Git repository.".into() } else { message } });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let branch_line = lines.next().unwrap_or_default();
    let branch = branch_line.strip_prefix("## ").map(|value| value.split("...").next().unwrap_or(value).to_string());
    let files = lines.filter(|line| line.len() >= 4).map(|line| GitFileStatus {
        index_status: line[0..1].to_string(),
        worktree_status: line[1..2].to_string(),
        path: line[3..].to_string(),
    }).collect();
    Ok(GitStatus { available: true, branch, files, message: "Read-only Git status.".into() })
}

async fn handle_permission_connection(
    stream: tokio::net::TcpStream,
    expected_token: String,
    app: AppHandle,
    pending: Arc<Mutex<std::collections::HashMap<String, (String, oneshot::Sender<PermissionDecision>)>>>,
    turn_id: String,
) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let Ok(Some(line)) = lines.next_line().await else { return };
    let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else { return };
    if request.get("token").and_then(|value| value.as_str()) != Some(expected_token.as_str()) { return; }
    let Some(tool_name) = request.get("tool_name").and_then(|value| value.as_str()).map(str::to_owned) else { return };
    let input = request.get("input").cloned().unwrap_or(serde_json::Value::Null);
    let permission_id = format!("{}-{}", std::process::id(), PERMISSION_SEQUENCE.fetch_add(1, Ordering::Relaxed));
    let (sender, receiver) = oneshot::channel();
    if let Ok(mut values) = pending.lock() { values.insert(permission_id.clone(), (turn_id.clone(), sender)); }
    let emitted = app.emit("permission-request", PermissionRequest { permission_id: permission_id.clone(), turn_id: turn_id.clone(), tool_name, input: input.clone() });
    let decision = if emitted.is_ok() { receiver.await.unwrap_or(PermissionDecision::Deny) } else { PermissionDecision::Deny };
    if let Ok(mut values) = pending.lock() { values.remove(&permission_id); }
    let response = match decision {
        PermissionDecision::Allow => serde_json::json!({ "allow": true, "input": input, "permission_id": permission_id, "turn_id": turn_id }),
        PermissionDecision::Deny => serde_json::json!({ "allow": false, "permission_id": permission_id, "turn_id": turn_id }),
    };
    let mut bytes = response.to_string().into_bytes();
    bytes.push(b'\n');
    let _ = writer.write_all(&bytes).await;
    let _ = writer.shutdown().await;
}

fn deny_pending_for_turn(pending: &Arc<Mutex<std::collections::HashMap<String, (String, oneshot::Sender<PermissionDecision>)>>>, turn_id: &str) {
    if let Ok(mut values) = pending.lock() {
        let ids: Vec<String> = values.iter().filter_map(|(id, (request_turn, _))| (request_turn == turn_id).then(|| id.clone())).collect();
        for id in ids {
            if let Some((_, sender)) = values.remove(&id) { let _ = sender.send(PermissionDecision::Deny); }
        }
    }
}

fn permission_server_response(id: &serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Entry point used when Claude Code launches AgentDeck as its temporary permission MCP server.
pub fn run_permission_server(address: String, token: String) {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        let id = request.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let method = request.get("method").and_then(|value| value.as_str()).unwrap_or_default();
        let response = match method {
            "initialize" => Some(permission_server_response(&id, serde_json::json!({
                "protocolVersion": "2024-11-05", "capabilities": { "tools": {} },
                "serverInfo": { "name": "agentdeck-permissions", "version": "0.2.0" }
            }))),
            "notifications/initialized" | "notifications/cancelled" => None,
            "ping" => Some(permission_server_response(&id, serde_json::json!({}))),
            "tools/list" => Some(permission_server_response(&id, serde_json::json!({ "tools": [{
                "name": "request_approval",
                "description": "Ask the AgentDeck user whether Claude Code may perform a specific action.",
                "inputSchema": { "type": "object", "properties": {
                    "tool_name": { "type": "string" },
                    "input": { "type": "object", "additionalProperties": true }
                }, "required": ["tool_name", "input"] }
            }] }))),
            "tools/call" => {
                let params = request.get("params").cloned().unwrap_or_default();
                if params.get("name").and_then(|value| value.as_str()) != Some("request_approval") {
                    Some(serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Unknown tool." } }))
                } else {
                    let arguments = params.get("arguments").cloned().unwrap_or_default();
                    let payload = serde_json::json!({
                        "token": token.clone(),
                        "tool_name": arguments.get("tool_name").and_then(|value| value.as_str()).unwrap_or("Unknown action"),
                        "input": arguments.get("input").cloned().unwrap_or(serde_json::Value::Null)
                    });
                    let decision = (|| -> Result<serde_json::Value, String> {
                        use std::io::{BufRead, BufReader, Write};
                        let mut socket = std::net::TcpStream::connect(&address).map_err(|_| "AgentDeck approval window is unavailable.".to_string())?;
                        socket.set_read_timeout(Some(std::time::Duration::from_secs(300))).ok();
                        writeln!(socket, "{payload}").map_err(|_| "Could not send the approval request.".to_string())?;
                        let mut answer = String::new();
                        BufReader::new(socket).read_line(&mut answer).map_err(|_| "AgentDeck did not return an approval decision.".to_string())?;
                        serde_json::from_str(&answer).map_err(|_| "AgentDeck returned an invalid approval decision.".to_string())
                    })().unwrap_or_else(|error| serde_json::json!({ "allow": false, "error": error }));
                    let permission_result = if decision.get("allow").and_then(|value| value.as_bool()) == Some(true) {
                        serde_json::json!({ "behavior": "allow", "updatedInput": decision.get("input").cloned().unwrap_or(serde_json::Value::Null), "updatedPermissions": [] })
                    } else {
                        serde_json::json!({ "behavior": "deny", "message": decision.get("error").and_then(|value| value.as_str()).unwrap_or("The user denied this action in AgentDeck.") })
                    };
                    let result_text = permission_result.to_string();
                    Some(permission_server_response(&id, serde_json::json!({ "content": [{ "type": "text", "text": result_text }] })))
                }
            }
            _ => Some(serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Method not found." } })),
        };
        if let Some(response) = response {
            if writeln!(output, "{response}").is_err() { break; }
            if output.flush().is_err() { break; }
        }
    }
}

#[tauri::command]
async fn start_turn(
    app: AppHandle,
    state: State<'_, AppState>,
    turn_id: String,
    prompt: String,
    project_path: String,
    session_id: Option<String>,
    provider_id: Option<String>,
    model: Option<String>,
) -> Result<(), String> {
    if prompt.trim().is_empty() {
        return Err("Write a message before sending it.".into());
    }
    if turn_id.is_empty() || turn_id.len() > 80 {
        return Err("Invalid turn identifier.".into());
    }
    let project = std::fs::canonicalize(&project_path)
        .map_err(|_| "Choose an existing project folder first.".to_string())?;
    if !project.is_dir() {
        return Err("The selected project path is not a folder.".into());
    }
    let binary = find_executable("claude").ok_or_else(|| "Claude Code CLI was not found. Install it or make it available on PATH.".to_string())?;

    let selected_provider = if let Some(id) = provider_id.filter(|id| !id.trim().is_empty()) {
        Some(read_providers(&app)?.into_iter().find(|provider| provider.id == id)
            .ok_or_else(|| "That provider is no longer configured. Open provider settings and try again.".to_string())?)
    } else { None };
    let selected_model = model.unwrap_or_default().trim().to_string();

    let permission_listener = TcpListener::bind(("127.0.0.1", 0)).await
        .map_err(|_| "Could not start AgentDeck's local permission bridge.".to_string())?;
    let permission_address = permission_listener.local_addr().map_err(|_| "Could not locate the local permission bridge.".to_string())?.to_string();
    let permission_token = format!("{}-{}-{}", std::process::id(), PERMISSION_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    let permission_config = serde_json::json!({ "mcpServers": { "agentdeck_gate": {
        "type": "stdio",
        "command": std::env::current_exe().map_err(|_| "Could not locate AgentDeck for the approval bridge.".to_string())?,
        "args": ["--agentdeck-permission-server", &permission_address, &permission_token]
    }}}).to_string();

    let mut command = Command::new(binary);
    command
        .current_dir(project)
        .arg("--print")
        .args(["--output-format", "stream-json", "--verbose", "--include-partial-messages"])
        .args(["--permission-mode", "default"])
        .arg("--mcp-config").arg(permission_config)
        .args(["--permission-prompt-tool", "mcp__agentdeck_gate__request_approval"]);
    if let Some(provider) = selected_provider {
        let model_name = if selected_model.is_empty() { provider.default_model.clone() } else { selected_model.clone() };
        if !provider.models.iter().any(|available| available == &model_name) {
            return Err("Choose a model listed for the selected provider.".into());
        }
        let api_key = secret_entry(&provider.id)?.get_password()
            .map_err(|error| format!("Could not read this provider's API key from the desktop keyring: {error}"))?;
        command
            .env("ANTHROPIC_BASE_URL", &provider.base_url)
            .env("ANTHROPIC_API_KEY", api_key)
            .env("ANTHROPIC_MODEL", model_name)
            .env_remove("ANTHROPIC_AUTH_TOKEN");
    } else if !selected_model.is_empty() {
        command.env("ANTHROPIC_MODEL", selected_model);
    }
    if let Some(id) = session_id.filter(|id| !id.trim().is_empty()) {
        command.args(["--resume", id.as_str()]);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let active_process = Arc::clone(&state.active_process);
    let mut child = command.spawn().map_err(|error| format!("Could not start Claude Code: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).await.map_err(|_| "Could not send the message to Claude Code.".to_string())?;
    }
    let pid = child.id().ok_or_else(|| "Could not identify the Claude Code process.".to_string())? as i32;
    {
        let mut active = active_process.lock().map_err(|_| "Process state is unavailable.".to_string())?;
        if active.is_some() {
            let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGINT);
            return Err("A Claude Code turn is already running.".into());
        }
        *active = Some((turn_id.clone(), pid));
    }

    let app_handle = app.clone();
    let task_turn_id = turn_id.clone();
    let pending_to_clear = Arc::clone(&state.pending_permissions);
    let pending_permissions = Arc::clone(&state.pending_permissions);
    let bridge_app = app.clone();
    let bridge_turn_id = turn_id.clone();
    let bridge_token = permission_token;
    let bridge_task = tauri::async_runtime::spawn(async move {
        while let Ok((stream, _)) = permission_listener.accept().await {
            let token = bridge_token.clone();
            let app = bridge_app.clone();
            let pending = Arc::clone(&pending_permissions);
            let turn_id = bridge_turn_id.clone();
            tauri::async_runtime::spawn(handle_permission_connection(stream, token, app, pending, turn_id));
        }
    });
    let stdout = child.stdout.take().ok_or_else(|| "Claude Code output was unavailable.".to_string())?;
    let stderr = child.stderr.take().ok_or_else(|| "Claude Code diagnostics were unavailable.".to_string())?;
    tauri::async_runtime::spawn(async move {
        // Drain stderr separately; never mix diagnostics into the newline-delimited JSON stream.
        let stderr_task = tauri::async_runtime::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while lines.next_line().await.ok().flatten().is_some() {}
        });
        let mut lines = BufReader::new(stdout).lines();
        let mut session: Option<String> = None;
        let mut streamed_text = false;
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
            if session.is_none() {
                session = event.get("session_id").and_then(|value| value.as_str()).map(str::to_owned);
            }
            if event.get("type").and_then(|value| value.as_str()) == Some("stream_event") {
                let delta = event.get("event").and_then(|value| value.get("delta"));
                if delta.and_then(|value| value.get("type")).and_then(|value| value.as_str()) == Some("text_delta") {
                    if let Some(text) = delta.and_then(|value| value.get("text")).and_then(|value| value.as_str()) {
                        streamed_text = true;
                        let _ = app_handle.emit("claude-turn", TurnEvent {
                            turn_id: task_turn_id.clone(), kind: "text".into(), text: Some(text.into()), session_id: None,
                        });
                    }
                }
            }
            if event.get("type").and_then(|value| value.as_str()) == Some("result") {
                if session.is_none() {
                    session = event.get("session_id").and_then(|value| value.as_str()).map(str::to_owned);
                }
                if !streamed_text {
                    if let Some(text) = event.get("result").and_then(|value| value.as_str()) {
                        let _ = app_handle.emit("claude-turn", TurnEvent {
                            turn_id: task_turn_id.clone(), kind: "text".into(), text: Some(text.into()), session_id: None,
                        });
                    }
                }
                if event.get("permission_denials").and_then(|value| value.as_array()).is_some_and(|items| !items.is_empty()) {
                    let _ = app_handle.emit("claude-turn", TurnEvent {
                        turn_id: task_turn_id.clone(), kind: "permission-denied".into(),
                        text: Some("An action requiring approval was denied safely. No action was auto-approved.".into()), session_id: None,
                    });
                }
            }
        }
        let exit_code = child.wait().await.ok().and_then(|status| status.code());
        bridge_task.abort();
        deny_pending_for_turn(&pending_to_clear, &task_turn_id);
        let _ = stderr_task.await;
        let final_kind = if exit_code == Some(0) { "complete" } else { "error" };
        let final_text = if exit_code == Some(0) { None } else { Some("Claude Code stopped with an error. Check its login, installation, or project settings.".into()) };
        let _ = app_handle.emit("claude-turn", TurnEvent {
            turn_id: task_turn_id.clone(), kind: "session".into(), text: None, session_id: session,
        });
        let _ = app_handle.emit("claude-turn", TurnEvent {
            turn_id: task_turn_id.clone(), kind: final_kind.into(), text: final_text, session_id: None,
        });
        if let Ok(mut active) = active_process.lock() {
            if active.as_ref().is_some_and(|(id, _)| id == &task_turn_id) { *active = None; }
        }
    });
    Ok(())
}

#[tauri::command]
fn stop_turn(state: State<'_, AppState>, turn_id: String) -> Result<(), String> {
    let active = state.active_process.lock().map_err(|_| "Process state is unavailable.".to_string())?;
    let Some((active_id, pid)) = active.as_ref() else { return Ok(()) };
    if active_id != &turn_id { return Err("That Claude Code turn is no longer active.".into()); }
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(*pid), nix::sys::signal::Signal::SIGINT)
        .map_err(|_| "Could not interrupt the Claude Code process.".to_string())
}

#[tauri::command]
fn answer_permission(state: State<'_, AppState>, permission_id: String, allow: bool) -> Result<(), String> {
    let (_, sender) = state.pending_permissions.lock().map_err(|_| "Permission state is unavailable.".to_string())?
        .remove(&permission_id).ok_or_else(|| "That permission request is no longer waiting.".to_string())?;
    sender.send(if allow { PermissionDecision::Allow } else { PermissionDecision::Deny })
        .map_err(|_| "Claude Code is no longer waiting for this decision.".to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![detect_claude, check_auth_status, open_auth_login, get_git_status, answer_permission, get_providers, save_provider, delete_provider, test_provider, discover_provider_models, list_project_files, read_project_file, start_turn, stop_turn])
        .run(tauri::generate_context!())
        .expect("error while running AgentDeck");
}
