import { useEffect, useState } from "react";
import { previewBridgeClient, type AppConfig, type RuntimeStatus } from "./bridge/PreviewBridgeClient";
import type { PreviewBridgeError, PreviewBridgeState, ScenarioFile } from "./bridge/protocol";
import "./App.css";

type StatusTone = "idle" | "busy" | "success" | "error";

type SeekDraft = {
  nodeRecordId: string;
  dialogueIndex: string;
};

// In-memory cache of file content loaded from disk via read_scenario_file, edited locally, and
// only written back via write_scenario_file when the user clicks "保存并刷新" - not autosaved.
type ScriptDraftMap = Record<string, string>;

const emptyState: PreviewBridgeState = {
  currentNodeRecordId: null,
  currentDialogueIndex: null,
  startNodeNames: [],
};

const defaultConfig: AppConfig = {
  nova2ProjectDir: "",
  godotExecutablePath: "",
  previewBridgePort: 9999,
};

const workflowItems = [
  "接收用户上传的自然语言剧本或剧情方案",
  "LLM 在理解 NovaScript 约束后生成可落盘的脚本改动",
  "把改动写回 scenario 文件并通过 PreviewBridge 触发 reload",
  "用 seek / get_state 回到指定节点验证实际游戏效果",
];

const formatStateValue = (value: number | null) => (value === null ? "—" : String(value));

function App() {
  const [config, setConfig] = useState<AppConfig>(defaultConfig);
  const [scenarioFiles, setScenarioFiles] = useState<ScenarioFile[]>([]);
  const [selectedScript, setSelectedScript] = useState<string | null>(null);
  const [drafts, setDrafts] = useState<ScriptDraftMap>({});
  const [bridgeState, setBridgeState] = useState<PreviewBridgeState>(emptyState);
  const [runtimeStatus, setRuntimeStatus] = useState<RuntimeStatus | null>(null);
  const [seekDraft, setSeekDraft] = useState<SeekDraft>({ nodeRecordId: "1", dialogueIndex: "0" });
  const [statusTone, setStatusTone] = useState<StatusTone>("idle");
  const [statusTitle, setStatusTitle] = useState("等待配置后端");
  const [statusMessage, setStatusMessage] = useState("先填写 Nova2 工程目录和 Godot 可执行文件路径，再加载工程，Rust 后端会负责拉起 Godot 并连接 PreviewBridge。\n");
  const [lastError, setLastError] = useState<PreviewBridgeError | null>(null);
  const [isHydrating, setIsHydrating] = useState(true);
  const [isSavingConfig, setIsSavingConfig] = useState(false);
  const [isLoadingProject, setIsLoadingProject] = useState(false);
  const [isLoadingScript, setIsLoadingScript] = useState(false);
  const [isSaving, setIsSaving] = useState(false);
  const [isSeeking, setIsSeeking] = useState(false);

  useEffect(() => {
    const bootstrap = async () => {
      try {
        const [savedConfig, runtime] = await Promise.all([
          previewBridgeClient.getAppConfig(),
          previewBridgeClient.getRuntimeStatus(),
        ]);
        setConfig(savedConfig);
        setRuntimeStatus(runtime);
        if (runtime.state) {
          setBridgeState(runtime.state);
          setSeekDraft({
            nodeRecordId: String(runtime.state.currentNodeRecordId ?? 1),
            dialogueIndex: String(runtime.state.currentDialogueIndex ?? 0),
          });
        }
      } catch (error) {
        applyUnknownError(error, "读取后端配置失败");
      } finally {
        setIsHydrating(false);
      }
    };

    void bootstrap();
  }, []);

  useEffect(() => {
    return previewBridgeClient.onStateChanged((event) => {
      setBridgeState(event.state);
      setRuntimeStatus((current) =>
        current
          ? { ...current, isConnected: true, state: event.state }
          : { isConnected: true, isGodotRunning: true, config, state: event.state },
      );
      setSeekDraft({
        nodeRecordId: String(event.state.currentNodeRecordId ?? 1),
        dialogueIndex: String(event.state.currentDialogueIndex ?? 0),
      });
      setStatusTone("success");
      setStatusTitle("预览状态已同步");
      setStatusMessage(`${selectedScript ? `当前脚本 ${selectedScript} ` : ""}已同步到预览态。nodeRecordId=${event.state.currentNodeRecordId ?? "—"}，dialogueIndex=${event.state.currentDialogueIndex ?? "—"}`);
      setLastError(null);
    });
  }, [config, selectedScript]);

  const editorValue = selectedScript ? drafts[selectedScript] ?? "" : "";

  const applyError = (error: PreviewBridgeError, fallbackTitle: string) => {
    setStatusTone("error");
    setStatusTitle(fallbackTitle);
    setStatusMessage(error.message);
    setLastError(error);
  };

  const applyUnknownError = (error: unknown, fallbackTitle: string) => {
    const bridgeError: PreviewBridgeError = {
      message: error instanceof Error ? error.message : String(error),
    };
    applyError(bridgeError, fallbackTitle);
  };

  const refreshRuntimeStatus = async () => {
    const runtime = await previewBridgeClient.getRuntimeStatus();
    setRuntimeStatus(runtime);
  };

  const handleConfigChange = <K extends keyof AppConfig>(key: K, value: AppConfig[K]) => {
    setConfig((current) => ({
      ...current,
      [key]: value,
    }));
  };

  const handleSaveConfig = async () => {
    setIsSavingConfig(true);
    setStatusTone("busy");
    setStatusTitle("正在保存配置");
    setStatusMessage("Rust 后端会把 Nova2 路径与 Godot 可执行文件路径持久化到本地配置。\n");
    setLastError(null);
    try {
      const savedConfig = await previewBridgeClient.saveAppConfig(config);
      setConfig(savedConfig);
      await refreshRuntimeStatus();
      setStatusTone("success");
      setStatusTitle("配置已保存");
      setStatusMessage("现在可以加载工程，后端会尝试启动 Godot 并握手 PreviewBridge。\n");
    } catch (error) {
      applyUnknownError(error, "保存配置失败");
    } finally {
      setIsSavingConfig(false);
    }
  };

  const loadScriptContent = async (name: string) => {
    setIsLoadingScript(true);
    try {
      const content = await previewBridgeClient.readScenarioFile(name);
      setDrafts((current) => ({ ...current, [name]: content }));
      setSelectedScript(name);
      setStatusTone("idle");
      setStatusTitle("脚本已切换");
      setStatusMessage(`已加载 ${name}。修改后点"保存并刷新"会写回 scenario 文件并触发 reload。`);
      setLastError(null);
    } catch (error) {
      applyUnknownError(error, `读取 ${name} 失败`);
    } finally {
      setIsLoadingScript(false);
    }
  };

  const handleSelectScript = (name: string) => {
    if (name === selectedScript) {
      return;
    }
    if (drafts[name] !== undefined) {
      setSelectedScript(name);
      setStatusTone("idle");
      setStatusTitle("脚本已切换");
      setStatusMessage(`已切换到 ${name}。`);
      setLastError(null);
      return;
    }
    void loadScriptContent(name);
  };

  const handleEditorChange = (value: string) => {
    if (!selectedScript) {
      return;
    }
    setDrafts((current) => ({
      ...current,
      [selectedScript]: value,
    }));
  };

  const handleLoadProject = async () => {
    setIsLoadingProject(true);
    setStatusTone("busy");
    setStatusTitle("正在加载工程");
    setStatusMessage("Rust 后端正在启动 Godot，并等待 PreviewBridge 端口握手成功...\n");
    setLastError(null);
    try {
      const result = await previewBridgeClient.loadProject();
      setBridgeState(result.state);
      const files = await previewBridgeClient.listScenarioFiles();
      setScenarioFiles(files);
      await refreshRuntimeStatus();
      setStatusTone("success");
      setStatusTitle("工程已连接");
      setStatusMessage(`已连接 ${config.nova2ProjectDir || "Nova2 工程"}，找到 ${files.length} 个剧本文件。点文件树里的某一项开始编辑。`);
      if (files.length > 0 && !selectedScript) {
        await loadScriptContent(files[0].name);
      }
    } catch (error) {
      applyUnknownError(error, "加载工程失败");
    } finally {
      setIsLoadingProject(false);
    }
  };

  const handleSave = async () => {
    if (!selectedScript) {
      return;
    }
    setIsSaving(true);
    setStatusTone("busy");
    setStatusTitle("正在保存并刷新预览");
    setStatusMessage(`准备把 ${selectedScript} 的脚本改动写回 scenario，并通过真实 PreviewBridge 触发 reload...`);
    setLastError(null);
    try {
      await previewBridgeClient.writeScenarioFile(selectedScript, drafts[selectedScript] ?? "");
      const result = await previewBridgeClient.reloadPreview();
      await refreshRuntimeStatus();
      if (!result.ok) {
        applyError(result.error, "保存失败");
        return;
      }
      setStatusTone("success");
      setStatusTitle("保存完成");
      setStatusMessage(`已写回 ${selectedScript} 并触发 reload。当前位置保持在 nodeRecordId=${result.state?.currentNodeRecordId ?? "—"}。`);
    } catch (error) {
      applyUnknownError(error, "保存失败");
    } finally {
      setIsSaving(false);
    }
  };

  const handleSeek = async () => {
    const nodeRecordId = Number.parseInt(seekDraft.nodeRecordId, 10);
    const dialogueIndex = Number.parseInt(seekDraft.dialogueIndex, 10);
    setIsSeeking(true);
    setStatusTone("busy");
    setStatusTitle("正在 seek");
    setStatusMessage(`准备跳到 ${selectedScript} 的 nodeRecordId=${seekDraft.nodeRecordId}，dialogueIndex=${seekDraft.dialogueIndex}`);
    setLastError(null);
    try {
      const result = await previewBridgeClient.seek(nodeRecordId, dialogueIndex);
      await refreshRuntimeStatus();
      if (!result.ok) {
        applyError(result.error, "Seek 失败");
        return;
      }
      setStatusTone("success");
      setStatusTitle("Seek 完成");
      setStatusMessage(`已跳转到 nodeRecordId=${result.state?.currentNodeRecordId ?? "—"}，dialogueIndex=${result.state?.currentDialogueIndex ?? "—"}，可以继续对照实际演出效果。`);
    } catch (error) {
      applyUnknownError(error, "Seek 失败");
    } finally {
      setIsSeeking(false);
    }
  };

  return (
    <main className="app-shell">
      <section className="topbar card">
        <div>
          <p className="eyebrow">VVN v0</p>
          <h1>Natural Language → NovaScript → Preview</h1>
          <p className="subtitle">VVN 面向的是“自然语言剧本/方案 → LLM 解析成 NovaScript → 实际改动 scenario 文件 → 立即预览游戏效果”的整条工作流。</p>
        </div>
        <div className="path-form compact-status">
          <span className="field-label">后端运行状态</span>
          <div className="runtime-pills">
            <span className={`runtime-pill ${runtimeStatus?.isGodotRunning ? "on" : "off"}`}>Godot {runtimeStatus?.isGodotRunning ? "Running" : "Idle"}</span>
            <span className={`runtime-pill ${runtimeStatus?.isConnected ? "on" : "off"}`}>Bridge {runtimeStatus?.isConnected ? "Connected" : "Disconnected"}</span>
            <span className="runtime-pill neutral">Port {config.previewBridgePort}</span>
          </div>
        </div>
      </section>

      <section className="settings-layout">
        <section className="settings-panel card">
          <div className="panel-header">
            <h2>本地配置</h2>
            <span>两个仓库独立配置，不硬编码路径</span>
          </div>
          <div className="settings-grid">
            <label>
              <span className="field-label">Nova2 工程目录</span>
              <input
                value={config.nova2ProjectDir}
                onChange={(event) => handleConfigChange("nova2ProjectDir", event.currentTarget.value)}
                placeholder="例如 E:/nova2/Nova2"
              />
            </label>
            <label>
              <span className="field-label">Godot 可执行文件路径</span>
              <input
                value={config.godotExecutablePath}
                onChange={(event) => handleConfigChange("godotExecutablePath", event.currentTarget.value)}
                placeholder="例如 C:/Tools/Godot_v4.6.3-stable_mono_win64_console.exe"
              />
            </label>
            <label>
              <span className="field-label">PreviewBridge 端口</span>
              <input
                value={String(config.previewBridgePort)}
                onChange={(event) => handleConfigChange("previewBridgePort", Number.parseInt(event.currentTarget.value || "9999", 10) || 9999)}
                inputMode="numeric"
              />
            </label>
          </div>
          <div className="settings-actions">
            <button type="button" onClick={handleSaveConfig} disabled={isSavingConfig || isHydrating}>
              {isSavingConfig ? "保存中..." : "保存配置"}
            </button>
            <button type="button" onClick={handleLoadProject} disabled={isLoadingProject || isHydrating}>
              {isLoadingProject ? "连接中..." : "启动 Godot 并连接"}
            </button>
          </div>
        </section>

        <section className="workflow card">
          <div className="panel-header">
            <h2>目标工作流</h2>
            <span>当前已切到真实 Tauri invoke + event</span>
          </div>
          <div className="workflow-grid">
            {workflowItems.map((item, index) => (
              <article key={item} className="workflow-step">
                <span className="workflow-index">0{index + 1}</span>
                <p>{item}</p>
              </article>
            ))}
          </div>
        </section>
      </section>

      <section className="workspace">
        <aside className="card sidebar">
          <div className="panel-header">
            <h2>Scenario 文件树</h2>
            <span>{selectedScript ?? "未选择"}</span>
          </div>
          <div className="tree-list" role="tree" aria-label="Script file tree">
            {scenarioFiles.length === 0 ? (
              <p className="editor-hint">点上方"启动 Godot 并连接"加载工程后，这里会列出 resources/scenarios/*.txt。</p>
            ) : (
              scenarioFiles.map((file) => {
                const isActive = file.name === selectedScript;
                return (
                  <button
                    key={file.name}
                    type="button"
                    className={`tree-node selectable ${isActive ? "active" : ""}`}
                    style={{ paddingLeft: "16px" }}
                    onClick={() => handleSelectScript(file.name)}
                  >
                    <span className="tree-node-icon">•</span>
                    <span>{file.name}</span>
                  </button>
                );
              })
            )}
          </div>
        </aside>

        <section className="editor-column">
          <div className="card editor-panel">
            <div className="panel-header">
              <div>
                <h2>NovaScript 编辑区</h2>
                <span>{selectedScript ?? "未选择"}</span>
              </div>
              <button type="button" onClick={handleSave} disabled={!selectedScript || isSaving || isLoadingProject || isLoadingScript || isHydrating}>
                {isSaving ? "保存中..." : "保存并刷新"}
              </button>
            </div>
            <p className="editor-hint">这里会承接 LLM 对自然语言剧本的解析结果。修改后点"保存并刷新"会通过 write_scenario_file 写回磁盘，再触发 PreviewBridge 的 reload。</p>
            <textarea
              value={editorValue}
              onChange={(event) => handleEditorChange(event.currentTarget.value)}
              disabled={!selectedScript || isLoadingScript}
              spellCheck={false}
              aria-label="NovaScript editor"
            />
          </div>

          <div className="bottom-row">
            <section className="card seek-panel">
              <div className="panel-header">
                <h2>Seek 控件</h2>
                <span>映射到 `seek(nodeRecordId, dialogueIndex)`</span>
              </div>
              <div className="seek-grid">
                <label>
                  <span className="field-label">Node Record Id</span>
                  <input
                    value={seekDraft.nodeRecordId}
                    onChange={(event) => setSeekDraft((current) => ({ ...current, nodeRecordId: event.currentTarget.value }))}
                    inputMode="numeric"
                  />
                </label>
                <label>
                  <span className="field-label">Dialogue Index</span>
                  <input
                    value={seekDraft.dialogueIndex}
                    onChange={(event) => setSeekDraft((current) => ({ ...current, dialogueIndex: event.currentTarget.value }))}
                    inputMode="numeric"
                  />
                </label>
                <button type="button" onClick={handleSeek} disabled={isSeeking || isLoadingProject || isHydrating}>
                  {isSeeking ? "跳转中..." : "执行 Seek"}
                </button>
              </div>
            </section>

            <section className={`card status-panel tone-${statusTone}`}>
              <div className="panel-header">
                <h2>状态面板</h2>
                <span>{statusTitle}</span>
              </div>
              <dl className="state-grid">
                <div>
                  <dt>currentNodeRecordId</dt>
                  <dd>{formatStateValue(bridgeState.currentNodeRecordId)}</dd>
                </div>
                <div>
                  <dt>currentDialogueIndex</dt>
                  <dd>{formatStateValue(bridgeState.currentDialogueIndex)}</dd>
                </div>
                <div>
                  <dt>startNodeNames</dt>
                  <dd>{bridgeState.startNodeNames.length ? bridgeState.startNodeNames.join(", ") : "—"}</dd>
                </div>
              </dl>
              <p className="status-message">{statusMessage}</p>
              {lastError ? (
                <p className="error-meta">
                  line {lastError.line ?? "—"}, column {lastError.column ?? "—"}
                </p>
              ) : null}
            </section>
          </div>
        </section>
      </section>
    </main>
  );
}

export default App;
