use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Manager, State};
use thiserror::Error;

const STATE_CHANGED_EVENT: &str = "preview_bridge://state_changed";
const DEFAULT_PORT: u16 = 9999;
const CONNECT_TIMEOUT_MS: u64 = 250;
const SPAWN_SETTLE_MS: u64 = 450;
const POLL_RETRY_MS: u64 = 350;
const POLL_TIMEOUT_SECS: u64 = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppConfig {
    nova2_project_dir: String,
    godot_executable_path: String,
    preview_bridge_port: u16,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            nova2_project_dir: String::new(),
            godot_executable_path: String::new(),
            preview_bridge_port: DEFAULT_PORT,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreviewBridgeError {
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    column: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreviewBridgeState {
    current_node_record_id: Option<i64>,
    current_dialogue_index: Option<i64>,
    start_node_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateChangedEvent {
    method: String,
    ok: bool,
    state: PreviewBridgeState,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScenarioFile {
    name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum BridgeResponse {
    Success(BridgeSuccessResponse),
    Error(BridgeErrorResponse),
    Event(StateChangedEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeSuccessResponse {
    id: Option<u64>,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<PreviewBridgeState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeErrorResponse {
    id: Option<u64>,
    ok: bool,
    error: PreviewBridgeError,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum CommandResult {
    Success(CommandSuccess),
    Error(CommandFailure),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandSuccess {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<PreviewBridgeState>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandFailure {
    ok: bool,
    error: PreviewBridgeError,
}

impl CommandResult {
    fn ok(state: Option<PreviewBridgeState>) -> Self {
        Self::Success(CommandSuccess { ok: true, state })
    }

    fn error(error: PreviewBridgeError) -> Self {
        Self::Error(CommandFailure { ok: false, error })
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LoadProjectResult {
    ok: bool,
    state: PreviewBridgeState,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeStatus {
    is_connected: bool,
    is_godot_running: bool,
    config: AppConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<PreviewBridgeState>,
}

#[derive(Debug, Clone)]
struct RuntimeSnapshot {
    config: AppConfig,
    latest_state: Option<PreviewBridgeState>,
    is_connected: bool,
    is_godot_running: bool,
}

#[derive(Debug)]
struct RuntimeInner {
    config: AppConfig,
    latest_state: Option<PreviewBridgeState>,
    godot_child: Option<Child>,
    command_counter: u64,
    state_listener_started: bool,
}

impl Default for RuntimeInner {
    fn default() -> Self {
        Self {
            config: AppConfig::default(),
            latest_state: None,
            godot_child: None,
            command_counter: 0,
            state_listener_started: false,
        }
    }
}

#[derive(Clone)]
struct AppRuntime {
    app: AppHandle,
    inner: Arc<Mutex<RuntimeInner>>,
}

#[derive(Debug, Error)]
enum BackendError {
    #[error("{0}")]
    Message(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

impl BackendError {
    fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    fn to_bridge_error(&self) -> PreviewBridgeError {
        PreviewBridgeError {
            message: self.to_string(),
            line: None,
            column: None,
        }
    }
}

impl AppRuntime {
    fn new(app: AppHandle) -> Self {
        Self {
            app,
            inner: Arc::new(Mutex::new(RuntimeInner::default())),
        }
    }

    fn initialize(&self) {
        if let Ok(config) = load_config_from_disk(&self.app) {
            self.inner.lock().config = config;
        }
    }

    fn config_path(&self) -> Result<PathBuf, BackendError> {
        let dir = self
            .app
            .path()
            .app_config_dir()
            .map_err(|error| BackendError::message(format!("无法获取配置目录: {error}")))?;
        Ok(dir.join("settings.json"))
    }

    fn get_config(&self) -> AppConfig {
        self.inner.lock().config.clone()
    }

    fn save_config(&self, config: AppConfig) -> Result<AppConfig, BackendError> {
        validate_config(&config)?;
        let path = self.config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, serde_json::to_vec_pretty(&config)?)?;
        self.inner.lock().config = config.clone();
        Ok(config)
    }

    fn snapshot(&self) -> RuntimeSnapshot {
        let mut inner = self.inner.lock();
        let is_godot_running = inner
            .godot_child
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten())
            .is_none()
            && inner.godot_child.is_some();
        RuntimeSnapshot {
            config: inner.config.clone(),
            latest_state: inner.latest_state.clone(),
            is_connected: inner.latest_state.is_some(),
            is_godot_running,
        }
    }

    fn next_command_id(&self) -> u64 {
        let mut inner = self.inner.lock();
        inner.command_counter += 1;
        inner.command_counter
    }

    fn ensure_godot_started(&self) -> Result<(), BackendError> {
        {
            let mut inner = self.inner.lock();
            if let Some(child) = inner.godot_child.as_mut() {
                if child.try_wait()?.is_none() {
                    return Ok(());
                }
                inner.godot_child = None;
            }
        }

        let config = self.get_config();
        validate_config(&config)?;
        let mut command = Command::new(&config.godot_executable_path);
        command.arg("--path").arg(&config.nova2_project_dir);
        let child = command.spawn()?;
        self.inner.lock().godot_child = Some(child);
        thread::sleep(Duration::from_millis(SPAWN_SETTLE_MS));
        Ok(())
    }

    fn ensure_bridge_ready(&self) -> Result<PreviewBridgeState, BackendError> {
        self.ensure_godot_started()?;
        let timeout = Duration::from_secs(POLL_TIMEOUT_SECS);
        let deadline = Instant::now() + timeout;
        let port = self.get_config().preview_bridge_port;
        let mut last_error: Option<BackendError> = None;

        while Instant::now() < deadline {
            match self.send_request(port, serde_json::json!({ "id": self.next_command_id(), "method": "get_state" })) {
                Ok(response) => match response {
                    BridgeResponse::Success(success) if success.ok => {
                        let state = success.state.ok_or_else(|| BackendError::message("get_state 响应缺少 state 字段"))?;
                        self.publish_state(state.clone())?;
                        self.spawn_state_listener(port);
                        return Ok(state);
                    }
                    BridgeResponse::Error(error) => {
                        return Err(BackendError::message(error.error.message));
                    }
                    _ => {
                        last_error = Some(BackendError::message("收到无法识别的 PreviewBridge 响应"));
                    }
                },
                Err(error) => {
                    last_error = Some(error);
                    thread::sleep(Duration::from_millis(POLL_RETRY_MS));
                }
            }
        }

        Err(last_error.unwrap_or_else(|| BackendError::message("连接 PreviewBridge 超时，Godot 可能尚未启动完成")))
    }

    /// Commands each open their own short-lived connection (see send_request), so PreviewBridge's
    /// unsolicited state_changed pushes (from the player using debug keys R/N/P directly in the
    /// Godot window) would otherwise never reach this process. Keep one long-lived connection open
    /// purely to listen for those pushes - started once per successful bridge handshake.
    fn spawn_state_listener(&self, port: u16) {
        {
            let mut inner = self.inner.lock();
            if inner.state_listener_started {
                return;
            }
            inner.state_listener_started = true;
        }

        let runtime = self.clone();
        thread::spawn(move || loop {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(stream) => {
                    let mut reader = BufReader::new(stream);
                    loop {
                        let mut line = String::new();
                        match reader.read_line(&mut line) {
                            Ok(0) => break,
                            Ok(_) => {
                                let trimmed = line.trim();
                                if trimmed.is_empty() {
                                    continue;
                                }
                                if let Ok(BridgeResponse::Event(event)) = serde_json::from_str::<BridgeResponse>(trimmed) {
                                    let _ = runtime.publish_state(event.state);
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
                Err(_) => {}
            }
            thread::sleep(Duration::from_secs(1));
        });
    }

    fn scenarios_dir(&self) -> PathBuf {
        Path::new(&self.get_config().nova2_project_dir)
            .join("resources")
            .join("scenarios")
    }

    fn resolve_scenario_path(&self, name: &str) -> Result<PathBuf, BackendError> {
        if name.is_empty() || name.contains("..") || name.contains('/') || name.contains('\\') {
            return Err(BackendError::message(format!("非法的脚本文件名: {name}")));
        }
        Ok(self.scenarios_dir().join(name))
    }

    fn list_scenario_files(&self) -> Result<Vec<ScenarioFile>, BackendError> {
        let dir = self.scenarios_dir();
        let entries = fs::read_dir(&dir).map_err(|error| BackendError::message(format!("无法读取 {}: {error}", dir.display())))?;
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("txt"))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        Ok(names.into_iter().map(|name| ScenarioFile { name }).collect())
    }

    fn read_scenario_file(&self, name: &str) -> Result<String, BackendError> {
        let path = self.resolve_scenario_path(name)?;
        fs::read_to_string(&path).map_err(|error| BackendError::message(format!("读取 {} 失败: {error}", path.display())))
    }

    fn write_scenario_file(&self, name: &str, content: &str) -> Result<(), BackendError> {
        let path = self.resolve_scenario_path(name)?;
        fs::write(&path, content).map_err(|error| BackendError::message(format!("写入 {} 失败: {error}", path.display())))
    }

    fn send_reload(&self) -> Result<CommandResult, BackendError> {
        self.ensure_bridge_ready()?;
        let port = self.get_config().preview_bridge_port;
        let response = self.send_request(port, serde_json::json!({ "id": self.next_command_id(), "method": "reload" }))?;
        self.map_command_response(response)
    }

    fn send_seek(&self, node_record_id: i64, dialogue_index: i64) -> Result<CommandResult, BackendError> {
        self.ensure_bridge_ready()?;
        let port = self.get_config().preview_bridge_port;
        let response = self.send_request(
            port,
            serde_json::json!({
                "id": self.next_command_id(),
                "method": "seek",
                "params": {
                    "nodeRecordId": node_record_id,
                    "dialogueIndex": dialogue_index
                }
            }),
        )?;
        self.map_command_response(response)
    }

    fn send_request(&self, port: u16, payload: Value) -> Result<BridgeResponse, BackendError> {
        let address = format!("127.0.0.1:{port}");
        let mut stream = TcpStream::connect(&address)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        stream.set_nodelay(true)?;

        let mut serialized = serde_json::to_vec(&payload)?;
        serialized.push(b'\n');
        stream.write_all(&serialized)?;
        stream.flush()?;

        let mut reader = BufReader::new(stream);
        let started = Instant::now();
        loop {
            let mut line = String::new();
            let bytes_read = reader.read_line(&mut line)?;
            if bytes_read == 0 {
                return Err(BackendError::message("PreviewBridge 连接已关闭，未收到响应"));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                if started.elapsed() > Duration::from_secs(5) {
                    return Err(BackendError::message("等待 PreviewBridge 响应超时"));
                }
                continue;
            }
            let response: BridgeResponse = serde_json::from_str(trimmed)?;
            if let BridgeResponse::Event(event) = &response {
                self.publish_state(event.state.clone())?;
                continue;
            }
            return Ok(response);
        }
    }

    fn map_command_response(&self, response: BridgeResponse) -> Result<CommandResult, BackendError> {
        match response {
            BridgeResponse::Success(success) if success.ok => {
                if let Some(state) = success.state.clone() {
                    self.publish_state(state.clone())?;
                    Ok(CommandResult::ok(Some(state)))
                } else {
                    Ok(CommandResult::ok(self.inner.lock().latest_state.clone()))
                }
            }
            BridgeResponse::Error(error) => Ok(CommandResult::error(error.error)),
            _ => Err(BackendError::message("收到无法识别的 PreviewBridge 响应")),
        }
    }

    fn publish_state(&self, state: PreviewBridgeState) -> Result<(), BackendError> {
        self.inner.lock().latest_state = Some(state.clone());
        self.app
            .emit(
                STATE_CHANGED_EVENT,
                StateChangedEvent {
                    method: "state_changed".to_string(),
                    ok: true,
                    state,
                },
            )
            .map_err(|error| BackendError::message(format!("向前端派发状态事件失败: {error}")))?;
        Ok(())
    }
}

fn validate_config(config: &AppConfig) -> Result<(), BackendError> {
    validate_existing_path(&config.nova2_project_dir, "Nova2 工程目录")?;
    validate_existing_path(&config.godot_executable_path, "Godot 可执行文件路径")?;
    if config.preview_bridge_port == 0 {
        return Err(BackendError::message("PreviewBridge 端口必须大于 0"));
    }
    Ok(())
}

fn validate_existing_path(value: &str, label: &str) -> Result<(), BackendError> {
    if value.trim().is_empty() {
        return Err(BackendError::message(format!("{label}不能为空")));
    }
    let path = Path::new(value);
    if !path.exists() {
        return Err(BackendError::message(format!("{label}不存在: {value}")));
    }
    Ok(())
}

fn load_config_from_disk(app: &AppHandle) -> Result<AppConfig, BackendError> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|error| BackendError::message(format!("无法获取配置目录: {error}")))?;
    let path = dir.join("settings.json");
    if !path.exists() {
        return Ok(AppConfig::default());
    }
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[tauri::command]
fn get_app_config(runtime: State<'_, AppRuntime>) -> Result<AppConfig, PreviewBridgeError> {
    Ok(runtime.get_config())
}

#[tauri::command]
fn save_app_config(runtime: State<'_, AppRuntime>, config: AppConfig) -> Result<AppConfig, PreviewBridgeError> {
    runtime.save_config(config).map_err(|error| error.to_bridge_error())
}

#[tauri::command]
fn get_runtime_status(runtime: State<'_, AppRuntime>) -> Result<RuntimeStatus, PreviewBridgeError> {
    let snapshot = runtime.snapshot();
    Ok(RuntimeStatus {
        is_connected: snapshot.is_connected,
        is_godot_running: snapshot.is_godot_running,
        config: snapshot.config,
        state: snapshot.latest_state,
    })
}

#[tauri::command]
fn load_project(runtime: State<'_, AppRuntime>) -> Result<LoadProjectResult, PreviewBridgeError> {
    runtime
        .ensure_bridge_ready()
        .map(|state| LoadProjectResult { ok: true, state })
        .map_err(|error| error.to_bridge_error())
}

#[tauri::command]
fn reload_preview(runtime: State<'_, AppRuntime>) -> Result<CommandResult, PreviewBridgeError> {
    runtime.send_reload().map_err(|error| error.to_bridge_error())
}

#[tauri::command]
fn seek(runtime: State<'_, AppRuntime>, node_record_id: i64, dialogue_index: i64) -> Result<CommandResult, PreviewBridgeError> {
    runtime
        .send_seek(node_record_id, dialogue_index)
        .map_err(|error| error.to_bridge_error())
}

#[tauri::command]
fn list_scenario_files(runtime: State<'_, AppRuntime>) -> Result<Vec<ScenarioFile>, PreviewBridgeError> {
    runtime.list_scenario_files().map_err(|error| error.to_bridge_error())
}

#[tauri::command]
fn read_scenario_file(runtime: State<'_, AppRuntime>, name: String) -> Result<String, PreviewBridgeError> {
    runtime.read_scenario_file(&name).map_err(|error| error.to_bridge_error())
}

#[tauri::command]
fn write_scenario_file(runtime: State<'_, AppRuntime>, name: String, content: String) -> Result<(), PreviewBridgeError> {
    runtime.write_scenario_file(&name, &content).map_err(|error| error.to_bridge_error())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let runtime = AppRuntime::new(app.handle().clone());
            runtime.initialize();
            app.manage(runtime);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_app_config,
            save_app_config,
            get_runtime_status,
            load_project,
            reload_preview,
            seek,
            list_scenario_files,
            read_scenario_file,
            write_scenario_file
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
