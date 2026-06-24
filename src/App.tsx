import { useEffect, useMemo, useState } from "react";
import { previewBridgeClient, type AppConfig, type RuntimeStatus } from "./bridge/PreviewBridgeClient";
import type {
  AssetFile,
  GenerateResult,
  PreviewBridgeError,
  PreviewBridgeState,
  ProjectSession,
  ScenarioFile,
  VersionInfo,
} from "./bridge/protocol";
import "./App.css";

type StatusTone = "idle" | "busy" | "success" | "error";

type SeekDraft = {
  nodeRecordId: string;
  dialogueIndex: string;
};

type ScriptDraftMap = Record<string, string>;
type CaptionDraftMap = Record<string, string>;

type GeneratePanelState = {
  userPrompt: string;
  targetFile: string;
  result: GenerateResult | null;
};

const emptyState: PreviewBridgeState = {
  currentNodeRecordId: null,
  currentDialogueIndex: null,
  startNodeNames: [],
};

const emptyProjectSession: ProjectSession = {
  hasProject: false,
  projectDir: "",
};

const defaultConfig: AppConfig = {
  nova2ProjectDir: "",
  godotExecutablePath: "",
  previewBridgePort: 9999,
  zhipuApiKey: "",
  deepseekApiKey: "",
  vfxNotesPath: "",
};

const formatStateValue = (value: number | null) => (value === null ? "—" : String(value));

const formatTimestamp = (timestampMs: number) => new Date(timestampMs).toLocaleString();

const formatBridgeError = (error?: PreviewBridgeError | null) => {
  if (!error) {
    return "—";
  }
  const location = error.line || error.column ? ` (line ${error.line ?? "—"}, column ${error.column ?? "—"})` : "";
  return `${error.message}${location}`;
};

function App() {
  const [config, setConfig] = useState<AppConfig>(defaultConfig);
  const [projectSession, setProjectSession] = useState<ProjectSession>(emptyProjectSession);
  const [scenarioFiles, setScenarioFiles] = useState<ScenarioFile[]>([]);
  const [assetFiles, setAssetFiles] = useState<AssetFile[]>([]);
  const [selectedScript, setSelectedScript] = useState<string | null>(null);
  const [selectedAsset, setSelectedAsset] = useState<string | null>(null);
  const [drafts, setDrafts] = useState<ScriptDraftMap>({});
  const [captionDrafts, setCaptionDrafts] = useState<CaptionDraftMap>({});
  const [bridgeState, setBridgeState] = useState<PreviewBridgeState>(emptyState);
  const [runtimeStatus, setRuntimeStatus] = useState<RuntimeStatus | null>(null);
  const [seekDraft, setSeekDraft] = useState<SeekDraft>({ nodeRecordId: "1", dialogueIndex: "0" });
  const [generatePanel, setGeneratePanel] = useState<GeneratePanelState>({
    userPrompt: "",
    targetFile: "",
    result: null,
  });
  const [statusTone, setStatusTone] = useState<StatusTone>("idle");
  const [statusTitle, setStatusTitle] = useState("等待进入项目");
  const [statusMessage, setStatusMessage] = useState("当前还没有进入项目。先选择 Nova2 工程目录和 Godot 路径，然后进入项目。进入后主界面会切换到自然语言开发工作台。\n");
  const [lastError, setLastError] = useState<PreviewBridgeError | null>(null);
  const [isHydrating, setIsHydrating] = useState(true);
  const [isSavingConfig, setIsSavingConfig] = useState(false);
  const [isLoadingProject, setIsLoadingProject] = useState(false);
  const [isLeavingProject, setIsLeavingProject] = useState(false);
  const [isLoadingScript, setIsLoadingScript] = useState(false);
  const [isSaving, setIsSaving] = useState(false);
  const [isSeeking, setIsSeeking] = useState(false);
  const [isCaptioningAsset, setIsCaptioningAsset] = useState<string | null>(null);
  const [isGeneratingScript, setIsGeneratingScript] = useState(false);
  const [isSettingsOpen, setIsSettingsOpen] = useState(false);
  const [isDeveloperOpen, setIsDeveloperOpen] = useState(false);
  const [isVersionPanelOpen, setIsVersionPanelOpen] = useState(false);
  const [versionList, setVersionList] = useState<VersionInfo[]>([]);
  const [isLoadingVersions, setIsLoadingVersions] = useState(false);
  const [restoringVersionId, setRestoringVersionId] = useState<string | null>(null);
  const [versionSearch, setVersionSearch] = useState("");

  const editorValue = selectedScript ? drafts[selectedScript] ?? "" : "";
  const selectedCaption = selectedAsset ? captionDrafts[selectedAsset] ?? "" : "";
  const hasProject = projectSession.hasProject;

  const filteredVersionList = useMemo(() => {
    const query = versionSearch.trim().toLowerCase();
    if (!query) {
      return versionList;
    }
    return versionList.filter((version) => `${version.summary} ${version.preview}`.toLowerCase().includes(query));
  }, [versionList, versionSearch]);

  const assetGroups = useMemo(() => {
    const groups = new Map<string, AssetFile[]>();
    for (const asset of assetFiles) {
      const parts = asset.path.split("/");
      const groupName = parts.length >= 2 ? `${parts[0]}/${parts[1]}` : asset.path;
      const current = groups.get(groupName) ?? [];
      current.push(asset);
      groups.set(groupName, current);
    }
    return Array.from(groups.entries());
  }, [assetFiles]);

  useEffect(() => {
    const bootstrap = async () => {
      try {
        const [savedConfig, runtime, session] = await Promise.all([
          previewBridgeClient.getAppConfig(),
          previewBridgeClient.getRuntimeStatus(),
          previewBridgeClient.getProjectSession(),
        ]);
        setConfig(savedConfig);
        setRuntimeStatus(runtime);
        setProjectSession(session);
        if (runtime.state) {
          setBridgeState(runtime.state);
          setSeekDraft({
            nodeRecordId: String(runtime.state.currentNodeRecordId ?? 1),
            dialogueIndex: String(runtime.state.currentDialogueIndex ?? 0),
          });
        }
        if (session.hasProject) {
          setStatusTone("idle");
          setStatusTitle("已识别本地项目配置");
          setStatusMessage(`当前记录的项目是 ${session.projectDir}。点击“进入项目”后会拉起引擎并切到 AI 工作台。`);
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
      setStatusMessage(`当前预览定位：nodeRecordId=${event.state.currentNodeRecordId ?? "—"}，dialogueIndex=${event.state.currentDialogueIndex ?? "—"}。后续针对“自然语言修改需求”的 AI 优先基于这里附近做改动。`);
      setLastError(null);
    });
  }, [config]);

  useEffect(() => {
    return previewBridgeClient.onSummaryReady((event) => {
      setGeneratePanel((current) =>
        current.targetFile === event.targetFile && current.result
          ? { ...current, result: { ...current.result, summary: event.summary } }
          : current,
      );
      if (selectedScript === event.targetFile && isVersionPanelOpen) {
        previewBridgeClient
          .listScriptVersions(event.targetFile)
          .then(setVersionList)
          .catch((error) => applyUnknownError(error, "刷新版本历史失败"));
      }
    });
  }, [selectedScript, isVersionPanelOpen]);

  const applyError = (error: PreviewBridgeError, fallbackTitle: string) => {
    setStatusTone("error");
    setStatusTitle(fallbackTitle);
    setStatusMessage(error.message);
    setLastError(error);
  };

  const isPreviewBridgeError = (value: unknown): value is PreviewBridgeError =>
    typeof value === "object" && value !== null && "message" in value && typeof (value as { message: unknown }).message === "string";

  const applyUnknownError = (error: unknown, fallbackTitle: string) => {
    // Tauri's invoke() rejects with whatever the Rust command's Err variant serialized to - for
    // every command here that's a plain PreviewBridgeError object, not a JS Error instance. Falling
    // through to String(error) on that shape produces a useless "[object Object]".
    const bridgeError: PreviewBridgeError = isPreviewBridgeError(error)
      ? error
      : { message: error instanceof Error ? error.message : String(error) };
    applyError(bridgeError, fallbackTitle);
  };

  const refreshRuntimeStatus = async () => {
    const runtime = await previewBridgeClient.getRuntimeStatus();
    setRuntimeStatus(runtime);
  };

  const refreshProjectSession = async () => {
    const session = await previewBridgeClient.getProjectSession();
    setProjectSession(session);
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
    setStatusTitle("正在保存设置");
    setStatusMessage("本地项目配置和 API 配置会统一由后端保存。本轮 UI 已把这些设置收纳到设置面板中。\n");
    setLastError(null);
    try {
      const savedConfig = await previewBridgeClient.saveAppConfig(config);
      setConfig(savedConfig);
      await Promise.all([refreshRuntimeStatus(), refreshProjectSession()]);
      setStatusTone("success");
      setStatusTitle("设置已保存");
      setStatusMessage(hasProject ? "你可以继续留在当前项目，或点击“切换项目/设置”修改路径后重新进入项目。" : "设置已保存。下一步点击“进入项目”即可切到自然语言开发工作台。");
    } catch (error) {
      applyUnknownError(error, "保存设置失败");
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
      setGeneratePanel((current) => ({
        ...current,
        targetFile: current.targetFile || name,
      }));
      setIsVersionPanelOpen(false);
      setVersionList([]);
      setVersionSearch("");
      setStatusTone("idle");
      setStatusTitle("脚本已切换");
      setStatusMessage(`已加载 ${name}。开发者模块中的 NovaScript 编辑区可继续手工调整。`);
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
      setGeneratePanel((current) => ({
        ...current,
        targetFile: current.targetFile || name,
      }));
      setIsVersionPanelOpen(false);
      setVersionList([]);
      setVersionSearch("");
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

  const handleCaptionChange = (value: string) => {
    if (!selectedAsset) {
      return;
    }
    setCaptionDrafts((current) => ({
      ...current,
      [selectedAsset]: value,
    }));
  };

  const handleLoadProject = async () => {
    setIsLoadingProject(true);
    setStatusTone("busy");
    setStatusTitle("正在进入项目");
    setStatusMessage("Rust 后端正在启动 Godot、连接 PreviewBridge，并同步资源描述索引...\n");
    setLastError(null);
    try {
      const result = await previewBridgeClient.loadProject();
      setBridgeState(result.state);
      const [files, assets] = await Promise.all([
        previewBridgeClient.listScenarioFiles(),
        previewBridgeClient.listAssetFiles(),
      ]);
      setScenarioFiles(files);
      setAssetFiles(assets);
      setGeneratePanel((current) => ({
        ...current,
        targetFile: current.targetFile || files[0]?.name || "",
      }));
      setProjectSession({ hasProject: true, projectDir: config.nova2ProjectDir });
      await refreshRuntimeStatus();
      setStatusTone("success");
      setStatusTitle("项目已进入");
      setStatusMessage(`已进入 ${config.nova2ProjectDir || "Nova2 工程"}。主工作区已切换到 AI 生成；开发者模块被收纳在下方。`);
      if (files.length > 0 && !selectedScript) {
        await loadScriptContent(files[0].name);
      }
    } catch (error) {
      applyUnknownError(error, "进入项目失败");
    } finally {
      setIsLoadingProject(false);
    }
  };

  const handleLeaveProject = async () => {
    setIsLeavingProject(true);
    setStatusTone("busy");
    setStatusTitle("正在退出当前项目");
    setStatusMessage("将断开当前运行时会话，但保留本地设置，方便之后重新进入或切换项目。\n");
    setLastError(null);
    try {
      const session = await previewBridgeClient.leaveProject();
      setProjectSession(session);
      setScenarioFiles([]);
      setAssetFiles([]);
      setSelectedScript(null);
      setSelectedAsset(null);
      setDrafts({});
      setCaptionDrafts({});
      setBridgeState(emptyState);
      setGeneratePanel((current) => ({ ...current, targetFile: "", result: null }));
      setIsDeveloperOpen(false);
      await refreshRuntimeStatus();
      setStatusTone("success");
      setStatusTitle("已退出项目");
      setStatusMessage("界面已回到项目入口模式。你可以在设置里切换路径，再重新进入项目。\n");
    } catch (error) {
      applyUnknownError(error, "退出项目失败");
    } finally {
      setIsLeavingProject(false);
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

  const handleToggleVersionHistory = async () => {
    if (!selectedScript) {
      return;
    }
    const willOpen = !isVersionPanelOpen;
    setIsVersionPanelOpen(willOpen);
    if (!willOpen) {
      return;
    }
    setIsLoadingVersions(true);
    try {
      const versions = await previewBridgeClient.listScriptVersions(selectedScript);
      setVersionList(versions);
    } catch (error) {
      applyUnknownError(error, "读取版本历史失败");
    } finally {
      setIsLoadingVersions(false);
    }
  };

  const handleRestoreVersion = async (versionId: string) => {
    if (!selectedScript) {
      return;
    }
    setRestoringVersionId(versionId);
    setStatusTone("busy");
    setStatusTitle("正在恢复历史版本");
    setStatusMessage(`准备把 ${selectedScript} 恢复到选中的历史版本，并触发引擎 reload...`);
    setLastError(null);
    try {
      const content = await previewBridgeClient.restoreScriptVersion(selectedScript, versionId);
      setDrafts((current) => ({ ...current, [selectedScript]: content }));
      const result = await previewBridgeClient.reloadPreview();
      await refreshRuntimeStatus();
      const versions = await previewBridgeClient.listScriptVersions(selectedScript);
      setVersionList(versions);
      if (!result.ok) {
        applyError(result.error, "恢复后 reload 失败");
        return;
      }
      setStatusTone("success");
      setStatusTitle("已恢复历史版本");
      setStatusMessage(`已把 ${selectedScript} 恢复到选中版本并触发 reload。`);
    } catch (error) {
      applyUnknownError(error, "恢复历史版本失败");
    } finally {
      setRestoringVersionId(null);
    }
  };

  const handleSeek = async () => {
    const nodeRecordId = Number.parseInt(seekDraft.nodeRecordId, 10);
    const dialogueIndex = Number.parseInt(seekDraft.dialogueIndex, 10);
    setIsSeeking(true);
    setStatusTone("busy");
    setStatusTitle("正在 seek");
    setStatusMessage(`准备跳到当前预览附近：nodeRecordId=${seekDraft.nodeRecordId}，dialogueIndex=${seekDraft.dialogueIndex}`);
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
      setStatusMessage(`已跳转到 nodeRecordId=${result.state?.currentNodeRecordId ?? "—"}，dialogueIndex=${result.state?.currentDialogueIndex ?? "—"}。后续 AI 应优先围绕这里做修改。`);
    } catch (error) {
      applyUnknownError(error, "Seek 失败");
    } finally {
      setIsSeeking(false);
    }
  };

  const handleCaptionAsset = async (path: string) => {
    setSelectedAsset(path);
    setIsCaptioningAsset(path);
    setStatusTone("busy");
    setStatusTitle("正在生成资源描述");
    setStatusMessage(`正在分析 ${path}，结果会同时写回项目内的资源描述索引。`);
    setLastError(null);
    try {
      const result = await previewBridgeClient.captionAsset(path);
      setCaptionDrafts((current) => ({ ...current, [path]: result }));
      setStatusTone("success");
      setStatusTitle("资源描述已更新");
      setStatusMessage(`已生成 ${path} 的视觉描述，并写回项目内的 JSON 索引。这个功能默认保持“隐形”，不额外打扰普通用户。`);
    } catch (error) {
      applyUnknownError(error, "生成资源描述失败");
    } finally {
      setIsCaptioningAsset(null);
    }
  };

  const handleGenerateScript = async () => {
    if (!generatePanel.targetFile.trim()) {
      applyError({ message: "请先选择 target_file" }, "AI 生成失败");
      return;
    }
    setIsGeneratingScript(true);
    setGeneratePanel((current) => ({ ...current, result: null }));
    setStatusTone("busy");
    setStatusTitle("正在调用 AI 生成剧本");
    setStatusMessage(`当前预览定位将作为后续“上下文优先修改”的基准。本轮 UI 先把 AI 工作台提升到最前面，生成结果仍会自动写回 ${generatePanel.targetFile}。`);
    setLastError(null);
    try {
      const result = await previewBridgeClient.generateScriptWithRetry(generatePanel.userPrompt, generatePanel.targetFile);
      setGeneratePanel((current) => ({ ...current, result }));
      if (result.applied) {
        await loadScriptContent(generatePanel.targetFile);
        await refreshRuntimeStatus();
        setStatusTone("success");
        setStatusTitle("AI 生成完成并已应用");
        setStatusMessage(`已在第 ${result.attempts} 次尝试后成功 reload。请先看游戏窗口验证效果；如果要做更底层修改，再展开开发者模块。`);
      } else {
        setStatusTone("error");
        setStatusTitle("AI 生成未通过引擎校验");
        setStatusMessage(`已尝试 ${result.attempts} 次，但最终 reload 仍失败。你可以在开发者模块里查看脚本，并继续手工修正。`);
        setLastError(result.lastError ?? null);
      }
    } catch (error) {
      applyUnknownError(error, "AI 生成失败");
    } finally {
      setIsGeneratingScript(false);
    }
  };

  return (
    <main className={`app-shell ${hasProject ? "app-shell-project" : "app-shell-empty"}`}>
      <button
        type="button"
        className={`settings-toggle ${isSettingsOpen ? "open" : ""}`}
        onClick={() => setIsSettingsOpen((current) => !current)}
      >
        {isSettingsOpen ? "关闭设置" : "设置"}
      </button>

      <aside className={`settings-drawer card ${isSettingsOpen ? "open" : ""}`}>
        <div className="panel-header">
          <div>
            <h2>设置</h2>
            <span>项目路径、本地 API 配置、VFX 备注路径都收在这里</span>
          </div>
        </div>
        <div className="settings-grid drawer-grid">
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
          <label>
            <span className="field-label">Zhipu API Key</span>
            <input
              type="password"
              value={config.zhipuApiKey}
              onChange={(event) => handleConfigChange("zhipuApiKey", event.currentTarget.value)}
              placeholder="仅在设置中维护"
            />
          </label>
          <label>
            <span className="field-label">DeepSeek API Key</span>
            <input
              type="password"
              value={config.deepseekApiKey}
              onChange={(event) => handleConfigChange("deepseekApiKey", event.currentTarget.value)}
              placeholder="仅在设置中维护"
            />
          </label>
          <label>
            <span className="field-label">VFX 备注文件路径</span>
            <input
              value={config.vfxNotesPath}
              onChange={(event) => handleConfigChange("vfxNotesPath", event.currentTarget.value)}
              placeholder="可留空，例如 E:/nova2/vfx-notes.md"
            />
          </label>
        </div>
        <div className="settings-actions drawer-actions">
          <button type="button" onClick={handleSaveConfig} disabled={isSavingConfig || isHydrating}>
            {isSavingConfig ? "保存中..." : "保存设置"}
          </button>
          {hasProject ? (
            <button type="button" className="secondary-button" onClick={handleLeaveProject} disabled={isLeavingProject}>
              {isLeavingProject ? "退出中..." : "退出当前项目"}
            </button>
          ) : null}
        </div>
      </aside>

      {!hasProject ? (
        <section className="entry-shell">
          <section className="entry-card card">
            <p className="eyebrow">VVN</p>
            <h1>进入一个 Nova2 项目</h1>
            <p className="subtitle">
              当前界面保持干净：这里只做项目入口。具体路径、API key、VFX 备注路径都收纳在右上角的“设置”里。
            </p>
            <div className="entry-meta">
              <div className="entry-meta-item">
                <span className="field-label">当前项目</span>
                <strong>{projectSession.projectDir || config.nova2ProjectDir || "尚未配置"}</strong>
              </div>
              <div className="entry-meta-item">
                <span className="field-label">后端状态</span>
                <div className="runtime-pills">
                  <span className={`runtime-pill ${runtimeStatus?.isGodotRunning ? "on" : "off"}`}>Godot {runtimeStatus?.isGodotRunning ? "Running" : "Idle"}</span>
                  <span className={`runtime-pill ${runtimeStatus?.isConnected ? "on" : "off"}`}>Bridge {runtimeStatus?.isConnected ? "Connected" : "Disconnected"}</span>
                </div>
              </div>
            </div>
            <div className="entry-actions">
              <button type="button" onClick={handleLoadProject} disabled={isLoadingProject || isHydrating}>
                {isLoadingProject ? "进入中..." : "进入项目"}
              </button>
              <button type="button" className="secondary-button" onClick={() => setIsSettingsOpen(true)}>
                打开设置
              </button>
            </div>
          </section>

          <section className={`card status-panel tone-${statusTone}`}>
            <div className="panel-header">
              <h2>状态面板</h2>
              <span>{statusTitle}</span>
            </div>
            <p className="status-message">{statusMessage}</p>
            {lastError ? (
              <p className="error-meta">
                line {lastError.line ?? "—"}, column {lastError.column ?? "—"}
              </p>
            ) : null}
          </section>
        </section>
      ) : (
        <section className="project-shell">
          <section className="hero-workbench card">
            <div className="hero-copy">
              <p className="eyebrow">AI Workbench</p>
              <h1>以自然语言驱动剧情与演出修改</h1>
              <p className="subtitle">
                这是默认工作台。优先通过自然语言描述目标效果，再由后端写回脚本并触发引擎 reload。开发者工具被折叠在下方，只有在需要深挖资源与脚本时才展开。
              </p>
            </div>
            <div className="project-summary">
              <div className="summary-item">
                <span className="field-label">当前项目</span>
                <strong>{projectSession.projectDir}</strong>
              </div>
              <div className="summary-item">
                <span className="field-label">当前预览定位</span>
                <strong>
                  node {formatStateValue(bridgeState.currentNodeRecordId)} / line {formatStateValue(bridgeState.currentDialogueIndex)}
                </strong>
              </div>
              <div className="summary-item">
                <span className="field-label">默认 target_file</span>
                <strong>{generatePanel.targetFile || "未选择"}</strong>
              </div>
            </div>
          </section>

          <section className="ai-first-grid">
            <section className="card ai-main-panel">
              <div className="panel-header">
                <div>
                  <h2>AI 生成</h2>
                  <span>主工作区：先自然语言，再决定是否展开开发者模块</span>
                </div>
              </div>
              <div className="context-banner">
                <strong>上下文策略（本轮 UI 预告）</strong>
                <p>
                  当用户提出“自然语言修改需求”时，后续会优先基于当前预览节点附近进行检查与改写；如果用户明确指出章节/对话/节点，再按显式定位优先。
                </p>
              </div>
              <div className="ai-form">
                <label>
                  <span className="field-label">自然语言需求</span>
                  <textarea
                    className="prompt-textarea hero-prompt"
                    value={generatePanel.userPrompt}
                    onChange={(event) => {
                      const value = event.currentTarget.value;
                      setGeneratePanel((current) => ({ ...current, userPrompt: value }));
                    }}
                    placeholder="例如：把当前预览附近的对话改得更克制，增加停顿感，并把背景切到夜晚教室。"
                  />
                </label>
                <div className="ai-inline-grid">
                  <label>
                    <span className="field-label">target_file</span>
                    <select
                      value={generatePanel.targetFile}
                      onChange={(event) => {
                        const value = event.currentTarget.value;
                        setGeneratePanel((current) => ({ ...current, targetFile: value }));
                      }}
                      disabled={scenarioFiles.length === 0}
                    >
                      <option value="">请选择目标剧本</option>
                      {scenarioFiles.map((file) => (
                        <option key={file.name} value={file.name}>
                          {file.name}
                        </option>
                      ))}
                    </select>
                  </label>
                  <button type="button" onClick={handleGenerateScript} disabled={isGeneratingScript || isLoadingProject || isHydrating || !generatePanel.userPrompt.trim()}>
                    {isGeneratingScript ? "生成中..." : "开始生成"}
                  </button>
                </div>
              </div>
              {generatePanel.result ? (
                <div className={`ai-result ${generatePanel.result.applied ? "success" : "error"}`}>
                  {generatePanel.result.summary ? (
                    <div className="result-line block result-summary">
                      <strong>本次修改摘要</strong>
                      <span>{generatePanel.result.summary}</span>
                    </div>
                  ) : generatePanel.result.applied ? (
                    <div className="result-line block result-summary pending">
                      <strong>本次修改摘要</strong>
                      <span>摘要生成中，稍后会自动补上…</span>
                    </div>
                  ) : null}
                  <div className="result-line">
                    <strong>尝试次数</strong>
                    <span>{generatePanel.result.attempts}</span>
                  </div>
                  <div className="result-line">
                    <strong>应用状态</strong>
                    <span>{generatePanel.result.applied ? "已自动应用到运行中引擎" : "未通过引擎 reload 校验"}</span>
                  </div>
                  <div className="result-line block">
                    <strong>最后错误</strong>
                    <span>{formatBridgeError(generatePanel.result.lastError)}</span>
                  </div>
                  {!generatePanel.result.applied ? (
                    <label>
                      <span className="field-label">最终脚本草稿（只读）</span>
                      <textarea readOnly value={generatePanel.result.finalScript} className="result-textarea" />
                    </label>
                  ) : (
                    <p className="result-note">生成结果已写回目标脚本。接下来建议先看游戏窗口中的真实效果，再决定是否展开开发者模块做细修。</p>
                  )}
                </div>
              ) : null}
            </section>

            <section className={`card status-panel tone-${statusTone}`}>
              <div className="panel-header">
                <h2>运行状态</h2>
                <span>{statusTitle}</span>
              </div>
              <dl className="state-grid compact-state-grid">
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
              <div className="runtime-pills status-pills">
                <span className={`runtime-pill ${runtimeStatus?.isGodotRunning ? "on" : "off"}`}>Godot {runtimeStatus?.isGodotRunning ? "Running" : "Idle"}</span>
                <span className={`runtime-pill ${runtimeStatus?.isConnected ? "on" : "off"}`}>Bridge {runtimeStatus?.isConnected ? "Connected" : "Disconnected"}</span>
                <span className="runtime-pill neutral">Port {config.previewBridgePort}</span>
              </div>
            </section>
          </section>

          <section className="developer-section card">
            <button type="button" className="developer-toggle" onClick={() => setIsDeveloperOpen((current) => !current)}>
              <span>开发者模块</span>
              <span>{isDeveloperOpen ? "收起" : "展开"}</span>
            </button>
            {isDeveloperOpen ? (
              <div className="developer-grid">
                <aside className="developer-sidebar">
                  <div className="panel-header nested-header">
                    <h2>Scenario 文件树</h2>
                    <span>{selectedScript ?? "未选择"}</span>
                  </div>
                  <div className="tree-list" role="tree" aria-label="Script file tree">
                    {scenarioFiles.length === 0 ? (
                      <p className="editor-hint">进入项目后会列出 resources/scenarios/*.txt。新建/删除留到下一轮开发者模块增强。</p>
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

                  <div className="sidebar-divider" />

                  <div className="panel-header nested-header">
                    <h2>图片资源</h2>
                    <span>{assetFiles.length} 项</span>
                  </div>
                  <div className="tree-list asset-list" role="tree" aria-label="Asset file tree">
                    {assetFiles.length === 0 ? (
                      <p className="editor-hint">资源描述索引已经有后端骨架。本轮还未加入上传/删除与音乐/视频资源管理。</p>
                    ) : (
                      assetGroups.map(([groupName, assets]) => (
                        <div key={groupName} className="tree-group">
                          <div className="tree-group-title">{groupName}</div>
                          {assets.map((asset) => {
                            const isActive = asset.path === selectedAsset;
                            const assetParts = asset.path.split("/");
                            const fileName = assetParts[assetParts.length - 1] ?? asset.path;
                            return (
                              <div key={asset.path} className={`asset-row ${isActive ? "active" : ""}`}>
                                <button
                                  type="button"
                                  className={`tree-node selectable asset-node ${isActive ? "active" : ""}`}
                                  onClick={() => setSelectedAsset(asset.path)}
                                >
                                  <span className="tree-node-icon">◦</span>
                                  <span className="asset-name">{fileName}</span>
                                </button>
                                <button
                                  type="button"
                                  className="secondary-button compact-button"
                                  onClick={() => handleCaptionAsset(asset.path)}
                                  disabled={isCaptioningAsset === asset.path || isLoadingProject || isHydrating}
                                >
                                  {isCaptioningAsset === asset.path ? "生成中..." : "生成描述"}
                                </button>
                              </div>
                            );
                          })}
                        </div>
                      ))
                    )}
                  </div>
                </aside>

                <section className="developer-main">
                  <div className="card editor-panel inner-card">
                    <div className="panel-header">
                      <div>
                        <h2>NovaScript 编辑区</h2>
                        <span>{selectedScript ?? "未选择"}</span>
                      </div>
                      <div className="editor-actions">
                        <button
                          type="button"
                          className="secondary-button compact-button"
                          onClick={handleToggleVersionHistory}
                          disabled={!selectedScript || isLoadingProject || isLoadingScript || isHydrating}
                        >
                          {isVersionPanelOpen ? "收起版本历史" : "版本历史"}
                        </button>
                        <button type="button" onClick={handleSave} disabled={!selectedScript || isSaving || isLoadingProject || isLoadingScript || isHydrating}>
                          {isSaving ? "保存中..." : "保存并刷新"}
                        </button>
                      </div>
                    </div>
                    <p className="editor-hint">这是面向开发者的底层编辑区。普通使用路径应先走上面的自然语言工作台。每次保存或 AI 生成写回前都会自动存一份历史快照，可在“版本历史”里回退。</p>
                    {isVersionPanelOpen ? (
                      <div className="version-history-panel">
                        {isLoadingVersions ? (
                          <p className="editor-hint">正在读取版本历史...</p>
                        ) : versionList.length === 0 ? (
                          <p className="editor-hint">还没有历史快照——只有在发生过保存/AI 写回之后才会出现可回退的版本。</p>
                        ) : (
                          <>
                            <input
                              type="text"
                              className="version-search"
                              value={versionSearch}
                              onChange={(event) => setVersionSearch(event.currentTarget.value)}
                              placeholder="按修改摘要或内容预览搜索历史版本..."
                            />
                            {filteredVersionList.length === 0 ? (
                              <p className="editor-hint">没有匹配“{versionSearch}”的历史版本。</p>
                            ) : (
                              <ul className="version-list">
                                {filteredVersionList.map((version) => (
                                  <li key={version.id} className="version-row">
                                    <div className="version-meta">
                                      <strong>{formatTimestamp(version.timestampMs)}</strong>
                                      <span className="version-preview">{version.summary || version.preview || "(空)"}</span>
                                    </div>
                                    <button
                                      type="button"
                                      className="secondary-button compact-button"
                                      onClick={() => handleRestoreVersion(version.id)}
                                      disabled={restoringVersionId === version.id}
                                    >
                                      {restoringVersionId === version.id ? "恢复中..." : "恢复此版本"}
                                    </button>
                                  </li>
                                ))}
                              </ul>
                            )}
                          </>
                        )}
                      </div>
                    ) : null}
                    <textarea
                      value={editorValue}
                      onChange={(event) => handleEditorChange(event.currentTarget.value)}
                      disabled={!selectedScript || isLoadingScript}
                      spellCheck={false}
                      aria-label="NovaScript editor"
                    />
                  </div>

                  <div className="developer-bottom-grid">
                    <section className="card caption-panel inner-card">
                      <div className="panel-header">
                        <h2>资源描述</h2>
                        <span>{selectedAsset ?? "未选择图片资源"}</span>
                      </div>
                      <p className="editor-hint">本轮已经把描述写回项目内 JSON 索引；前端这里仍保留一个可见面板，方便开发者抽查结果。</p>
                      <label>
                        <span className="field-label">描述内容</span>
                        <textarea
                          className="caption-textarea"
                          value={selectedCaption}
                          onChange={(event) => handleCaptionChange(event.currentTarget.value)}
                          disabled={!selectedAsset}
                          placeholder="生成结果会显示在这里。"
                        />
                      </label>
                    </section>

                    <section className="card seek-panel inner-card">
                      <div className="panel-header">
                        <h2>Seek 控件</h2>
                        <span>映射到 `seek(nodeRecordId, dialogueIndex)`</span>
                      </div>
                      <div className="seek-grid">
                        <label>
                          <span className="field-label">Node Record Id</span>
                          <input
                            value={seekDraft.nodeRecordId}
                            onChange={(event) => {
                              const value = event.currentTarget.value;
                              setSeekDraft((current) => ({ ...current, nodeRecordId: value }));
                            }}
                            inputMode="numeric"
                          />
                        </label>
                        <label>
                          <span className="field-label">Dialogue Index</span>
                          <input
                            value={seekDraft.dialogueIndex}
                            onChange={(event) => {
                              const value = event.currentTarget.value;
                              setSeekDraft((current) => ({ ...current, dialogueIndex: value }));
                            }}
                            inputMode="numeric"
                          />
                        </label>
                        <button type="button" onClick={handleSeek} disabled={isSeeking || isLoadingProject || isHydrating}>
                          {isSeeking ? "跳转中..." : "执行 Seek"}
                        </button>
                      </div>
                    </section>
                  </div>
                </section>
              </div>
            ) : null}
          </section>
        </section>
      )}
    </main>
  );
}

export default App;
