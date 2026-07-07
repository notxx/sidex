import { invoke } from '@tauri-apps/api/core';

// ---------------------------------------------------------------------------
// Platform bootstrap / lifecycle
// ---------------------------------------------------------------------------

export interface IExtensionTransportInfo {
  kind: string;
  endpoint: string;
}

export interface INodeRuntimeInfo {
  path: string;
  version?: string;
  source: string;
  bundled: boolean;
}

export interface IExtensionPathsInfo {
  serverScript: string;
  builtinExtensionsDir: string;
  userExtensionsDir: string;
  globalStorageDir: string;
}

export interface IExtensionManifestSummary {
  id: string;
  name: string;
  version: string;
  kind: 'node' | 'wasm';
  activationEvents: string[];
  main?: string;
  browser?: string;
  wasmBinary?: string;
  contributes: string[];
  location: string;
}

export interface IExtensionPlatformBootstrap {
  transport: IExtensionTransportInfo;
  runtime: INodeRuntimeInfo;
  paths: IExtensionPathsInfo;
  sessionKind: string;
  extensions: IExtensionManifestSummary[];
  initDataJson: string;
  nodeAvailable?: boolean;
  nodeError?: string | null;
}

export interface IExtensionPlatformStatus {
  running: boolean;
  port?: number;
  sessionId?: string;
  uptimeSecs?: number;
  extensionCount?: number;
  restartCount?: number;
  totalCrashes: number;
}

export async function bootstrapExtensionPlatform(): Promise<IExtensionPlatformBootstrap> {
  return invoke<IExtensionPlatformBootstrap>('extension_platform_bootstrap');
}

export async function getExtensionPlatformStatus(): Promise<IExtensionPlatformStatus> {
  return invoke<IExtensionPlatformStatus>('extension_platform_status');
}

export async function restartExtensionPlatform(): Promise<IExtensionPlatformStatus> {
  return invoke<IExtensionPlatformStatus>('extension_platform_restart');
}

export async function stopExtensionPlatform(): Promise<void> {
  return invoke<void>('extension_platform_stop');
}

export async function getExtensionPlatformInitData(): Promise<string> {
  return invoke<string>('extension_platform_init_data');
}

// ---------------------------------------------------------------------------
// Runtime diagnostics — extension activation tracking
// ---------------------------------------------------------------------------

export type ExtensionStatus =
  | 'discovered'
  | 'loading'
  | 'activated'
  | 'failed'
  | 'deactivated'
  | 'disabled';

export interface IExtensionRuntimeRecord {
  id: string;
  status: ExtensionStatus;
  activationTimeMs?: number;
  activatedAt?: string;
  deactivatedAt?: string;
  error?: string;
  errorCount: number;
  isSlow: boolean;
  disabledByBisect: boolean;
  providerCount: number;
  commandCount: number;
}

export interface IExtensionProfileRecord {
  id: string;
  activationTimeMs: number;
  isSlow: boolean;
  totalProviderCalls: number;
  totalProviderTimeMs: number;
  avgProviderTimeMs: number;
  peakProviderTimeMs: number;
  errorCount: number;
}

export interface IExtensionStartupSummary {
  startupComplete: boolean;
  startupTimeMs?: number;
  totalExtensions: number;
  activatedCount: number;
  failedCount: number;
  slowCount: number;
  totalActivationTimeMs: number;
  slowestExtension?: string;
  slowestActivationMs: number;
}

export interface IExtensionActivationReport {
  extensionId: string;
  activationTimeMs: number;
  error?: string;
  providerCount?: number;
  commandCount?: number;
}

export async function reportExtensionActivated(report: IExtensionActivationReport): Promise<void> {
  return invoke<void>('extension_report_activated', { report });
}

export async function reportExtensionProviderCall(extensionId: string, durationMs: number): Promise<void> {
  return invoke<void>('extension_report_provider_call', { extensionId, durationMs });
}

export async function reportExtensionDeactivated(extensionId: string): Promise<void> {
  return invoke<void>('extension_report_deactivated', { extensionId });
}

export async function reportExtensionError(extensionId: string, error: string): Promise<void> {
  return invoke<void>('extension_report_error', { extensionId, error });
}

export async function markStartupComplete(): Promise<number> {
  return invoke<number>('extension_mark_startup_complete');
}

export async function registerExtensionSession(extensionIds: string[]): Promise<void> {
  return invoke<void>('extension_register_session', { extensionIds });
}

export async function getExtensionRuntimeStatus(): Promise<IExtensionRuntimeRecord[]> {
  return invoke<IExtensionRuntimeRecord[]>('extension_runtime_status');
}

export async function getExtensionRuntimeProfile(): Promise<IExtensionProfileRecord[]> {
  return invoke<IExtensionProfileRecord[]>('extension_runtime_profile');
}

export async function getSlowExtensions(): Promise<IExtensionProfileRecord[]> {
  return invoke<IExtensionProfileRecord[]>('extension_slow_extensions');
}

export async function getExtensionStartupSummary(): Promise<IExtensionStartupSummary> {
  return invoke<IExtensionStartupSummary>('extension_startup_summary');
}

// ---------------------------------------------------------------------------
// Extension bisect — binary search for problematic extensions
// ---------------------------------------------------------------------------

export interface IBisectState {
  active: boolean;
  round: number;
  totalRounds: number;
  allExtensionIds: string[];
  enabledIds: string[];
  disabledIds: string[];
  confirmedBad: string[];
  confirmedGood: string[];
}

export async function startExtensionBisect(): Promise<IBisectState> {
  return invoke<IBisectState>('extension_bisect_start');
}

export async function bisectReportGood(): Promise<IBisectState> {
  return invoke<IBisectState>('extension_bisect_good');
}

export async function bisectReportBad(): Promise<IBisectState> {
  return invoke<IBisectState>('extension_bisect_bad');
}

export async function resetExtensionBisect(): Promise<void> {
  return invoke<void>('extension_bisect_reset');
}

export async function getExtensionBisectState(): Promise<IBisectState> {
  return invoke<IBisectState>('extension_bisect_state');
}

// ---------------------------------------------------------------------------
// WASM extension management
// ---------------------------------------------------------------------------

export async function loadWasmExtension(extensionId: string, wasmPath: string): Promise<void> {
  return invoke<void>('wasm_load_extension', { extensionId, wasmPath });
}

export async function unloadWasmExtension(extensionId: string): Promise<void> {
  return invoke<void>('wasm_unload_extension', { extensionId });
}

export async function listWasmExtensions(): Promise<string[]> {
  return invoke<string[]>('wasm_list_extensions');
}

// ---------------------------------------------------------------------------
// WASM document sync — keep WASM host state in sync with open editors
// ---------------------------------------------------------------------------

export async function wasmSyncDocument(uri: string, languageId: string, text: string): Promise<void> {
  return invoke<void>('wasm_sync_document', { uri, languageId, text });
}

export async function wasmCloseDocument(uri: string): Promise<void> {
  return invoke<void>('wasm_close_document', { uri });
}

export async function wasmSyncWorkspaceFolders(folders: string[]): Promise<void> {
  return invoke<void>('wasm_sync_workspace_folders', { folders });
}

// ---------------------------------------------------------------------------
// WASM provider calls — direct Rust invocation, no WebSocket round-trip
// ---------------------------------------------------------------------------

export interface IWasmProviderParams {
  extensionId: string;
  uri: string;
  languageId: string;
  version: number;
  line: number;
  character: number;
}

export async function wasmProvideCompletion(params: IWasmProviderParams): Promise<any> {
  return invoke<any>('wasm_provide_completion', { params });
}

export async function wasmProvideHover(params: IWasmProviderParams): Promise<any> {
  return invoke<any>('wasm_provide_hover', { params });
}

export async function wasmProvideDefinition(params: IWasmProviderParams): Promise<any> {
  return invoke<any>('wasm_provide_definition', { params });
}

export async function wasmProvideReferences(params: IWasmProviderParams): Promise<any> {
  return invoke<any>('wasm_provide_references', { params });
}

export async function wasmProvideDocumentSymbols(params: IWasmProviderParams): Promise<any> {
  return invoke<any>('wasm_provide_document_symbols', { params });
}

export async function wasmProvideFormatting(
  params: IWasmProviderParams,
  tabSize: number,
  insertSpaces: boolean,
): Promise<any> {
  return invoke<any>('wasm_provide_formatting', { params, tabSize, insertSpaces });
}

// ---------------------------------------------------------------------------
// WASM broadcast providers — fan out to all loaded WASM extensions
// ---------------------------------------------------------------------------

export async function wasmProvideCompletionAll(
  uri: string, languageId: string, version: number, line: number, character: number,
): Promise<any> {
  return invoke<any>('wasm_provide_completion_all', { uri, languageId, version, line, character });
}

export async function wasmProvideHoverAll(
  uri: string, languageId: string, version: number, line: number, character: number,
): Promise<any> {
  return invoke<any>('wasm_provide_hover_all', { uri, languageId, version, line, character });
}

export async function wasmProvideDefinitionAll(
  uri: string, languageId: string, version: number, line: number, character: number,
): Promise<any> {
  return invoke<any>('wasm_provide_definition_all', { uri, languageId, version, line, character });
}

export async function wasmProvideDocumentSymbolsAll(
  uri: string, languageId: string, version: number,
): Promise<any> {
  return invoke<any>('wasm_provide_document_symbols_all', { uri, languageId, version });
}

export async function wasmProvideFormattingAll(
  uri: string, languageId: string, version: number, tabSize: number, insertSpaces: boolean,
): Promise<any> {
  return invoke<any>('wasm_provide_formatting_all', { uri, languageId, version, tabSize, insertSpaces });
}

// ---------------------------------------------------------------------------
// WASM extension metadata & tree view
// ---------------------------------------------------------------------------

export async function wasmGetExtensionMetadata(extensionId: string): Promise<any> {
  return invoke<any>('wasm_get_extension_metadata', { extensionId });
}

export async function wasmGetTreeChildren(extensionId: string, viewId: string, elementId: string | null): Promise<any[]> {
  return invoke<any[]>('wasm_get_tree_children', { extensionId, viewId, elementId });
}

export async function wasmGetTreeItem(extensionId: string, viewId: string, elementId: string): Promise<any> {
  return invoke<any>('wasm_get_tree_item', { extensionId, viewId, elementId });
}

export async function wasmOnTreeItemActivated(extensionId: string, viewId: string, elementId: string): Promise<void> {
  return invoke<void>('wasm_on_tree_item_activated', { extensionId, viewId, elementId });
}

export async function wasmOnTreeVisibilityChanged(extensionId: string, viewId: string, visible: boolean): Promise<void> {
  return invoke<void>('wasm_on_tree_visibility_changed', { extensionId, viewId, visible });
}

export async function wasmExecuteCommandAll(commandId: string, args: string): Promise<any> {
  return invoke<any>('wasm_execute_command_all', { commandId, args });
}
