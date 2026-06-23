import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { PreviewBridgeError, PreviewBridgeState, ScenarioFile, StateChangedEvent } from "./protocol";

type Listener = (event: StateChangedEvent) => void;
type BridgeResult = { ok: true; state?: PreviewBridgeState } | { ok: false; error: PreviewBridgeError };
type LoadProjectResult = { ok: true; state: PreviewBridgeState };

const STATE_CHANGED_EVENT = "preview_bridge://state_changed";

export type AppConfig = {
  nova2ProjectDir: string;
  godotExecutablePath: string;
  previewBridgePort: number;
};

export type RuntimeStatus = {
  isConnected: boolean;
  isGodotRunning: boolean;
  config: AppConfig;
  state?: PreviewBridgeState;
};

export class PreviewBridgeClient {
  private listeners = new Set<Listener>();
  private unlistenPromise: Promise<UnlistenFn> | null = null;

  async getAppConfig(): Promise<AppConfig> {
    return invoke<AppConfig>("get_app_config");
  }

  async saveAppConfig(config: AppConfig): Promise<AppConfig> {
    return invoke<AppConfig>("save_app_config", { config });
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

  async readScenarioFile(name: string): Promise<string> {
    return invoke<string>("read_scenario_file", { name });
  }

  async writeScenarioFile(name: string, content: string): Promise<void> {
    await invoke("write_scenario_file", { name, content });
  }

  onStateChanged(listener: Listener) {
    this.listeners.add(listener);
    void this.ensureEventBridge();
    return () => {
      this.listeners.delete(listener);
    };
  }

  private emitStateChanged(event: StateChangedEvent) {
    this.listeners.forEach((listener) => listener(event));
  }

  private async ensureEventBridge() {
    if (!this.unlistenPromise) {
      this.unlistenPromise = listen<StateChangedEvent>(STATE_CHANGED_EVENT, (event) => {
        this.emitStateChanged(event.payload);
      });
    }
    await this.unlistenPromise;
  }
}

export const previewBridgeClient = new PreviewBridgeClient();
