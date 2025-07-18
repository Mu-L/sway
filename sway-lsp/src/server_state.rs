//! The context or environment in which the language server functions.

use crate::{
    capabilities::runnable::Runnable,
    config::{Config, GarbageCollectionConfig, Warnings},
    core::{
        document::{Documents, PidLockedFiles},
        session::{self, program_id_from_path, Session},
        sync::SyncWorkspace,
        token_map::TokenMap,
    },
    error::{DirectoryError, DocumentError, LanguageServerError},
    utils::{debug, keyword_docs::KeywordDocs},
};
use crossbeam_channel::{Receiver, Sender};
use dashmap::{
    mapref::{multiple::RefMulti, one::Ref},
    DashMap,
};
use forc_pkg::{
    manifest::{GenericManifestFile, ManifestFile},
    PackageManifestFile,
};
use lsp_types::{
    Diagnostic, DidChangeWatchedFilesRegistrationOptions, FileSystemWatcher, GlobPattern,
    Registration, Url, WatchKind,
};
use parking_lot::{Mutex, RwLock};
use std::{
    collections::{BTreeMap, VecDeque},
    mem,
    ops::Deref,
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use sway_core::{language::Programs, Engines, LspConfig};
use sway_types::ProgramId;
use tokio::sync::Notify;
use tower_lsp::{jsonrpc, Client};

const DEFAULT_SESSION_CACHE_CAPACITY: usize = 4;

pub type RunnableMap = DashMap<PathBuf, Vec<Box<dyn Runnable>>>;

/// `ServerState` is the primary mutable state of the language server
pub struct ServerState {
    pub(crate) client: Option<Client>,
    pub config: Arc<RwLock<Config>>,
    pub sync_workspaces: DashMap<PathBuf, Arc<SyncWorkspace>>,
    pub token_map: Arc<TokenMap>,
    pub engines: Arc<RwLock<Engines>>,
    pub compiled_programs: Arc<CompiledPrograms>,
    pub runnables: Arc<RunnableMap>,
    pub(crate) keyword_docs: Arc<KeywordDocs>,
    /// A Least Recently Used (LRU) cache of [Session]s, each representing a project opened in the user's workspace.
    /// This cache limits memory usage by maintaining a fixed number of active sessions, automatically
    /// evicting the least recently used sessions when the capacity is reached.
    pub sessions: LruSessionCache,
    pub documents: Documents,
    // Compilation thread related fields
    pub(crate) retrigger_compilation: Arc<AtomicBool>,
    pub is_compiling: Arc<AtomicBool>,
    pub(crate) cb_tx: Sender<TaskMessage>,
    pub(crate) cb_rx: Arc<Receiver<TaskMessage>>,
    pub(crate) finished_compilation: Arc<Notify>,
    pub(crate) pid_locked_files: PidLockedFiles,
    manifest_cache: DashMap<Url, Arc<PathBuf>>,
    last_compilation_state: Arc<RwLock<LastCompilationState>>,
}

impl Default for ServerState {
    fn default() -> Self {
        let (cb_tx, cb_rx) = crossbeam_channel::bounded(1);
        let state = ServerState {
            client: None,
            token_map: Arc::new(TokenMap::new()),
            engines: Arc::new(RwLock::new(Engines::default())),
            compiled_programs: Arc::new(CompiledPrograms(DashMap::new())),
            runnables: Arc::new(RunnableMap::new()),
            config: Arc::new(RwLock::new(Config::default())),
            sync_workspaces: DashMap::new(),
            keyword_docs: Arc::new(KeywordDocs::new()),
            sessions: LruSessionCache::new(DEFAULT_SESSION_CACHE_CAPACITY),
            documents: Documents::new(),
            retrigger_compilation: Arc::new(AtomicBool::new(false)),
            is_compiling: Arc::new(AtomicBool::new(false)),
            cb_tx,
            cb_rx: Arc::new(cb_rx),
            finished_compilation: Arc::new(Notify::new()),
            pid_locked_files: PidLockedFiles::new(),
            manifest_cache: DashMap::new(),
            last_compilation_state: Arc::new(RwLock::new(LastCompilationState::Uninitialized)),
        };
        // Spawn a new thread dedicated to handling compilation tasks
        state.spawn_compilation_thread();
        state
    }
}

/// `LastCompilationState` represents the state of the last compilation process.
/// It is primarily used for debugging purposes.
#[derive(Debug, PartialEq)]
enum LastCompilationState {
    Success,
    Failed,
    Uninitialized,
}

/// `TaskMessage` represents the set of messages or commands that can be sent to and processed by a worker thread in the compilation environment.
#[derive(Debug)]
pub enum TaskMessage {
    CompilationContext(CompilationContext),
    // A signal to the receiving thread to gracefully terminate its operation.
    Terminate,
}

/// `CompilationContext` encapsulates all the necessary details required by the compilation thread to execute a compilation process.
/// It acts as a container for shared resources and state information relevant to a specific compilation task.
#[derive(Debug)]
pub struct CompilationContext {
    pub session: Arc<Session>,
    pub sync: Arc<SyncWorkspace>,
    pub token_map: Arc<TokenMap>,
    pub engines: Arc<RwLock<Engines>>,
    pub compiled_programs: Arc<CompiledPrograms>,
    pub runnables: Arc<RunnableMap>,
    pub uri: Url,
    pub version: Option<i32>,
    pub optimized_build: bool,
    pub gc_options: GarbageCollectionConfig,
    pub file_versions: BTreeMap<PathBuf, Option<u64>>,
}

impl ServerState {
    pub fn new(client: Client) -> ServerState {
        ServerState {
            client: Some(client),
            ..Default::default()
        }
    }

    /// Registers a file system watcher for Forc.toml files with the client.
    pub async fn register_forc_toml_watcher(&self) -> Result<(), LanguageServerError> {
        let client = self
            .client
            .as_ref()
            .ok_or(LanguageServerError::ClientNotInitialized)?;

        let watchers = vec![FileSystemWatcher {
            glob_pattern: GlobPattern::String("**/Forc.toml".to_string()),
            kind: Some(WatchKind::Create | WatchKind::Change),
        }];
        let registration_options = DidChangeWatchedFilesRegistrationOptions { watchers };
        let registration = Registration {
            id: "forc-toml-watcher".to_string(),
            method: "workspace/didChangeWatchedFiles".to_string(),
            register_options: Some(
                serde_json::to_value(registration_options)
                    .expect("Failed to serialize registration options"),
            ),
        };

        client
            .register_capability(vec![registration])
            .await
            .map_err(|err| LanguageServerError::ClientRequestError(err.to_string()))?;

        Ok(())
    }

    /// Spawns a new thread dedicated to handling compilation tasks. This thread listens for
    /// `TaskMessage` instances sent over a channel and processes them accordingly.
    ///
    /// This approach allows for asynchronous compilation tasks to be handled in parallel to
    /// the main application flow, improving efficiency and responsiveness.
    pub fn spawn_compilation_thread(&self) {
        let is_compiling = self.is_compiling.clone();
        let retrigger_compilation = self.retrigger_compilation.clone();
        let finished_compilation = self.finished_compilation.clone();
        let rx = self.cb_rx.clone();
        let last_compilation_state = self.last_compilation_state.clone();
        std::thread::spawn(move || {
            while let Ok(msg) = rx.recv() {
                match msg {
                    TaskMessage::CompilationContext(ctx) => {
                        let uri = &ctx.uri;
                        let path = uri.to_file_path().unwrap();
                        let mut engines_clone = ctx.engines.read().clone();
                        let lsp_mode = Some(LspConfig {
                            optimized_build: ctx.optimized_build,
                            file_versions: ctx.file_versions.clone(),
                        });

                        let (needs_reprocessing, _) =
                            needs_reprocessing(&ctx.token_map, &path, lsp_mode.as_ref());

                        // Perform garbage collection if enabled and if the file has been modified to manage memory usage.
                        if ctx.gc_options.gc_enabled && needs_reprocessing {
                            // Call this on the engines clone so we don't clear types that are still in use
                            // and might be needed in the case cancel compilation was triggered.
                            if let Err(err) =
                                session::garbage_collect_module(&mut engines_clone, uri)
                            {
                                tracing::error!(
                                    "Unable to perform garbage collection: {}",
                                    err.to_string()
                                );
                            }
                        }

                        // Set the is_compiling flag to true so that the wait_for_parsing function knows that we are compiling
                        is_compiling.store(true, Ordering::SeqCst);
                        match session::parse_project(
                            uri,
                            &engines_clone,
                            Some(retrigger_compilation.clone()),
                            &ctx,
                            lsp_mode.as_ref(),
                        ) {
                            Ok(()) => {
                                // Use the uri to get the metrics for the program
                                match ctx.compiled_programs.program_from_uri(uri, &engines_clone) {
                                    Some(program) => {
                                        // It's very important to check if the workspace AST was reused to determine if we need to overwrite the engines.
                                        // Because the engines_clone has garbage collection applied. If the workspace AST was reused, we need to keep the old engines
                                        // as the engines_clone might have cleared some types that are still in use.
                                        if program.value().metrics.reused_programs == 0 {
                                            // Commit local changes in the programs, module, and function caches to the shared state.
                                            // This ensures that any modifications made during compilation are preserved
                                            // before we swap the engines.
                                            engines_clone.qe().commit();
                                            // The compiler did not reuse the workspace AST.
                                            // We need to overwrite the old engines with the engines clone.
                                            mem::swap(
                                                &mut *ctx.engines.write(),
                                                &mut engines_clone,
                                            );
                                        }
                                        *last_compilation_state.write() =
                                            LastCompilationState::Success;
                                    }
                                    None => {
                                        *last_compilation_state.write() =
                                            LastCompilationState::Failed;
                                    }
                                }
                            }
                            Err(err) => {
                                tracing::error!("{}", err.to_string());
                                *last_compilation_state.write() = LastCompilationState::Failed;
                            }
                        }

                        // Reset the flags to false
                        is_compiling.store(false, Ordering::SeqCst);
                        retrigger_compilation.store(false, Ordering::SeqCst);

                        // Make sure there isn't any pending compilation work
                        if rx.is_empty() {
                            // finished compilation, notify waiters
                            finished_compilation.notify_waiters();
                        }
                    }
                    TaskMessage::Terminate => {
                        // If we receive a terminate message, we need to exit the thread
                        return;
                    }
                }
            }
        });
    }

    /// Spawns a new thread dedicated to checking if the client process is still active,
    /// and if not, shutting down the server.
    pub fn spawn_client_heartbeat(&self, client_pid: usize) {
        tokio::spawn(async move {
            loop {
                // Not using sysinfo here because it has compatibility issues with fuel.nix
                // https://github.com/FuelLabs/fuel.nix/issues/64
                let output = Command::new("ps")
                    .arg("-p")
                    .arg(client_pid.to_string())
                    .output()
                    .expect("Failed to execute ps command");

                if String::from_utf8_lossy(&output.stdout).contains(&format!("{client_pid} ")) {
                    tracing::trace!("Client Heartbeat: still running ({client_pid})");
                } else {
                    std::process::exit(0);
                }
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });
    }

    /// Waits asynchronously for the `is_compiling` flag to become false.
    ///
    /// This function checks the state of `is_compiling`, and if it's true,
    /// it awaits on a notification. Once notified, it checks again, repeating
    /// this process until `is_compiling` becomes false.
    pub async fn wait_for_parsing(&self) {
        loop {
            // Check both the is_compiling flag and the last_compilation_state.
            // Wait if is_compiling is true or if the last_compilation_state is Uninitialized.
            if !self.is_compiling.load(Ordering::SeqCst)
                && *self.last_compilation_state.read() != LastCompilationState::Uninitialized
            {
                // compilation is finished, lets check if there are pending compilation requests.
                if self.cb_rx.is_empty() {
                    // no pending compilation work, safe to break.
                    break;
                }
            }
            // We are still compiling, lets wait to be notified.
            self.finished_compilation.notified().await;
        }
    }

    pub fn shutdown_server(&self) -> jsonrpc::Result<()> {
        let _p = tracing::trace_span!("shutdown_server").entered();
        tracing::info!("Shutting Down the Sway Language Server");

        // Drain pending compilation requests
        while self.cb_rx.try_recv().is_ok() {}

        // Set the retrigger_compilation flag to true so that the compilation exits early
        self.retrigger_compilation.store(true, Ordering::SeqCst);

        // Send a terminate message to the compilation thread
        self.cb_tx
            .send(TaskMessage::Terminate)
            .expect("failed to send terminate message");

        // Delete all temporary directories.
        for entry in self.sync_workspaces.iter() {
            entry.value().remove_temp_dir();
        }

        Ok(())
    }

    pub(crate) async fn publish_diagnostics(
        &self,
        uri: Url,
        workspace_uri: Url,
        session: Arc<Session>,
    ) {
        let diagnostics = self.diagnostics(&uri, session.clone());
        // Note: Even if the computed diagnostics vec is empty, we still have to push the empty Vec
        // in order to clear former diagnostics. Newly pushed diagnostics always replace previously pushed diagnostics.
        if let Some(client) = self.client.as_ref() {
            client
                .publish_diagnostics(workspace_uri.clone(), diagnostics, None)
                .await;
        }
    }

    fn diagnostics(&self, uri: &Url, session: Arc<Session>) -> Vec<Diagnostic> {
        let mut diagnostics_to_publish = vec![];
        let config = &self.config.read();
        let tokens = self.token_map.tokens_for_file(uri);
        match config.debug.show_collected_tokens_as_warnings {
            // If collected_tokens_as_warnings is Parsed or Typed,
            // take over the normal error and warning display behavior
            // and instead show the either the parsed or typed tokens as warnings.
            // This is useful for debugging the lsp parser.
            Warnings::Parsed => {
                diagnostics_to_publish = debug::generate_warnings_for_parsed_tokens(tokens);
            }
            Warnings::Typed => {
                diagnostics_to_publish = debug::generate_warnings_for_typed_tokens(tokens);
            }
            Warnings::Default => {
                if let Some(diagnostics) =
                    session.diagnostics.read().get(&PathBuf::from(uri.path()))
                {
                    if config.diagnostic.show_warnings {
                        diagnostics_to_publish.extend(diagnostics.warnings.clone());
                    }
                    if config.diagnostic.show_errors {
                        diagnostics_to_publish.extend(diagnostics.errors.clone());
                    }
                }
            }
        }
        diagnostics_to_publish
    }

    /// Constructs and returns a tuple of `(Arc<SyncWorkspace>, Url)` from a given workspace URI.
    /// The returned URL represents the temp directory workspace.
    pub fn sync_and_uri_from_workspace(
        &self,
        workspace_uri: &Url,
    ) -> Result<(Arc<SyncWorkspace>, Url), LanguageServerError> {
        let sync = self.get_sync_workspace_for_uri(workspace_uri)?;
        let uri = sync.workspace_to_temp_url(workspace_uri)?;
        Ok((sync, uri))
    }

    /// Constructs and returns a tuple of `(Arc<SyncWorkspace>, Url, Arc<Session>)` from a given workspace URI.
    /// The returned URL represents the temp directory workspace.
    pub fn sync_uri_and_session_from_workspace(
        &self,
        workspace_uri: &Url,
    ) -> Result<(Arc<SyncWorkspace>, Url, Arc<Session>), LanguageServerError> {
        let sync = self.get_sync_workspace_for_uri(workspace_uri)?;
        let uri = sync.workspace_to_temp_url(workspace_uri)?;
        let session = self.url_to_session(workspace_uri)?;
        Ok((sync, uri, session))
    }

    /// Constructs and returns a tuple of `(Url, Arc<Session>)` from a given workspace URI.
    /// The returned URL represents the temp directory workspace.
    pub fn uri_and_session_from_workspace(
        &self,
        workspace_uri: &Url,
    ) -> Result<(Url, Arc<Session>), LanguageServerError> {
        let temp_uri = self.uri_from_workspace(workspace_uri)?;
        let session = self.url_to_session(workspace_uri)?;
        Ok((temp_uri, session))
    }

    /// Constructs and returns the temp directory URL from a given workspace URI.
    pub fn uri_from_workspace(&self, workspace_uri: &Url) -> Result<Url, LanguageServerError> {
        let sync = self.get_sync_workspace_for_uri(workspace_uri)?;
        sync.workspace_to_temp_url(workspace_uri)
            .map_err(LanguageServerError::from)
    }

    fn url_to_session(&self, uri: &Url) -> Result<Arc<Session>, LanguageServerError> {
        // Try to get the manifest directory from the cache
        let manifest_dir = if let Some(cached_dir) = self.manifest_cache.get(uri) {
            cached_dir.clone()
        } else {
            // Otherwise, find the manifest directory from the uri and cache it
            let path = PathBuf::from(uri.path());
            let manifest = PackageManifestFile::from_dir(&path).map_err(|_| {
                DocumentError::ManifestFileNotFound {
                    dir: path.to_string_lossy().to_string(),
                }
            })?;
            let dir = Arc::new(
                manifest
                    .path()
                    .parent()
                    .ok_or(DirectoryError::ManifestDirNotFound)?
                    .to_path_buf(),
            );
            self.manifest_cache.insert(uri.clone(), dir.clone());
            dir
        };

        // If the session is already in the cache, return it
        if let Some(session) = self.sessions.get(&manifest_dir) {
            return Ok(session);
        }

        // If no session can be found, then we need to call init and insert a new session into the map
        let session = Arc::new(Session::new());
        self.sessions
            .insert((*manifest_dir).clone(), session.clone());

        Ok(session)
    }

    /// Determines the workspace root for the given file URI.
    fn find_workspace_root_for_uri(&self, file_uri: &Url) -> Result<PathBuf, LanguageServerError> {
        let path = PathBuf::from(file_uri.path());
        let search_dir = path.parent().unwrap_or(&path);

        // Find the initial manifest (could be package or workspace)
        let initial_manifest_file = ManifestFile::from_dir(search_dir).map_err(|_| {
            DocumentError::ManifestFileNotFound {
                dir: search_dir.to_string_lossy().into(),
            }
        })?;

        // Determine the true workspace root.
        // If the initial manifest is a package that's part of a workspace, get that workspace root.
        // Otherwise, the initial manifest's directory is the root.
        let actual_sync_root = match &initial_manifest_file {
            ManifestFile::Package(pkg_mf) => {
                // Check if this package is part of a workspace
                match pkg_mf
                    .workspace()
                    .map_err(|e| DocumentError::WorkspaceManifestNotFound { err: e.to_string() })?
                {
                    Some(ws_mf) => {
                        // It's part of a workspace, use the workspace's directory
                        tracing::trace!(
                            "Package {:?} is part of workspace {:?}. Using workspace root.",
                            pkg_mf.path(),
                            ws_mf.path()
                        );
                        ws_mf.dir().to_path_buf()
                    }
                    None => {
                        // It's a standalone package, use its directory
                        tracing::trace!(
                            "Package {:?} is standalone. Using package root.",
                            pkg_mf.path()
                        );
                        initial_manifest_file.dir().to_path_buf()
                    }
                }
            }
            ManifestFile::Workspace(ws_mf) => {
                // It's already a workspace manifest, use its directory
                tracing::trace!(
                    "Initial manifest is a workspace: {:?}. Using its root.",
                    ws_mf.path()
                );
                initial_manifest_file.dir().to_path_buf()
            }
        };

        Ok(actual_sync_root)
    }

    /// Gets the SyncWorkspace for the given URI.
    pub fn get_sync_workspace_for_uri(
        &self,
        uri: &Url,
    ) -> Result<Arc<SyncWorkspace>, LanguageServerError> {
        let workspace_root = self.find_workspace_root_for_uri(uri)?;
        self.sync_workspaces
            .get(&workspace_root)
            .map(|entry| entry.value().clone())
            .ok_or(LanguageServerError::GlobalWorkspaceNotInitialized)
    }

    /// Gets the existing SyncWorkspace for a file URI or initializes it if it doesn't exist.
    pub async fn get_or_init_sync_workspace(
        &self,
        uri: &Url,
    ) -> Result<Arc<SyncWorkspace>, LanguageServerError> {
        // First try to find the workspace root for this URI
        let workspace_root = self.find_workspace_root_for_uri(uri)?;
        let canonical_root = workspace_root.canonicalize().unwrap_or(workspace_root);

        // Check if we already have a SyncWorkspace for this root
        if let Some(sw_arc) = self.sync_workspaces.get(&canonical_root) {
            return Ok(sw_arc.value().clone());
        }

        // Otherwise, initialize a new workspace sync for this root
        let initialized_sw = self.initialize_workspace_sync(uri).await?;

        self.sync_workspaces
            .insert(canonical_root.clone(), initialized_sw.clone());

        tracing::info!(
            "SyncWorkspace successfully initialized for {:?}",
            canonical_root
        );
        Ok(initialized_sw)
    }

    /// Initializes a new SyncWorkspace by creating a temporary directory structure and syncing manifest files.
    /// This sets up the workspace environment needed for language server operations.
    pub async fn initialize_workspace_sync(
        &self,
        file_uri_triggering_init: &Url,
    ) -> Result<Arc<SyncWorkspace>, LanguageServerError> {
        let actual_sync_root = self.find_workspace_root_for_uri(file_uri_triggering_init)?;

        tracing::debug!(
            "Initializing SyncWorkspace for root: {:?}",
            actual_sync_root
        );

        let sw = Arc::new(SyncWorkspace::new());
        sw.create_temp_dir_from_workspace(&actual_sync_root)?;
        sw.clone_manifest_dir_to_temp()?;
        sw.sync_manifest()?;

        let temp_dir_for_docs = sw.temp_dir()?;
        self.documents
            .store_sway_files_from_temp(temp_dir_for_docs)
            .await?;

        Ok(sw)
    }

    /// Checks if a workspace is already initialized for the given URI
    pub fn is_workspace_initialized(&self, uri: &Url) -> bool {
        if let Ok(workspace_root) = self.find_workspace_root_for_uri(uri) {
            self.sync_workspaces.contains_key(&workspace_root)
        } else {
            false
        }
    }
}

/// Determines if expensive operations (traversal, GC, etc.) should be performed
/// based on whether tokens exist and if files were modified.
pub fn needs_reprocessing<'a>(
    token_map: &TokenMap,
    path: &'a PathBuf,
    lsp_mode: Option<&'a LspConfig>,
) -> (bool, Option<&'a PathBuf>) {
    let has_tokens = token_map
        .iter()
        .any(|item| item.key().path.as_ref() == Some(path));
    let modified_file_path = modified_file(lsp_mode);
    let reprocess = !has_tokens || modified_file_path.is_some();
    (reprocess, modified_file_path)
}

/// Returns the modified file from the LspConfig if it exists.
pub fn modified_file(lsp_mode: Option<&LspConfig>) -> Option<&PathBuf> {
    lsp_mode.and_then(|mode| {
        mode.file_versions
            .iter()
            .find_map(|(path, version)| version.map(|_| path))
    })
}

#[derive(Debug)]
pub struct CompiledPrograms(DashMap<ProgramId, Programs>);

impl Default for CompiledPrograms {
    fn default() -> Self {
        CompiledPrograms(DashMap::new())
    }
}

impl CompiledPrograms {
    pub fn new() -> Self {
        CompiledPrograms::default()
    }

    pub fn program_from_uri(
        &self,
        uri: &Url,
        engines: &Engines,
    ) -> Option<Ref<'_, ProgramId, Programs>> {
        let path = uri.to_file_path().ok()?;
        let program_id = program_id_from_path(&path, engines).ok()?;
        self.get(&program_id)
    }
}

impl Deref for CompiledPrograms {
    type Target = DashMap<ProgramId, Programs>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// A Least Recently Used (LRU) cache for storing and managing `Session` objects.
/// This cache helps limit memory usage by maintaining a fixed number of active sessions.
pub struct LruSessionCache {
    /// Stores the actual `Session` objects, keyed by their file paths.
    sessions: Arc<DashMap<PathBuf, Arc<Session>>>,
    /// Keeps track of the order in which sessions were accessed, with most recent at the front.
    usage_order: Arc<Mutex<VecDeque<PathBuf>>>,
    /// The maximum number of sessions that can be stored in the cache.
    capacity: usize,
}

impl LruSessionCache {
    /// Creates a new `LruSessionCache` with the specified capacity.
    pub fn new(capacity: usize) -> Self {
        LruSessionCache {
            sessions: Arc::new(DashMap::new()),
            usage_order: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            capacity,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = RefMulti<'_, PathBuf, Arc<Session>>> {
        self.sessions.iter()
    }

    /// Retrieves a session from the cache and updates its position to the front of the usage order.
    pub fn get(&self, path: &PathBuf) -> Option<Arc<Session>> {
        if let Some(session) = self.sessions.try_get(path).try_unwrap() {
            if self.sessions.len() >= self.capacity {
                self.move_to_front(path);
            }
            Some(session.clone())
        } else {
            None
        }
    }

    /// Inserts or updates a session in the cache.
    /// If at capacity and inserting a new session, evicts the least recently used one.
    /// For existing sessions, updates their position in the usage order if at capacity.
    pub fn insert(&self, path: PathBuf, session: Arc<Session>) {
        if let Some(mut entry) = self.sessions.get_mut(&path) {
            // Session already exists, update it
            *entry = session;
            self.move_to_front(&path);
        } else {
            // New session
            if self.sessions.len() >= self.capacity {
                self.evict_least_used();
            }
            self.sessions.insert(path.clone(), session);
            let mut order = self.usage_order.lock();
            order.push_front(path);
        }
    }

    /// Moves the specified path to the front of the usage order, marking it as most recently used.
    fn move_to_front(&self, path: &PathBuf) {
        tracing::trace!("Moving path to front of usage order: {:?}", path);
        let mut order = self.usage_order.lock();
        if let Some(index) = order.iter().position(|p| p == path) {
            order.remove(index);
        }
        order.push_front(path.clone());
    }

    /// Removes the least recently used session from the cache when the capacity is reached.
    fn evict_least_used(&self) {
        let mut order = self.usage_order.lock();
        if let Some(old_path) = order.pop_back() {
            tracing::trace!(
                "Cache at capacity. Evicting least used session: {:?}",
                old_path
            );
            self.sessions.remove(&old_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    #[test]
    fn test_lru_session_cache_insertion_and_retrieval() {
        let cache = LruSessionCache::new(2);
        let path1 = PathBuf::from("/path/1");
        let path2 = PathBuf::from("/path/2");
        let session1 = Arc::new(Session::new());
        let session2 = Arc::new(Session::new());

        cache.insert(path1.clone(), session1.clone());
        cache.insert(path2.clone(), session2.clone());

        assert!(Arc::ptr_eq(&cache.get(&path1).unwrap(), &session1));
        assert!(Arc::ptr_eq(&cache.get(&path2).unwrap(), &session2));
    }

    #[test]
    fn test_lru_session_cache_capacity() {
        let cache = LruSessionCache::new(2);
        let path1 = PathBuf::from("/path/1");
        let path2 = PathBuf::from("/path/2");
        let path3 = PathBuf::from("/path/3");
        let session1 = Arc::new(Session::new());
        let session2 = Arc::new(Session::new());
        let session3 = Arc::new(Session::new());

        cache.insert(path1.clone(), session1);
        cache.insert(path2.clone(), session2);
        cache.insert(path3.clone(), session3);

        assert!(cache.get(&path1).is_none());
        assert!(cache.get(&path2).is_some());
        assert!(cache.get(&path3).is_some());
    }

    #[test]
    fn test_lru_session_cache_update_order() {
        let cache = LruSessionCache::new(2);
        let path1 = PathBuf::from("/path/1");
        let path2 = PathBuf::from("/path/2");
        let path3 = PathBuf::from("/path/3");
        let session1 = Arc::new(Session::new());
        let session2 = Arc::new(Session::new());
        let session3 = Arc::new(Session::new());

        cache.insert(path1.clone(), session1.clone());
        cache.insert(path2.clone(), session2.clone());

        // Access path1 to move it to the front
        cache.get(&path1);

        // Insert path3, which should evict path2
        cache.insert(path3.clone(), session3);

        assert!(cache.get(&path1).is_some());
        assert!(cache.get(&path2).is_none());
        assert!(cache.get(&path3).is_some());
    }

    #[test]
    fn test_lru_session_cache_overwrite() {
        let cache = LruSessionCache::new(2);
        let path1 = PathBuf::from("/path/1");
        let session1 = Arc::new(Session::new());
        let session1_new = Arc::new(Session::new());

        cache.insert(path1.clone(), session1);
        cache.insert(path1.clone(), session1_new.clone());

        assert!(Arc::ptr_eq(&cache.get(&path1).unwrap(), &session1_new));
    }
}
