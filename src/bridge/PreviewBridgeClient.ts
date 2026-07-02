import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  AssetFile,
  GenerateResult,
  GenerationStatusEvent,
  PreviewBridgeError,
  PreviewBridgeState,
  ProjectSession,
  RegularizeScriptResult,
  ScenarioFile,
  StateChangedEvent,
  SummaryReadyEvent,
  VersionInfo,
} from "./protocol";

type Listener = (event: StateChangedEvent) => void;
type SummaryListener = (event: SummaryReadyEvent) => void;
type GenerationStatusListener = (event: GenerationStatusEvent) => void;
type BridgeResult = { ok: true; state?: PreviewBridgeState } | { ok: false; error: PreviewBridgeError };
type LoadProjectResult = { ok: true; state: PreviewBridgeState };

const STATE_CHANGED_EVENT = "preview_bridge://state_changed";
const SUMMARY_READY_EVENT = "preview_bridge://generation_summary_ready";
const GENERATION_STATUS_EVENT = "preview_bridge://generation_status";

export type AppConfig = {
  nova2ProjectDir: string;
  godotExecutablePath: string;
  previewBridgePort: number;
  zhipuApiKey: string;
  deepseekApiKey: string;
  vfxNotesPath: string;
};

export type RuntimeStatus = {
  isConnected: boolean;
  isGodotRunning: boolean;
  config: AppConfig;
  state?: PreviewBridgeState;
};

export class PreviewBridgeClient {
  private listeners = new Set<Listener>();
  private unlistenPromise: Promise<UnlistenFn | null> | null = null;
  private summaryListeners = new Set<SummaryListener>();
  private summaryUnlistenPromise: Promise<UnlistenFn | null> | null = null;
  private generationStatusListeners = new Set<GenerationStatusListener>();
  private generationStatusUnlistenPromise: Promise<UnlistenFn | null> | null = null;

  async getAppConfig(): Promise<AppConfig> {
    return invoke<AppConfig>("get_app_config");
  }

  async saveAppConfig(config: AppConfig): Promise<AppConfig> {
    return invoke<AppConfig>("save_app_config", { config });
  }

  async getProjectSession(): Promise<ProjectSession> {
    return invoke<ProjectSession>("get_project_session");
  }

  async leaveProject(): Promise<ProjectSession> {
    return invoke<ProjectSession>("leave_project");
  }

  async getRuntimeStatus(): Promise<RuntimeStatus> {
    return invoke<RuntimeStatus>("get_runtime_status");
  }

  async loadProject(): Promise<LoadProjectResult> {
    await this.ensureEventBridge();
    const result = await invoke<LoadProjectResult>("load_project");
    this.emitStateChanged({ method: "state_changed", ok: true, state: result.state });
    return result;
  }

  async reloadPreview(): Promise<BridgeResult> {
    await this.ensureEventBridge();
    const result = await invoke<BridgeResult>("reload_preview");
    if (result.ok && result.state) {
      this.emitStateChanged({ method: "state_changed", ok: true, state: result.state });
    }
    return result;
  }

  async seek(nodeRecordId: number, dialogueIndex: number): Promise<BridgeResult> {
    await this.ensureEventBridge();
    const result = await invoke<BridgeResult>("seek", { nodeRecordId, dialogueIndex });
    if (result.ok && result.state) {
      this.emitStateChanged({ method: "state_changed", ok: true, state: result.state });
    }
    return result;
  }

  async listScenarioFiles(): Promise<ScenarioFile[]> {
    return invoke<ScenarioFile[]>("list_scenario_files");
  }

  async listAssetFiles(): Promise<AssetFile[]> {
    return invoke<AssetFile[]>("list_asset_files");
  }

  async readScenarioFile(name: string): Promise<string> {
    return invoke<string>("read_scenario_file", { name });
  }

  async writeScenarioFile(name: string, content: string): Promise<void> {
    await invoke("write_scenario_file", { name, content });
  }

  async captionAsset(path: string): Promise<string> {
    return invoke<string>("caption_asset_cmd", { path });
  }

  async generateScriptWithRetry(userPrompt: string, targetFile: string): Promise<GenerateResult> {
    return invoke<GenerateResult>("generate_script_with_retry", {
      request: { userPrompt, targetFile },
    });
  }

  async incrementalTweak(userPrompt: string, targetFile: string): Promise<GenerateResult> {
    return invoke<GenerateResult>("incremental_tweak", {
      request: { userPrompt, targetFile },
    });
  }

  // AUTOSTAGE reuses the same GenerateRequest shape as the other generation modes - dialogueOnlyText
  // travels in the userPrompt field (see autostage_inner in lib.rs), since the backend's
  // GenerateRequest has no dedicated field for it and adding one would mean a second request type
  // for what's otherwise an identical { userPrompt, targetFile } shape.
  async autostage(dialogueOnlyText: string, targetFile: string, labelName?: string): Promise<GenerateResult> {
    return invoke<GenerateResult>("autostage", {
      request: { userPrompt: dialogueOnlyText, targetFile, labelName: labelName || null },
    });
  }

  // Free-form entry point: turns whatever the author actually wrote into marker-language text
  // (one LLM call, with structural-error retry handled entirely on the Rust side) and returns a
  // human-readable confirmation view alongside the underlying marker text. The marker text is
  // never shown to the author directly - once they confirm the view, pass `markerText` straight
  // into `autostage()` above as if they'd hand-written the markers themselves.
  async regularizeScript(freeScript: string, targetFile: string): Promise<RegularizeScriptResult> {
    return invoke<RegularizeScriptResult>("regularize_script", {
      request: { freeScript, targetFile },
    });
  }

  async listScriptVersions(name: string): Promise<VersionInfo[]> {
    return invoke<VersionInfo[]>("list_script_versions", { name });
  }

  async restoreScriptVersion(name: string, versionId: string): Promise<string> {
    return invoke<string>("restore_script_version", { name, versionId });
  }

  onStateChanged(listener: Listener) {
    this.listeners.add(listener);
    void this.ensureEventBridge();
    return () => {
      this.listeners.delete(listener);
    };
  }

  // Generation's "write a changelog summary + snapshot version history" step now runs in the
  // background after generate_script_with_retry already returned (see lib.rs) - this is how the
  // summary text and an up-to-date version-history entry arrive once that background work finishes,
  // without the AI-generation call itself having to wait for it.
  onSummaryReady(listener: SummaryListener) {
    this.summaryListeners.add(listener);
    void this.ensureSummaryEventBridge();
    return () => {
      this.summaryListeners.delete(listener);
    };
  }

  onGenerationStatus(listener: GenerationStatusListener) {
    this.generationStatusListeners.add(listener);
    void this.ensureGenerationStatusEventBridge();
    return () => {
      this.generationStatusListeners.delete(listener);
    };
  }

  private emitStateChanged(event: StateChangedEvent) {
    this.listeners.forEach((listener) => listener(event));
  }

  private async ensureEventBridge() {
    if (!this.isTauriRuntime()) {
      return;
    }
    if (!this.unlistenPromise) {
      this.unlistenPromise = listen<StateChangedEvent>(STATE_CHANGED_EVENT, (event) => {
        this.emitStateChanged(event.payload);
      }).then((unlisten) => unlisten);
    }
    await this.unlistenPromise;
  }

  private async ensureSummaryEventBridge() {
    if (!this.isTauriRuntime()) {
      return;
    }
    if (!this.summaryUnlistenPromise) {
      this.summaryUnlistenPromise = listen<SummaryReadyEvent>(SUMMARY_READY_EVENT, (event) => {
        this.summaryListeners.forEach((listener) => listener(event.payload));
      }).then((unlisten) => unlisten);
    }
    await this.summaryUnlistenPromise;
  }

  private async ensureGenerationStatusEventBridge() {
    if (!this.isTauriRuntime()) {
      return;
    }
    if (!this.generationStatusUnlistenPromise) {
      this.generationStatusUnlistenPromise = listen<GenerationStatusEvent>(GENERATION_STATUS_EVENT, (event) => {
        this.generationStatusListeners.forEach((listener) => listener(event.payload));
      }).then((unlisten) => unlisten);
    }
    await this.generationStatusUnlistenPromise;
  }

  private isTauriRuntime() {
    return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
  }
}

export const previewBridgeClient = new PreviewBridgeClient();
