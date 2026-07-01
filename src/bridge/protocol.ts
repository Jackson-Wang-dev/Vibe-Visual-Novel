export type PreviewBridgeError = {
  message: string;
  line?: number;
  column?: number;
};

export type PreviewBridgeState = {
  currentNodeRecordId: number | null;
  currentDialogueIndex: number | null;
  startNodeNames: string[];
};

export type ReloadRequest = { id: number; method: "reload" };
export type SeekRequest = { id: number; method: "seek"; params: { nodeRecordId: number; dialogueIndex: number } };
export type GetStateRequest = { id: number; method: "get_state" };
export type BridgeRequest = ReloadRequest | SeekRequest | GetStateRequest;

export type ReloadResponse = { id: number; ok: true };
export type SeekResponse = { id: number; ok: true };
export type GetStateResponse = { id: number; ok: true; state: PreviewBridgeState };
export type BridgeErrorResponse = { id: number; ok: false; error: PreviewBridgeError };
export type BridgeResponse = ReloadResponse | SeekResponse | GetStateResponse | BridgeErrorResponse;

export type StateChangedEvent = { method: "state_changed"; ok: true; state: PreviewBridgeState };

export type ScenarioFile = { name: string };
export type AssetFile = { path: string };
export type ProjectSession = {
  hasProject: boolean;
  projectDir: string;
};
export type GenerateResult = {
  finalScript: string;
  attempts: number;
  applied: boolean;
  lastError?: PreviewBridgeError;
  summary: string;
};
export type ConfirmationBeatView = {
  text: string;
  speaker?: string;
  background?: string;
  onStage: string[];
  hint?: string;
};
export type ConfirmationNodeView = {
  name?: string;
  beats: ConfirmationBeatView[];
  hasHandwrittenContent: boolean;
  terminatorSummary: string;
};
export type ConfirmationView = {
  nodes: ConfirmationNodeView[];
  newCharacters: string[];
  unresolvedResources: string[];
};
export type RegularizeScriptResult = {
  markerText: string;
  confirmation: ConfirmationView;
};
export type VersionInfo = {
  id: string;
  timestampMs: number;
  preview: string;
  summary: string;
};
export type SummaryReadyEvent = {
  targetFile: string;
  summary: string;
};

export type GenerationStatusEvent = {
  phase: string;
  label: string;
  detail: string;
  targetFile: string;
  attempt?: number;
};
