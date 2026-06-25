mod character_template;
mod generation;
mod llm;
mod version_history;

use generation::{
    build_atmosphere_plan_prompt, build_atmosphere_plan_retry_prompt, build_generation_prompt, build_retry_prompt,
    build_summary_prompt, find_unknown_asset_paths, find_unknown_audio_tracks, find_unknown_shaders,
    find_unknown_sound_tracks, format_asset_path_issues, format_audio_track_issues, format_shader_issues,
    format_sound_track_issues, generate_with_prompt, list_known_asset_script_paths, list_known_audio_layout,
    list_known_character_bind_names, list_known_shader_names, parse_atmosphere_plan, parse_generated_output,
    register_new_characters, AtmospherePlan,
};
use version_history::VersionInfo;
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
const SUMMARY_READY_EVENT: &str = "preview_bridge://generation_summary_ready";
const DEFAULT_PORT: u16 = 9999;
const CONNECT_TIMEOUT_MS: u64 = 250;
const SPAWN_SETTLE_MS: u64 = 450;
const POLL_RETRY_MS: u64 = 350;
const POLL_TIMEOUT_SECS: u64 = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct AppConfig {
    nova2_project_dir: String,
    godot_executable_path: String,
    preview_bridge_port: u16,
    zhipu_api_key: String,
    deepseek_api_key: String,
    vfx_notes_path: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            nova2_project_dir: String::new(),
            godot_executable_path: String::new(),
            preview_bridge_port: DEFAULT_PORT,
            zhipu_api_key: String::new(),
            deepseek_api_key: String::new(),
            vfx_notes_path: String::new(),
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
struct SummaryReadyEvent {
    target_file: String,
    summary: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScenarioFile {
    name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AssetFile {
    path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssetDescriptionRecord {
    description: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectSession {
    has_project: bool,
    project_dir: String,
}

// Variant order matters here: serde tries untagged variants in declaration order and commits to
// the first structural match. BridgeSuccessResponse's fields are all optional except `ok`, and
// serde silently ignores JSON fields a struct doesn't declare - so a real error payload
// ({id, ok:false, error:{...}}) or an unsolicited state_changed push ({method, ok:true, state})
// would both happily deserialize as a "successful" BridgeSuccessResponse if it were tried first,
// since their extra `error`/`method` fields are just dropped. Error and Event are listed first
// because they each have a field Success doesn't (`error`, `method`), so only genuine success
// payloads (which have neither) fall through to Success.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum BridgeResponse {
    Error(BridgeErrorResponse),
    Event(StateChangedEvent),
    Success(BridgeSuccessResponse),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeSuccessResponse {
    id: Option<u64>,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<PreviewBridgeState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    node_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dialogue_index: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    node_record_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reached: Option<bool>,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateRequest {
    user_prompt: String,
    target_file: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerateResult {
    final_script: String,
    attempts: u32,
    applied: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error: Option<PreviewBridgeError>,
    summary: String,
}

#[derive(Debug, Clone)]
struct LocateResult {
    node_name: String,
    dialogue_index: i64,
    node_record_id: Option<i64>,
    reached: bool,
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
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
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

    fn get_project_session(&self) -> ProjectSession {
        let config = self.get_config();
        ProjectSession {
            has_project: !config.nova2_project_dir.trim().is_empty(),
            project_dir: config.nova2_project_dir,
        }
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

    fn leave_project(&self) -> Result<ProjectSession, BackendError> {
        let mut inner = self.inner.lock();
        inner.latest_state = None;
        inner.state_listener_started = false;
        if let Some(child) = inner.godot_child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        inner.godot_child = None;
        Ok(ProjectSession {
            has_project: false,
            project_dir: String::new(),
        })
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

    fn project_path(&self, relative: &str) -> PathBuf {
        Path::new(&self.get_config().nova2_project_dir).join(relative)
    }

    fn vvn_data_dir(&self) -> PathBuf {
        self.project_path("vvn_data")
    }

    fn asset_descriptions_path(&self) -> PathBuf {
        self.vvn_data_dir().join("asset_descriptions.json")
    }

    fn resolve_asset_path(&self, relative: &str) -> PathBuf {
        self.project_path(relative)
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

    fn list_asset_files(&self) -> Result<Vec<AssetFile>, BackendError> {
        let mut files = Vec::new();
        self.collect_asset_files(&self.project_path("resources/standings"), "resources/standings", &mut files)?;
        self.collect_asset_files(&self.project_path("resources/backgrounds"), "resources/backgrounds", &mut files)?;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    fn collect_asset_files(&self, dir: &Path, relative_prefix: &str, output: &mut Vec<AssetFile>) -> Result<(), BackendError> {
        if !dir.exists() {
            return Ok(());
        }
        let entries = fs::read_dir(dir).map_err(|error| BackendError::message(format!("无法读取 {}: {error}", dir.display())))?;
        for entry in entries {
            let entry = entry.map_err(BackendError::from)?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(BackendError::from)?;
            if file_type.is_dir() {
                let child_prefix = format!("{relative_prefix}/{}", entry.file_name().to_string_lossy());
                self.collect_asset_files(&path, &child_prefix, output)?;
                continue;
            }
            if !is_image_path(&path) {
                continue;
            }
            let file_name = entry.file_name().to_string_lossy().replace('\\', "/");
            output.push(AssetFile {
                path: format!("{relative_prefix}/{file_name}"),
            });
        }
        Ok(())
    }

    fn load_asset_description_index(&self) -> Result<std::collections::BTreeMap<String, AssetDescriptionRecord>, BackendError> {
        let path = self.asset_descriptions_path();
        if !path.exists() {
            return Ok(std::collections::BTreeMap::new());
        }
        let content = fs::read_to_string(&path)
            .map_err(|error| BackendError::message(format!("读取 {} 失败: {error}", path.display())))?;
        if content.trim().is_empty() {
            return Ok(std::collections::BTreeMap::new());
        }
        serde_json::from_str(&content)
            .map_err(|error| BackendError::message(format!("解析 {} 失败: {error}", path.display())))
    }

    fn save_asset_description_index(
        &self,
        index: &std::collections::BTreeMap<String, AssetDescriptionRecord>,
    ) -> Result<(), BackendError> {
        let dir = self.vvn_data_dir();
        fs::create_dir_all(&dir)?;
        let path = self.asset_descriptions_path();
        fs::write(&path, serde_json::to_vec_pretty(index)?)
            .map_err(|error| BackendError::message(format!("写入 {} 失败: {error}", path.display())))
    }

    fn sync_asset_description_index(&self) -> Result<(), BackendError> {
        let asset_files = self.list_asset_files()?;
        let mut index = self.load_asset_description_index()?;
        let current_paths: std::collections::BTreeSet<String> = asset_files.iter().map(|asset| asset.path.clone()).collect();
        index.retain(|path, _| current_paths.contains(path));
        for asset in asset_files {
            index.entry(asset.path).or_insert_with(|| AssetDescriptionRecord {
                description: String::new(),
            });
        }
        self.save_asset_description_index(&index)
    }

    fn upsert_asset_description(&self, path: &str, description: String) -> Result<(), BackendError> {
        let mut index = self.load_asset_description_index()?;
        index.insert(
            path.to_string(),
            AssetDescriptionRecord {
                description,
            },
        );
        self.save_asset_description_index(&index)
    }

    fn read_scenario_file(&self, name: &str) -> Result<String, BackendError> {
        let path = self.resolve_scenario_path(name)?;
        fs::read_to_string(&path).map_err(|error| BackendError::message(format!("读取 {} 失败: {error}", path.display())))
    }

    fn write_scenario_file(&self, name: &str, content: &str) -> Result<(), BackendError> {
        let path = self.resolve_scenario_path(name)?;
        let project_dir = Path::new(&self.get_config().nova2_project_dir).to_path_buf();
        version_history::snapshot_before_write(&path, &project_dir, name, "")?;
        fs::write(&path, content).map_err(|error| BackendError::message(format!("写入 {} 失败: {error}", path.display())))
    }

    /// Writes a generation-loop draft to disk without creating a history snapshot. The retry loop
    /// calls this once per attempt - snapshotting every failed intermediate draft would flood
    /// version history with noise; the loop snapshots the true pre-generation content exactly once,
    /// at the end, if and when generation succeeds (see `generate_script_with_retry_inner`).
    fn write_scenario_file_draft(&self, name: &str, content: &str) -> Result<(), BackendError> {
        let path = self.resolve_scenario_path(name)?;
        fs::write(&path, content).map_err(|error| BackendError::message(format!("写入 {} 失败: {error}", path.display())))
    }

    fn list_script_versions(&self, name: &str) -> Result<Vec<VersionInfo>, BackendError> {
        let project_dir = Path::new(&self.get_config().nova2_project_dir).to_path_buf();
        version_history::list_versions(&project_dir, name)
    }

    fn restore_script_version(&self, name: &str, version_id: &str) -> Result<String, BackendError> {
        let project_dir = Path::new(&self.get_config().nova2_project_dir).to_path_buf();
        let content = version_history::read_version(&project_dir, name, version_id)?;
        self.write_scenario_file(name, &content)?;
        Ok(content)
    }

    /// Stage 1 of the pipeline: ask the model to decompose the requested mood/effect across
    /// sound/text/visual axes before any NovaScript gets written, so stage 2 (the actual script
    /// edit) has an explicit multi-dimensional checklist instead of defaulting to a single
    /// `tint()` call. One retry on a JSON parse failure; if that retry also fails to parse, falls
    /// back to `None` (no plan) rather than blocking the whole generation request on a planning
    /// hiccup - the main generation prompt works fine without a plan, just less guided.
    async fn build_atmosphere_plan(&self, existing_content: &str, user_prompt: &str, api_key: &str) -> Option<AtmospherePlan> {
        // Stays on the pro model deliberately - composing an atmosphere across sound/text/visual
        // is a creative-judgment call, unlike the more mechanical script-writing/summary stages.
        let base_prompt = build_atmosphere_plan_prompt(existing_content, user_prompt);
        let first_output = generate_with_prompt(&base_prompt, api_key, llm::DEEPSEEK_MODEL_PRO).await.ok()?;
        let parse_error = match parse_atmosphere_plan(&first_output) {
            Ok(plan) => return Some(plan),
            Err(error) => error,
        };

        let retry_prompt = build_atmosphere_plan_retry_prompt(&base_prompt, &first_output, &parse_error);
        let retry_output = generate_with_prompt(&retry_prompt, api_key, llm::DEEPSEEK_MODEL_PRO).await.ok()?;
        parse_atmosphere_plan(&retry_output).ok()
    }

    async fn generate_script_with_retry_inner(&self, request: GenerateRequest) -> Result<GenerateResult, BackendError> {
        let config = self.get_config();
        let nova2_project_dir = Path::new(&config.nova2_project_dir);
        // Empty string (rather than failing) when target_file doesn't exist yet - that's the
        // legitimate "generate a brand new file from scratch" case build_generation_prompt handles.
        let existing_content = self.read_scenario_file(&request.target_file).unwrap_or_default();

        let atmosphere_plan = self
            .build_atmosphere_plan(&existing_content, &request.user_prompt, &config.deepseek_api_key)
            .await;

        let base_prompt = build_generation_prompt(
            nova2_project_dir,
            &config.vfx_notes_path,
            &request.target_file,
            &existing_content,
            &request.user_prompt,
            atmosphere_plan.as_ref(),
        )?;

        let known_asset_paths = list_known_asset_script_paths(nova2_project_dir);
        let known_characters = list_known_character_bind_names(nova2_project_dir);
        let known_audio_layout = list_known_audio_layout(nova2_project_dir);
        let known_shaders = list_known_shader_names(nova2_project_dir);

        let mut prompt = base_prompt.clone();
        let mut attempts = 0;
        let mut final_script = String::new();
        let mut last_error = None;

        while attempts < 3 {
            attempts += 1;
            let model_output = generate_with_prompt(&prompt, &config.deepseek_api_key, llm::DEEPSEEK_MODEL_FLASH).await?;
            let (new_chars, script) = parse_generated_output(&model_output)?;
            register_new_characters(nova2_project_dir, &new_chars)?;
            self.write_scenario_file_draft(&request.target_file, &script)?;
            final_script = script.clone();

            // Static checks first: Godot's load() on a missing texture/shader/track path silently
            // returns null instead of throwing, so any of these hallucinations would otherwise
            // sail through reload looking "successful". Catching them here also skips a Godot
            // reload round trip when one fires, and folds every category found into a single
            // retry instead of needing one retry per category.
            let asset_issues = find_unknown_asset_paths(&script, &known_asset_paths, &known_characters);
            let audio_issues = find_unknown_audio_tracks(&script, &known_audio_layout);
            let sound_issues = find_unknown_sound_tracks(&script, &known_audio_layout.one_shot_tracks);
            let shader_issues = find_unknown_shaders(&script, &known_shaders);
            if !asset_issues.is_empty() || !audio_issues.is_empty() || !sound_issues.is_empty() || !shader_issues.is_empty() {
                let mut messages = Vec::new();
                if !asset_issues.is_empty() {
                    messages.push(format_asset_path_issues(&asset_issues));
                }
                if !audio_issues.is_empty() {
                    messages.push(format_audio_track_issues(&audio_issues));
                }
                if !sound_issues.is_empty() {
                    messages.push(format_sound_track_issues(&sound_issues));
                }
                if !shader_issues.is_empty() {
                    messages.push(format_shader_issues(&shader_issues));
                }
                let error = PreviewBridgeError {
                    message: messages.join("；"),
                    line: None,
                    column: None,
                };
                prompt = build_retry_prompt(&base_prompt, &script, &error);
                last_error = Some(error);
                continue;
            }

            let changed_line = first_changed_line(&existing_content, &script);
            match self.send_reload()? {
                CommandResult::Success(_) => {
                    if let Some(line) = changed_line {
                        self.seek_to_changed_line_best_effort(&request.target_file, line);
                    }

                    // The changelog summary is a nice-to-have, not part of what the user is
                    // waiting on to see their change applied - reload already succeeded, so return
                    // immediately and write the summary + version-history snapshot in the
                    // background. Shaves a full LLM round trip off perceived latency for every
                    // successful generation; the frontend picks the summary up later via
                    // SUMMARY_READY_EVENT (see PreviewBridgeClient.onSummaryReady).
                    let runtime = self.clone();
                    let user_prompt = request.user_prompt.clone();
                    let target_file = request.target_file.clone();
                    let previous_content = existing_content.clone();
                    let applied_script = script.clone();
                    let api_key = config.deepseek_api_key.clone();
                    let project_dir = nova2_project_dir.to_path_buf();
                    tokio::spawn(async move {
                        let summary = runtime.build_change_summary(&user_prompt, &previous_content, &applied_script, &api_key).await;
                        if let Err(error) = version_history::snapshot_content(&project_dir, &target_file, &previous_content, &summary) {
                            eprintln!("vvn: 后台写入 {target_file} 的版本历史失败: {error}");
                        }
                        runtime.publish_summary_ready(&target_file, &summary);
                    });

                    return Ok(GenerateResult {
                        final_script: script,
                        attempts,
                        applied: true,
                        last_error: None,
                        summary: String::new(),
                    });
                }
                CommandResult::Error(error) => {
                    prompt = build_retry_prompt(&base_prompt, &script, &error.error);
                    last_error = Some(error.error);
                }
            }
        }

        Ok(GenerateResult {
            final_script,
            attempts,
            applied: false,
            last_error,
            summary: String::new(),
        })
    }

    /// Stage 3 (after a successful apply): asks the model for a short plain-language changelog
    /// entry describing what changed, for display in the UI and storage in version history. This
    /// is a nice-to-have, not a correctness gate - if the call fails for any reason, generation
    /// still succeeded and shouldn't be reported as a failure over a missing summary.
    async fn build_change_summary(&self, user_prompt: &str, previous_content: &str, final_script: &str, api_key: &str) -> String {
        let prompt = build_summary_prompt(user_prompt, previous_content, final_script);
        generate_with_prompt(&prompt, api_key, llm::DEEPSEEK_MODEL_FLASH).await.unwrap_or_default()
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

    fn send_locate(&self, file: &str, line: u32) -> Result<LocateResult, BackendError> {
        self.ensure_bridge_ready()?;
        let port = self.get_config().preview_bridge_port;
        let response = self.send_request(
            port,
            serde_json::json!({
                "id": self.next_command_id(),
                "method": "locate",
                "params": { "file": file, "line": line }
            }),
        )?;
        match response {
            BridgeResponse::Success(success) if success.ok => Ok(LocateResult {
                node_name: success.node_name.ok_or_else(|| BackendError::message("locate 响应缺少 nodeName 字段"))?,
                dialogue_index: success.dialogue_index.ok_or_else(|| BackendError::message("locate 响应缺少 dialogueIndex 字段"))?,
                node_record_id: success.node_record_id,
                reached: success.reached.unwrap_or(false),
            }),
            BridgeResponse::Error(error) => Err(BackendError::message(error.error.message)),
            _ => Err(BackendError::message("收到无法识别的 PreviewBridge locate 响应")),
        }
    }

    fn seek_to_changed_line_best_effort(&self, file: &str, line: u32) {
        let Ok(location) = self.send_locate(file, line) else {
            return;
        };
        let Some(node_record_id) = location.node_record_id else {
            eprintln!(
                "vvn: locate found {}:{} at {}#{} but it is not reached yet; preview stays where it is",
                file, line, location.node_name, location.dialogue_index
            );
            return;
        };
        if !location.reached {
            return;
        }
        if let Err(error) = self.send_seek(node_record_id, location.dialogue_index) {
            eprintln!("vvn: seek after locate failed for {file}:{line}: {error}");
        }
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

    /// Fired once the background changelog-summary task (spawned from
    /// `generate_script_with_retry_inner`'s success branch) finishes - best-effort, so a failed
    /// emit just means the frontend won't show this particular summary live (it can still be
    /// found later via list_script_versions).
    fn publish_summary_ready(&self, target_file: &str, summary: &str) {
        let _ = self.app.emit(
            SUMMARY_READY_EVENT,
            SummaryReadyEvent {
                target_file: target_file.to_string(),
                summary: summary.to_string(),
            },
        );
    }
}

fn first_changed_line(before: &str, after: &str) -> Option<u32> {
    if before.is_empty() || before == after {
        return None;
    }
    let mut before_lines = before.lines();
    let mut after_lines = after.lines();
    let mut line = 1u32;
    loop {
        match (before_lines.next(), after_lines.next()) {
            (Some(left), Some(right)) if left == right => line += 1,
            (Some(_), Some(_)) | (Some(_), None) | (None, Some(_)) => return Some(line),
            (None, None) => return None,
        }
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

fn is_image_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("png") | Some("jpg") | Some("jpeg") | Some("webp") | Some("gif")
    )
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
async fn caption_asset_cmd(runtime: State<'_, AppRuntime>, path: String) -> Result<String, PreviewBridgeError> {
    let config = runtime.get_config();
    let full_path = runtime.resolve_asset_path(&path);
    let description = llm::caption_asset(&full_path, &config.zhipu_api_key)
        .await
        .map_err(|error| error.to_bridge_error())?;
    runtime
        .upsert_asset_description(&path, description.clone())
        .map_err(|error| error.to_bridge_error())?;
    Ok(description)
}

#[tauri::command]
async fn generate_script_cmd(runtime: State<'_, AppRuntime>, prompt: String) -> Result<String, PreviewBridgeError> {
    let config = runtime.get_config();
    llm::generate_script(&prompt, &config.deepseek_api_key, llm::DEEPSEEK_MODEL_FLASH)
        .await
        .map_err(|error| error.to_bridge_error())
}

#[tauri::command]
fn get_project_session(runtime: State<'_, AppRuntime>) -> Result<ProjectSession, PreviewBridgeError> {
    Ok(runtime.get_project_session())
}

#[tauri::command]
fn leave_project(runtime: State<'_, AppRuntime>) -> Result<ProjectSession, PreviewBridgeError> {
    runtime.leave_project().map_err(|error| error.to_bridge_error())
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
    let state = runtime.ensure_bridge_ready().map_err(|error| error.to_bridge_error())?;
    runtime
        .sync_asset_description_index()
        .map_err(|error| error.to_bridge_error())?;
    Ok(LoadProjectResult { ok: true, state })
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
fn list_asset_files(runtime: State<'_, AppRuntime>) -> Result<Vec<AssetFile>, PreviewBridgeError> {
    runtime.list_asset_files().map_err(|error| error.to_bridge_error())
}

#[tauri::command]
fn read_scenario_file(runtime: State<'_, AppRuntime>, name: String) -> Result<String, PreviewBridgeError> {
    runtime.read_scenario_file(&name).map_err(|error| error.to_bridge_error())
}

#[tauri::command]
fn write_scenario_file(runtime: State<'_, AppRuntime>, name: String, content: String) -> Result<(), PreviewBridgeError> {
    runtime.write_scenario_file(&name, &content).map_err(|error| error.to_bridge_error())
}

#[tauri::command]
fn list_script_versions(runtime: State<'_, AppRuntime>, name: String) -> Result<Vec<VersionInfo>, PreviewBridgeError> {
    runtime.list_script_versions(&name).map_err(|error| error.to_bridge_error())
}

#[tauri::command]
fn restore_script_version(runtime: State<'_, AppRuntime>, name: String, version_id: String) -> Result<String, PreviewBridgeError> {
    runtime
        .restore_script_version(&name, &version_id)
        .map_err(|error| error.to_bridge_error())
}

#[tauri::command]
async fn generate_script_with_retry(
    runtime: State<'_, AppRuntime>,
    request: GenerateRequest,
) -> Result<GenerateResult, PreviewBridgeError> {
    runtime
        .generate_script_with_retry_inner(request)
        .await
        .map_err(|error| error.to_bridge_error())
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
            caption_asset_cmd,
            generate_script_cmd,
            generate_script_with_retry,
            get_project_session,
            leave_project,
            get_runtime_status,
            load_project,
            reload_preview,
            seek,
            list_scenario_files,
            list_asset_files,
            read_scenario_file,
            write_scenario_file,
            list_script_versions,
            restore_script_version
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod bridge_response_tests {
    use super::*;

    // Regression test for a real bug: BridgeSuccessResponse's fields are all optional except `ok`,
    // and serde ignores fields a struct doesn't declare - so if Success were tried before
    // Error/Event in the untagged enum, every real error response and every unsolicited
    // state_changed push would silently misdeserialize as a "successful" empty-state response.

    #[test]
    fn error_payload_deserializes_as_error_not_success() {
        let json = r#"{"id":1,"ok":false,"error":{"message":"Failed to parse ch1.txt","line":3,"column":5}}"#;
        let response: BridgeResponse = serde_json::from_str(json).unwrap();
        match response {
            BridgeResponse::Error(error) => {
                assert_eq!(error.error.message, "Failed to parse ch1.txt");
                assert_eq!(error.error.line, Some(3));
            }
            other => panic!("expected BridgeResponse::Error, got {other:?}"),
        }
    }

    #[test]
    fn success_payload_deserializes_as_success() {
        let json = r#"{"id":1,"ok":true,"state":{"currentNodeRecordId":2,"currentDialogueIndex":1,"startNodeNames":["ch1"]}}"#;
        let response: BridgeResponse = serde_json::from_str(json).unwrap();
        match response {
            BridgeResponse::Success(success) => {
                assert!(success.ok);
                assert_eq!(success.state.unwrap().current_node_record_id, Some(2));
            }
            other => panic!("expected BridgeResponse::Success, got {other:?}"),
        }
    }

    #[test]
    fn state_changed_push_deserializes_as_event_not_success() {
        let json = r#"{"method":"state_changed","ok":true,"state":{"currentNodeRecordId":0,"currentDialogueIndex":0,"startNodeNames":[]}}"#;
        let response: BridgeResponse = serde_json::from_str(json).unwrap();
        match response {
            BridgeResponse::Event(event) => {
                assert_eq!(event.method, "state_changed");
            }
            other => panic!("expected BridgeResponse::Event, got {other:?}"),
        }
    }

    #[test]
    fn map_command_response_surfaces_structured_error() {
        let json = r#"{"id":1,"ok":false,"error":{"message":"unexpected token","line":7,"column":2}}"#;
        let response: BridgeResponse = serde_json::from_str(json).unwrap();
        // map_command_response only needs &self for publish_state on the success path, which this
        // input never reaches - so any AppRuntime would do, but constructing one needs a real
        // AppHandle. Replicate its match logic directly to keep this test Tauri-runtime-free.
        let result = match response {
            BridgeResponse::Success(success) if success.ok => CommandResult::ok(success.state),
            BridgeResponse::Error(error) => CommandResult::error(error.error),
            _ => panic!("response should have matched Error, not fallen through"),
        };
        match result {
            CommandResult::Error(failure) => {
                assert_eq!(failure.error.message, "unexpected token");
                assert_eq!(failure.error.line, Some(7));
            }
            CommandResult::Success(_) => panic!("expected an error CommandResult"),
        }
    }
    #[test]
    fn first_changed_line_returns_none_for_new_file_or_no_change() {
        assert_eq!(first_changed_line("", "a\n"), None);
        assert_eq!(first_changed_line("a\nb\n", "a\nb\n"), None);
    }

    #[test]
    fn first_changed_line_finds_modified_inserted_and_deleted_lines() {
        assert_eq!(first_changed_line("a\nb\nc\n", "a\nB\nc\n"), Some(2));
        assert_eq!(first_changed_line("a\nb\n", "a\nx\nb\n"), Some(2));
        assert_eq!(first_changed_line("a\nb\nc\n", "a\nc\n"), Some(2));
    }

    #[test]
    fn locate_payload_deserializes_from_bridge_success() {
        let json = r#"{"id":3,"ok":true,"nodeName":"ch1","dialogueIndex":4,"nodeRecordId":12,"reached":true}"#;
        let response: BridgeResponse = serde_json::from_str(json).unwrap();
        match response {
            BridgeResponse::Success(success) => {
                assert_eq!(success.node_name.as_deref(), Some("ch1"));
                assert_eq!(success.dialogue_index, Some(4));
                assert_eq!(success.node_record_id, Some(12));
                assert_eq!(success.reached, Some(true));
            }
            other => panic!("expected BridgeResponse::Success, got {other:?}"),
        }
    }
}
