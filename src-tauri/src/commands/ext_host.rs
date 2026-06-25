use crate::commands::extension_platform::{
    build_extension_descriptions, build_init_data, extension_search_paths, global_storage_dir,
    resolve_builtin_extensions_dir, resolve_node_runtime, resolve_server_script, scan_extensions,
    user_extensions_dir, ExtensionHostInitData, ExtensionKind, ExtensionManifest, NodeRuntimeInfo,
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::AppHandle;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

fn normalize_for_node(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        dunce::simplified(&path).to_path_buf()
    }
    #[cfg(not(windows))]
    {
        path
    }
}

// State model

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum ExtHostLifecycleState {
    Idle,
    Bootstrapping,
    ServerStarting,
    ServerListening,
    WsConnecting,
    WsConnected,
    HostStarting,
    HostReady,
    ActivatingExtensions,
    Ready,
    NodeMissing,
    ServerScriptMissing,
    ServerStartFailed,
    PortTimeout,
    WsConnectFailed,
    WsClosed,
    HostReadyTimeout,
    HostExited,
    ActivationTimeout,
    ActivationFailed,
    Degraded,
}

impl std::fmt::Display for ExtHostLifecycleState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

// Session types

struct ExtHostSession {
    child: Child,
    port: u16,
    session_id: String,
    started_at: Instant,
    #[allow(dead_code)]
    init_data: ExtensionHostInitData,
    #[allow(dead_code)]
    manifests: Vec<ExtensionManifest>,
    recent_stderr: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionPlatformRuntimeState {
    pub running: bool,
    pub port: Option<u16>,
    pub session_id: Option<String>,
    pub uptime_secs: Option<u64>,
    pub restart_count: u32,
    pub total_crashes: u32,
    pub lifecycle_state: ExtHostLifecycleState,
    pub crash_loop_detected: bool,
}

pub struct ExtensionPlatformSupervisor {
    inner: Mutex<SupervisorState>,
}

struct CrashRecord {
    timestamp: Instant,
    session_id: String,
}

struct SupervisorState {
    session: Option<ExtHostSession>,
    total_crashes: u32,
    restart_count: u32,
    lifecycle_state: ExtHostLifecycleState,
    crash_history: VecDeque<CrashRecord>,
    crash_loop_detected: bool,
    degraded_at: Option<Instant>,
}

impl ExtensionPlatformSupervisor {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(SupervisorState {
                session: None,
                total_crashes: 0,
                restart_count: 0,
                lifecycle_state: ExtHostLifecycleState::Idle,
                crash_history: VecDeque::with_capacity(10),
                crash_loop_detected: false,
                degraded_at: None,
            }),
        }
    }

    pub fn set_lifecycle_state(&self, state: ExtHostLifecycleState) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.lifecycle_state = state.clone();
            log::info!("[ext-host/status] state={state}");
        }
    }

    pub fn ensure_started(
        &self,
        app: &AppHandle,
        _init_data_json: &str,
        _extension_search_paths: &[String],
    ) -> Result<u16, String> {
        let mut guard = self.inner.lock().map_err(|e| e.to_string())?;

        // Crash loop detection
        if guard.crash_loop_detected {
            if let Some(degraded_at) = guard.degraded_at {
                if degraded_at.elapsed() > Duration::from_secs(30) {
                    log::info!("[ext-host] crash loop cooldown elapsed, allowing restart");
                    guard.crash_loop_detected = false;
                    guard.degraded_at = None;
                    guard.crash_history.clear();
                } else {
                    return Err("Extension host is in Degraded state due to repeated crashes. \
                                 Use extension_platform_restart to manually restart."
                        .into());
                }
            }
        }

        // Check existing session
        if let Some(ref mut session) = guard.session {
            if session.child.try_wait().ok().flatten().is_none() {
                return Ok(session.port);
            }
            Self::check_child_exit_locked(&mut guard);
            if guard.crash_loop_detected {
                return Err("Extension host crash loop detected. State set to Degraded. \
                             Please restart manually via extension_platform_restart."
                    .into());
            }
        }

        guard.lifecycle_state = ExtHostLifecycleState::ServerStarting;
        let started = match spawn_host_process(app, &[]) {
            Ok(s) => s,
            Err(e) => {
                if e.starts_with("ERR_NODE_MISSING:") {
                    guard.lifecycle_state = ExtHostLifecycleState::NodeMissing;
                } else if e.starts_with("ERR_SERVER_SCRIPT_MISSING:") {
                    guard.lifecycle_state = ExtHostLifecycleState::ServerScriptMissing;
                } else if e.starts_with("ERR_SPAWN_FAILED:") {
                    guard.lifecycle_state = ExtHostLifecycleState::ServerStartFailed;
                } else if e.starts_with("ERR_PORT_TIMEOUT:") {
                    guard.lifecycle_state = ExtHostLifecycleState::PortTimeout;
                } else {
                    guard.lifecycle_state = ExtHostLifecycleState::ServerStartFailed;
                }
                return Err(e);
            }
        };
        let port = started.port;
        let session_id = started.session_id.clone();

        log::info!(
            "[ext-host/bootstrap] session={} port={} node={} server_script={}",
            session_id,
            port,
            started.node_runtime.path,
            started.server_script_path.display()
        );

        guard.restart_count += 1;
        guard.session = Some(ExtHostSession {
            child: started.child,
            port: started.port,
            session_id: started.session_id,
            started_at: Instant::now(),
            init_data: started.init_data,
            manifests: started.manifests,
            recent_stderr: Vec::new(),
        });
        guard.lifecycle_state = ExtHostLifecycleState::ServerListening;
        Ok(port)
    }

    /// Update lifecycle state only if session_id matches the current session.
    /// Prevents stale events (e.g. ws_closed from old socket) from overwriting
    /// a newer session's state. Empty session_id is rejected to avoid writing
    /// state from early startup events that lack session context.
    fn set_state_if_session_match(
        guard: &SupervisorState,
        state: ExtHostLifecycleState,
        session_id: &str,
    ) -> Option<ExtHostLifecycleState> {
        if session_id.is_empty() {
            log::warn!(
                "[ext-host/status] ignoring state={state} with empty session_id"
            );
            return None;
        }
        match &guard.session {
            Some(session) if session.session_id == session_id => Some(state),
            _ => {
                log::warn!(
                    "[ext-host/status] ignoring state={state} for stale session={session_id} (current={})",
                    guard.session.as_ref().map(|s| s.session_id.as_str()).unwrap_or("none")
                );
                None
            }
        }
    }

    pub fn on_ws_connected(&self, session_id: &str) {
        if let Ok(mut guard) = self.inner.lock() {
            if let Some(s) = Self::set_state_if_session_match(&guard, ExtHostLifecycleState::WsConnected, session_id) {
                guard.lifecycle_state = s;
            }
        }
    }

    pub fn on_host_ready(&self, session_id: &str) {
        if let Ok(mut guard) = self.inner.lock() {
            if let Some(s) = Self::set_state_if_session_match(&guard, ExtHostLifecycleState::HostReady, session_id) {
                guard.lifecycle_state = s;
            }
        }
    }

    pub fn on_ws_closed(&self, session_id: &str) {
        if let Ok(mut guard) = self.inner.lock() {
            if let Some(s) = Self::set_state_if_session_match(&guard, ExtHostLifecycleState::WsClosed, session_id) {
                guard.lifecycle_state = s;
            }
        }
    }

    pub fn on_host_ready_timeout(&self, session_id: &str) {
        if let Ok(mut guard) = self.inner.lock() {
            if let Some(s) = Self::set_state_if_session_match(&guard, ExtHostLifecycleState::HostReadyTimeout, session_id) {
                guard.lifecycle_state = s;
            }
        }
    }

    pub fn on_activation_failed(&self, session_id: &str) {
        if let Ok(mut guard) = self.inner.lock() {
            if let Some(s) = Self::set_state_if_session_match(&guard, ExtHostLifecycleState::ActivationFailed, session_id) {
                guard.lifecycle_state = s;
            }
        }
    }

    pub fn on_host_exited(&self, session_id: &str) {
        if let Ok(mut guard) = self.inner.lock() {
            if let Some(s) = Self::set_state_if_session_match(&guard, ExtHostLifecycleState::HostExited, session_id) {
                guard.lifecycle_state = s;
            }
        }
    }

    pub fn stop(&self) -> Result<(), String> {
        let mut guard = self.inner.lock().map_err(|e| e.to_string())?;
        if let Some(mut session) = guard.session.take() {
            let pid = session.child.id();
            let _ = session.child.kill();
            let _ = session.child.wait();
            log::info!(
                "[ext-host] stopped session={} pid={} port={}",
                session.session_id,
                pid,
                session.port
            );
        }
        guard.lifecycle_state = ExtHostLifecycleState::Idle;
        Ok(())
    }

    pub fn restart(
        &self,
        app: &AppHandle,
        init_data_json: &str,
        extension_search_paths: &[String],
    ) -> Result<u16, String> {
        if let Ok(mut guard) = self.inner.lock() {
            guard.crash_loop_detected = false;
            guard.degraded_at = None;
            guard.crash_history.clear();
            guard.lifecycle_state = ExtHostLifecycleState::Bootstrapping;
        }
        self.stop()?;
        self.ensure_started(app, init_data_json, extension_search_paths)
    }

    fn check_child_exit_locked(guard: &mut SupervisorState) {
        if let Some(ref mut session) = guard.session {
            if let Some(status) = session.child.try_wait().ok().flatten() {
                guard.total_crashes += 1;
                let session_id = session.session_id.clone();
                guard.crash_history.push_back(CrashRecord {
                    timestamp: Instant::now(),
                    session_id: session_id.clone(),
                });
                log::warn!(
                    "[ext-host/exit] session={} pid={} exited: code={:?} (total_crashes={})",
                    session_id,
                    session.child.id(),
                    status.code(),
                    guard.total_crashes,
                );
                guard.session = None;
                guard.lifecycle_state = ExtHostLifecycleState::HostExited;

                let window = Duration::from_secs(300);
                guard.crash_history.retain(|r| r.timestamp.elapsed() < window);
                if guard.crash_history.len() >= 3 {
                    guard.crash_loop_detected = true;
                    guard.degraded_at = Some(Instant::now());
                    guard.lifecycle_state = ExtHostLifecycleState::Degraded;
                    log::error!(
                        "[ext-host] crash loop detected: {} crashes in 5min, entering Degraded state",
                        guard.crash_history.len()
                    );
                }
            }
        }
    }

    pub fn snapshot(&self) -> Result<ExtensionPlatformRuntimeState, String> {
        let mut guard = self.inner.lock().map_err(|e| e.to_string())?;
        Self::check_child_exit_locked(&mut guard);
        match &guard.session {
            Some(s) => Ok(ExtensionPlatformRuntimeState {
                running: true,
                port: Some(s.port),
                session_id: Some(s.session_id.clone()),
                uptime_secs: Some(s.started_at.elapsed().as_secs()),
                restart_count: guard.restart_count,
                total_crashes: guard.total_crashes,
                lifecycle_state: guard.lifecycle_state.clone(),
                crash_loop_detected: guard.crash_loop_detected,
            }),
            None => Ok(ExtensionPlatformRuntimeState {
                running: false,
                port: None,
                session_id: None,
                uptime_secs: None,
                restart_count: 0,
                total_crashes: guard.total_crashes,
                lifecycle_state: guard.lifecycle_state.clone(),
                crash_loop_detected: guard.crash_loop_detected,
            }),
        }
    }
}

// Supporting types

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionTransportInfo {
    pub kind: String,
    pub endpoint: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionPathsInfo {
    pub server_script: String,
    pub builtin_extensions_dir: String,
    pub user_extensions_dir: String,
    pub global_storage_dir: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionPlatformBootstrap {
    pub transport: ExtensionTransportInfo,
    pub runtime: NodeRuntimeInfo,
    pub paths: ExtensionPathsInfo,
    pub session_kind: String,
    pub extensions: Vec<ExtensionManifestSummary>,
    pub init_data_json: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionPlatformStatus {
    pub running: bool,
    pub port: Option<u16>,
    pub session_id: Option<String>,
    pub uptime_secs: Option<u64>,
    pub extension_count: Option<usize>,
    pub restart_count: Option<u32>,
    pub total_crashes: u32,
    pub lifecycle_state: ExtHostLifecycleState,
    pub crash_loop_detected: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionManifestSummary {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: String,
    pub activation_events: Vec<String>,
    pub main: Option<String>,
    pub browser: Option<String>,
    pub wasm_binary: Option<String>,
    pub contributes: Vec<String>,
    pub location: String,
}

// Port protocol

const SIDEX_EXT_HOST_PORT_PREFIX: &str = "SIDEX_EXT_HOST_PORT ";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PortMessage {
    #[serde(rename = "type")]
    msg_type: String,
    session_id: String,
    port: u16,
}

// Spawn result types

struct StartedSession {
    port: u16,
    session_id: String,
    init_data: ExtensionHostInitData,
    manifests: Vec<ExtensionManifest>,
    child: Child,
    node_runtime: ResolvedNodeExt,
    server_script_path: PathBuf,
}

struct ResolvedNodeExt {
    path: String,
    version: Option<String>,
    source: String,
    bundled: bool,
}

// spawn_host_process
// Reads port from stdout using a single-shot reader thread that exits
// immediately after sending the first line. No lingering thread on success.

fn spawn_host_process(
    app: &AppHandle,
    workspace_folders: &[String],
) -> Result<StartedSession, String> {
    let runtime = resolve_node_runtime(app)
        .map_err(|e| format!("ERR_NODE_MISSING: {e}"))?;
    let node_path = runtime.path.clone();
    let node_version = runtime.version.clone();
    let node_source = runtime.source.to_string();
    let node_bundled = runtime.bundled;

    let server_js = normalize_for_node(resolve_server_script(app));

    if !server_js.exists() {
        log::error!("[ext-host/spawn] server script not found at {}", server_js.display());
        return Err(format!(
            "ERR_SERVER_SCRIPT_MISSING: extension host script not found at {}",
            server_js.display()
        ));
    }

    let user_ext_dir = user_extensions_dir();
    let builtin_ext_dir = resolve_builtin_extensions_dir(app);
    let global_store_dir = global_storage_dir();
    let search_paths = extension_search_paths(app);

    let manifests = scan_extensions(app, &search_paths);
    let descriptions = build_extension_descriptions(&manifests);
    let init_data = build_init_data(&descriptions, workspace_folders);
    let session_id = uuid::Uuid::new_v4().to_string();
    let init_data_file = std::env::temp_dir().join(format!("sidex-init-{}.json", &session_id));

    let scanned_ids: Vec<&str> = manifests.iter().map(|m| m.id.as_str()).collect();

    log::info!("[ext-host/bootstrap] session={} invoked", session_id);
    log::info!(
        "[ext-host/node] path={} version={:?} source={} bundled={}",
        node_path, node_version, node_source, node_bundled
    );
    log::info!(
        "[ext-host/spawn] server_script={} exists={}",
        server_js.display(),
        server_js.exists()
    );
    log::info!("[ext-host/spawn] extensions_dir={}", user_ext_dir.display());
    log::info!("[ext-host/spawn] builtin_extensions_dir={}", builtin_ext_dir.display());
    log::info!(
        "[ext-host/spawn] searched {} paths, found {} extensions: {:?}",
        search_paths.len(),
        manifests.len(),
        scanned_ids
    );
    log::info!("[ext-host/spawn] init_data_file={}", init_data_file.display());

    let init_data_json = serde_json::to_string(&init_data)
        .map_err(|e| format!("failed to serialize init data: {e}"))?;
    let search_paths_json = serde_json::to_string(
        &search_paths.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>(),
    )
    .map_err(|e| format!("failed to encode search paths: {e}"))?;

    std::fs::write(&init_data_file, &init_data_json)
        .map_err(|e| format!("failed to write init data file: {e}"))?;

    let mut child_cmd = Command::new(&node_path);
    child_cmd
        .arg("--max-old-space-size=3072")
        .arg(&server_js)
        .env("SIDEX_EXTENSIONS_DIR", &user_ext_dir)
        .env("SIDEX_BUILTIN_EXTENSIONS_DIR", &builtin_ext_dir)
        .env("SIDEX_GLOBAL_STORAGE_DIR", &global_store_dir)
        .env("SIDEX_EXTENSION_SEARCH_PATHS", &search_paths_json)
        .env("SIDEX_INIT_DATA_FILE", &init_data_file)
        .env("SIDEX_SESSION_ID", &session_id)
        .env("NODE_ENV", "production")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    {
        child_cmd.creation_flags(0x0800_0000);
    }

    let mut child = child_cmd
        .spawn()
        .map_err(|e| format!("ERR_SPAWN_FAILED: failed to spawn extension host: {e}"))?;

    let child_pid = child.id();
    log::info!("[ext-host/spawn] pid={} session={}", child_pid, session_id);

    let stdout = child.stdout.take().ok_or("failed to capture extension host stdout")?;

    let stderr_buf: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf_capture = Arc::clone(&stderr_buf);
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                log::debug!("[ext-host/stderr] {line}");
                if let Ok(mut guard) = stderr_buf_capture.lock() {
                    if guard.len() >= 200 {
                        guard.remove(0);
                    }
                    guard.push(line);
                }
            }
        });
    }

    let recent_stderr = || -> String {
        thread::sleep(Duration::from_millis(150));
        match stderr_buf.lock() {
            Ok(guard) if !guard.is_empty() => format!(" stderr: {}", guard.join(" | ")),
            _ => String::new(),
        }
    };

    // Port reading with 15s timeout
    // Uses a single-shot reader thread: reads stdout lines, sends the first
    // one to a channel, then exits immediately. No lingering thread.
    let port = read_port_with_timeout(stdout, &mut child, &session_id, &init_data_file, &recent_stderr)?;

    log::info!("[ext-host/port] session={} port={} pid={}", session_id, port, child_pid);

    Ok(StartedSession {
        port,
        session_id,
        init_data,
        manifests,
        child,
        node_runtime: ResolvedNodeExt {
            path: node_path,
            version: node_version,
            source: node_source,
            bundled: node_bundled,
        },
        server_script_path: server_js,
    })
}

fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

// read_port_with_timeout
// Reads one line from stdout, parses the port, exits.
// No background thread lingers: the reader thread sends one line then exits.
// On timeout, kills child, waits, then returns error.

fn read_port_with_timeout(
    stdout: std::process::ChildStdout,
    child: &mut Child,
    session_id: &str,
    init_data_file: &std::path::Path,
    recent_stderr: &dyn Fn() -> String,
) -> Result<u16, String> {
    let timeout = Duration::from_secs(15);
    let start = Instant::now();
    let (tx, rx) = mpsc::channel::<String>();

    // Single-shot reader: reads first line from stdout, sends it, exits.
    // Dropping the sender after thread exits makes rx return Disconnected.
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) > 0 {
            let _ = tx.send(line);
        }
    });

    loop {
        if start.elapsed() > timeout {
            kill_child(child);
            log::error!(
                "[ext-host/port] timeout after {}s (session={}, init_data_file={})",
                timeout.as_secs(),
                session_id,
                init_data_file.display(),
            );
            return Err(format!(
                "ERR_PORT_TIMEOUT: extension host port timeout ({}s). init_data_file={}.{}",
                timeout.as_secs(),
                init_data_file.display(),
                recent_stderr()
            ));
        }

        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(raw_line) => {
                let trimmed = raw_line.trim().to_string();
                // Check for SIDEX_EXT_HOST_PORT prefix
                if let Some(json_part) = trimmed.strip_prefix(SIDEX_EXT_HOST_PORT_PREFIX) {
                    match serde_json::from_str::<PortMessage>(json_part.trim()) {
                        Ok(msg) => {
                            if msg.msg_type != "sidex:server-port" {
                                kill_child(child);
                                log::warn!(
                                    "[ext-host/port] unexpected type '{}' line: {}",
                                    msg.msg_type, trimmed
                                );
                                return Err("unexpected port message type".into());
                            }
                            if msg.session_id != session_id {
                                kill_child(child);
                                log::warn!(
                                    "[ext-host/port] session_id mismatch: expected={}, got={}",
                                    session_id, msg.session_id
                                );
                                return Err("session_id mismatch in port message".into());
                            }
                            return Ok(msg.port);
                        }
                        Err(e) => {
                            kill_child(child);
                            log::warn!("[ext-host/port] bad JSON: {e} (line: {trimmed:?})");
                            return Err(format!("bad port JSON: {e}"));
                        }
                    }
                } else {
                    // Fallback: plain JSON port message
                    match serde_json::from_str::<serde_json::Value>(&trimmed) {
                        Ok(val) => {
                            if let Some(port_val) = val.get("port").and_then(|p| p.as_u64()) {
                                log::warn!(
                                    "[ext-host/port] legacy port format, line={trimmed:?} session={session_id}"
                                );
                                return Ok(port_val as u16);
                            }
                            kill_child(child);
                            log::warn!(
                                "[ext-host/port] non-port JSON (session={}): {trimmed:?}",
                                session_id
                            );
                            return Err("stdout line is not a port message".into());
                        }
                        Err(_) => {
                            kill_child(child);
                            log::warn!(
                                "[ext-host/port] non-JSON stdout (session={}): {trimmed:?}",
                                session_id
                            );
                            return Err("stdout line is not valid JSON".into());
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                kill_child(child);
                log::error!(
                    "[ext-host/port] stdout reader exited without sending port (session={})",
                    session_id
                );
                return Err(format!(
                    "stdout reader exited without sending port. init_data_file={}.{}",
                    init_data_file.display(),
                    recent_stderr()
                ));
            }
        }
    }
}

// Helper functions

#[allow(dead_code)]
fn build_manifest_summaries(manifests: &[ExtensionManifest]) -> Vec<ExtensionManifestSummary> {
    manifests
        .iter()
        .map(|m| ExtensionManifestSummary {
            id: m.id.clone(),
            name: m.display_name.clone(),
            version: m.version.clone(),
            kind: match m.kind {
                ExtensionKind::Node => "node".to_string(),
                ExtensionKind::Wasm => "wasm".to_string(),
            },
            activation_events: m.activation_events.clone(),
            main: m.main.clone(),
            browser: m.browser.clone(),
            wasm_binary: m.wasm_binary.clone(),
            contributes: m.contributes_keys.clone(),
            location: m.path.clone(),
        })
        .collect()
}

#[allow(dead_code)]
fn ensure_session(guard: &mut SupervisorState, app: &AppHandle) -> Result<(), String> {
    if guard.session.is_some() {
        return Ok(());
    }
    let started = spawn_host_process(app, &[])?;
    guard.session = Some(ExtHostSession {
        child: started.child,
        port: started.port,
        session_id: started.session_id,
        started_at: Instant::now(),
        init_data: started.init_data,
        manifests: started.manifests,
        recent_stderr: Vec::new(),
    });
    Ok(())
}

#[allow(dead_code)]
fn kill_session(session: &mut ExtHostSession) {
    let pid = session.child.id();
    let _ = session.child.kill();
    let _ = session.child.wait();
    log::info!(
        "[ext-host] stopped session={} pid={} port={} uptime={}s",
        session.session_id,
        pid,
        session.port,
        session.started_at.elapsed().as_secs()
    );
}
