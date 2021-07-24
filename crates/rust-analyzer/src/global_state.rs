//! The context or environment in which the language server functions. In our
//! server implementation this is know as the `WorldState`.
//!
//! Each tick provides an immutable snapshot of the state as `WorldSnapshot`.

use std::{sync::Arc, time::Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use flycheck::FlycheckHandle;
use ide::{Analysis, AnalysisHost, Cancellable, Change, FileId};
use ide_db::base_db::{CrateId, VfsPath};
use lsp_types::{SemanticTokens, Url};
use parking_lot::{Mutex, RwLock};
use project_model::{
    CargoWorkspace, ProcMacroClient, ProjectWorkspace, Target, WorkspaceBuildScripts,
};
use rustc_hash::FxHashMap;
use vfs::AnchoredPathBuf;

use crate::{
    config::Config,
    diagnostics::{CheckFixes, DiagnosticCollection},
    document::DocumentData,
    from_proto,
    line_index::{LineEndings, LineIndex},
    lsp_ext,
    main_loop::Task,
    op_queue::OpQueue,
    reload::SourceRootConfig,
    request_metrics::{LatestRequests, RequestMetrics},
    thread_pool::TaskPool,
    to_proto::url_from_abs_path,
    Result,
};

// Enforces drop order
pub(crate) struct Handle<H, C> {
    pub(crate) handle: H,
    pub(crate) receiver: C,
}

pub(crate) type ReqHandler = fn(&mut GlobalState, lsp_server::Response);
pub(crate) type ReqQueue = lsp_server::ReqQueue<(String, Instant), ReqHandler>;

/// `GlobalState` is the primary mutable state of the language server
///
/// The most interesting components are `vfs`, which stores a consistent
/// snapshot of the file systems, and `analysis_host`, which stores our
/// incremental salsa database.
///
/// Note that this struct has more than on impl in various modules!
pub(crate) struct GlobalState {
    sender: Sender<lsp_server::Message>,
    req_queue: ReqQueue,
    pub(crate) task_pool: Handle<TaskPool<Task>, Receiver<Task>>,
    pub(crate) loader: Handle<Box<dyn vfs::loader::Handle>, Receiver<vfs::loader::Message>>,
    pub(crate) config: Arc<Config>,
    pub(crate) analysis_host: AnalysisHost,
    pub(crate) diagnostics: DiagnosticCollection,
    pub(crate) mem_docs: FxHashMap<VfsPath, DocumentData>,
    pub(crate) semantic_tokens_cache: Arc<Mutex<FxHashMap<Url, SemanticTokens>>>,
    pub(crate) shutdown_requested: bool,
    pub(crate) last_reported_status: Option<lsp_ext::ServerStatusParams>,
    pub(crate) source_root_config: SourceRootConfig,
    pub(crate) proc_macro_client: Option<ProcMacroClient>,

    pub(crate) flycheck: Vec<FlycheckHandle>,
    pub(crate) flycheck_sender: Sender<flycheck::Message>,
    pub(crate) flycheck_receiver: Receiver<flycheck::Message>,

    pub(crate) vfs: Arc<RwLock<(vfs::Vfs, FxHashMap<FileId, LineEndings>)>>,
    pub(crate) vfs_config_version: u32,
    pub(crate) vfs_progress_config_version: u32,
    pub(crate) vfs_progress_n_total: usize,
    pub(crate) vfs_progress_n_done: usize,

    /// `workspaces` field stores the data we actually use, while the `OpQueue`
    /// stores the result of the last fetch.
    ///
    /// If the fetch (partially) fails, we do not update the current value.
    ///
    /// The handling of build data is subtle. We fetch workspace in two phases:
    ///
    /// *First*, we run `cargo metadata`, which gives us fast results for
    /// initial analysis.
    ///
    /// *Second*, we run `cargo check` which runs build scripts and compiles
    /// proc macros.
    ///
    /// We need both for the precise analysis, but we want rust-analyzer to be
    /// at least partially available just after the first phase. That's because
    /// first phase is much faster, and is much less likely to fail.
    ///
    /// This creates a complication -- by the time the second phase completes,
    /// the results of the fist phase could be invalid. That is, while we run
    /// `cargo check`, the user edits `Cargo.toml`, we notice this, and the new
    /// `cargo metadata` completes before `cargo check`.
    ///
    /// An additional complication is that we want to avoid needless work. When
    /// the user just adds comments or whitespace to Cargo.toml, we do not want
    /// to invalidate any salsa caches.
    pub(crate) workspaces: Arc<Vec<ProjectWorkspace>>,
    pub(crate) fetch_workspaces_queue: OpQueue<Vec<anyhow::Result<ProjectWorkspace>>>,
    pub(crate) fetch_build_data_queue:
        OpQueue<(Arc<Vec<ProjectWorkspace>>, Vec<anyhow::Result<WorkspaceBuildScripts>>)>,

    pub(crate) prime_caches_queue: OpQueue<()>,

    latest_requests: Arc<RwLock<LatestRequests>>,
}

/// An immutable snapshot of the world's state at a point in time.
pub(crate) struct GlobalStateSnapshot {
    pub(crate) config: Arc<Config>,
    pub(crate) analysis: Analysis,
    pub(crate) check_fixes: CheckFixes,
    pub(crate) latest_requests: Arc<RwLock<LatestRequests>>,
    mem_docs: FxHashMap<VfsPath, DocumentData>,
    pub(crate) semantic_tokens_cache: Arc<Mutex<FxHashMap<Url, SemanticTokens>>>,
    vfs: Arc<RwLock<(vfs::Vfs, FxHashMap<FileId, LineEndings>)>>,
    pub(crate) workspaces: Arc<Vec<ProjectWorkspace>>,
}

impl GlobalState {
    pub(crate) fn new(sender: Sender<lsp_server::Message>, config: Config) -> GlobalState {
        let loader = {
            let (sender, receiver) = unbounded::<vfs::loader::Message>();
            let handle: vfs_notify::NotifyHandle =
                vfs::loader::Handle::spawn(Box::new(move |msg| sender.send(msg).unwrap()));
            let handle = Box::new(handle) as Box<dyn vfs::loader::Handle>;
            Handle { handle, receiver }
        };

        let task_pool = {
            let (sender, receiver) = unbounded();
            let handle = TaskPool::new(sender);
            Handle { handle, receiver }
        };

        let analysis_host = AnalysisHost::new(config.lru_capacity());
        let (flycheck_sender, flycheck_receiver) = unbounded();
        let mut this = GlobalState {
            sender,
            req_queue: ReqQueue::default(),
            task_pool,
            loader,
            config: Arc::new(config.clone()),
            analysis_host,
            diagnostics: Default::default(),
            mem_docs: FxHashMap::default(),
            semantic_tokens_cache: Arc::new(Default::default()),
            shutdown_requested: false,
            last_reported_status: None,
            source_root_config: SourceRootConfig::default(),
            proc_macro_client: None,

            flycheck: Vec::new(),
            flycheck_sender,
            flycheck_receiver,

            vfs: Arc::new(RwLock::new((vfs::Vfs::default(), FxHashMap::default()))),
            vfs_config_version: 0,
            vfs_progress_config_version: 0,
            vfs_progress_n_total: 0,
            vfs_progress_n_done: 0,

            workspaces: Arc::new(Vec::new()),
            fetch_workspaces_queue: OpQueue::default(),
            prime_caches_queue: OpQueue::default(),

            fetch_build_data_queue: OpQueue::default(),
            latest_requests: Default::default(),
        };
        // Apply any required database inputs from the config.
        this.update_configuration(config);
        this
    }

    pub(crate) fn process_changes(&mut self) -> bool {
        let _p = profile::span("GlobalState::process_changes");
        let mut fs_changes = Vec::new();
        let mut has_fs_changes = false;

        let change = {
            let mut change = Change::new();
            let (vfs, line_endings_map) = &mut *self.vfs.write();
            let changed_files = vfs.take_changes();
            if changed_files.is_empty() {
                return false;
            }

            for file in changed_files {
                if file.is_created_or_deleted() {
                    if let Some(path) = vfs.file_path(file.file_id).as_path() {
                        fs_changes.push((path.to_path_buf(), file.change_kind));
                        has_fs_changes = true;
                    }
                }

                let text = if file.exists() {
                    let bytes = vfs.file_contents(file.file_id).to_vec();
                    match String::from_utf8(bytes).ok() {
                        Some(text) => {
                            let (text, line_endings) = LineEndings::normalize(text);
                            line_endings_map.insert(file.file_id, line_endings);
                            Some(Arc::new(text))
                        }
                        None => None,
                    }
                } else {
                    None
                };
                change.change_file(file.file_id, text);
            }
            if has_fs_changes {
                let roots = self.source_root_config.partition(vfs);
                change.set_roots(roots);
            }
            change
        };

        self.analysis_host.apply_change(change);
        self.maybe_refresh(&fs_changes);
        true
    }

    pub(crate) fn snapshot(&self) -> GlobalStateSnapshot {
        GlobalStateSnapshot {
            config: Arc::clone(&self.config),
            workspaces: Arc::clone(&self.workspaces),
            analysis: self.analysis_host.analysis(),
            vfs: Arc::clone(&self.vfs),
            latest_requests: Arc::clone(&self.latest_requests),
            check_fixes: Arc::clone(&self.diagnostics.check_fixes),
            mem_docs: self.mem_docs.clone(),
            semantic_tokens_cache: Arc::clone(&self.semantic_tokens_cache),
        }
    }

    pub(crate) fn send_request<R: lsp_types::request::Request>(
        &mut self,
        params: R::Params,
        handler: ReqHandler,
    ) {
        let request = self.req_queue.outgoing.register(R::METHOD.to_string(), params, handler);
        self.send(request.into());
    }
    pub(crate) fn complete_request(&mut self, response: lsp_server::Response) {
        let handler = self.req_queue.outgoing.complete(response.id.clone());
        handler(self, response)
    }

    pub(crate) fn send_notification<N: lsp_types::notification::Notification>(
        &mut self,
        params: N::Params,
    ) {
        let not = lsp_server::Notification::new(N::METHOD.to_string(), params);
        self.send(not.into());
    }

    pub(crate) fn register_request(
        &mut self,
        request: &lsp_server::Request,
        request_received: Instant,
    ) {
        self.req_queue
            .incoming
            .register(request.id.clone(), (request.method.clone(), request_received));
    }
    pub(crate) fn respond(&mut self, response: lsp_server::Response) {
        if let Some((method, start)) = self.req_queue.incoming.complete(response.id.clone()) {
            let duration = start.elapsed();
            log::info!("handled req#{} in {:?}", response.id, duration);
            let metrics = RequestMetrics { id: response.id.clone(), method, duration };
            self.latest_requests.write().record(metrics);
            self.send(response.into());
        }
    }
    pub(crate) fn cancel(&mut self, request_id: lsp_server::RequestId) {
        if let Some(response) = self.req_queue.incoming.cancel(request_id) {
            self.send(response.into());
        }
    }

    fn send(&mut self, message: lsp_server::Message) {
        self.sender.send(message).unwrap()
    }
}

impl Drop for GlobalState {
    fn drop(&mut self) {
        self.analysis_host.request_cancellation()
    }
}

impl GlobalStateSnapshot {
    pub(crate) fn url_to_file_id(&self, url: &Url) -> Result<FileId> {
        url_to_file_id(&self.vfs.read().0, url)
    }

    pub(crate) fn file_id_to_url(&self, id: FileId) -> Url {
        file_id_to_url(&self.vfs.read().0, id)
    }

    pub(crate) fn file_line_index(&self, file_id: FileId) -> Cancellable<LineIndex> {
        let endings = self.vfs.read().1[&file_id];
        let index = self.analysis.file_line_index(file_id)?;
        let res = LineIndex { index, endings, encoding: self.config.offset_encoding() };
        Ok(res)
    }

    pub(crate) fn url_file_version(&self, url: &Url) -> Option<i32> {
        let path = from_proto::vfs_path(url).ok()?;
        Some(self.mem_docs.get(&path)?.version)
    }

    pub(crate) fn anchored_path(&self, path: &AnchoredPathBuf) -> Url {
        let mut base = self.vfs.read().0.file_path(path.anchor);
        base.pop();
        let path = base.join(&path.path).unwrap();
        let path = path.as_path().unwrap();
        url_from_abs_path(path)
    }

    pub(crate) fn cargo_target_for_crate_root(
        &self,
        crate_id: CrateId,
    ) -> Option<(&CargoWorkspace, Target)> {
        let file_id = self.analysis.crate_root(crate_id).ok()?;
        let path = self.vfs.read().0.file_path(file_id);
        let path = path.as_path()?;
        self.workspaces.iter().find_map(|ws| match ws {
            ProjectWorkspace::Cargo { cargo, .. } => {
                cargo.target_by_root(path).map(|it| (cargo, it))
            }
            ProjectWorkspace::Json { .. } => None,
            ProjectWorkspace::DetachedFiles { .. } => None,
        })
    }
}

pub(crate) fn file_id_to_url(vfs: &vfs::Vfs, id: FileId) -> Url {
    let path = vfs.file_path(id);
    let path = path.as_path().unwrap();
    url_from_abs_path(path)
}

pub(crate) fn url_to_file_id(vfs: &vfs::Vfs, url: &Url) -> Result<FileId> {
    let path = from_proto::vfs_path(url)?;
    let res = vfs.file_id(&path).ok_or_else(|| format!("file not found: {}", path))?;
    Ok(res)
}
