//! Config used by the language server.
//!
//! We currently get this config from `initialize` LSP request, which is not the
//! best way to do it, but was the simplest thing we could implement.
//!
//! Of particular interest is the `feature_flags` hash map: while other fields
//! configure the server itself, feature flags are passed into analysis, and
//! tweak things like automatic insertion of `()` in completions.

use std::{ffi::OsString, iter, path::PathBuf};

use flycheck::FlycheckConfig;
use ide::{
    AssistConfig, CompletionConfig, DiagnosticsConfig, HoverConfig, HoverDocFormat,
    InlayHintsConfig, JoinLinesConfig,
};
use ide_db::helpers::{
    insert_use::{ImportGranularity, InsertUseConfig, PrefixKind},
    SnippetCap,
};
use lsp_types::{ClientCapabilities, MarkupKind};
use project_model::{CargoConfig, ProjectJson, ProjectJsonData, ProjectManifest, RustcSource};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{de::DeserializeOwned, Deserialize};
use vfs::AbsPathBuf;

use crate::{
    caps::completion_item_edit_resolve, diagnostics::DiagnosticsMapConfig,
    line_index::OffsetEncoding, lsp_ext::supports_utf8, lsp_ext::WorkspaceSymbolSearchKind,
    lsp_ext::WorkspaceSymbolSearchScope,
};

// Defines the server-side configuration of the rust-analyzer. We generate
// *parts* of VS Code's `package.json` config from this.
//
// However, editor specific config, which the server doesn't know about, should
// be specified directly in `package.json`.
//
// To deprecate an option by replacing it with another name use `new_name | `old_name` so that we keep
// parsing the old name.
config_data! {
    struct ConfigData {
        /// How imports should be grouped into use statements.
        assist_importGranularity |
        assist_importMergeBehavior |
        assist_importMergeBehaviour: ImportGranularityDef  = "\"crate\"",
        /// Whether to enforce the import granularity setting for all files. If set to false rust-analyzer will try to keep import styles consistent per file.
        assist_importEnforceGranularity: bool              = "false",
        /// The path structure for newly inserted paths to use.
        assist_importPrefix: ImportPrefixDef               = "\"plain\"",
        /// Group inserted imports by the [following order](https://rust-analyzer.github.io/manual.html#auto-import). Groups are separated by newlines.
        assist_importGroup: bool                           = "true",
        /// Whether to allow import insertion to merge new imports into single path glob imports like `use std::fmt::*;`.
        assist_allowMergingIntoGlobImports: bool           = "true",

        /// Show function name and docs in parameter hints.
        callInfo_full: bool                                = "true",

        /// Automatically refresh project info via `cargo metadata` on
        /// `Cargo.toml` changes.
        cargo_autoreload: bool           = "true",
        /// Activate all available features (`--all-features`).
        cargo_allFeatures: bool          = "false",
        /// Unsets `#[cfg(test)]` for the specified crates.
        cargo_unsetTest: Vec<String>   = "[\"core\"]",
        /// List of features to activate.
        cargo_features: Vec<String>      = "[]",
        /// Run build scripts (`build.rs`) for more precise code analysis.
        cargo_runBuildScripts |
        cargo_loadOutDirsFromCheck: bool = "true",
        /// Use `RUSTC_WRAPPER=rust-analyzer` when running build scripts to
        /// avoid compiling unnecessary things.
        cargo_useRustcWrapperForBuildScripts: bool = "true",
        /// Do not activate the `default` feature.
        cargo_noDefaultFeatures: bool    = "false",
        /// Compilation target (target triple).
        cargo_target: Option<String>     = "null",
        /// Internal config for debugging, disables loading of sysroot crates.
        cargo_noSysroot: bool            = "false",

        /// Run specified `cargo check` command for diagnostics on save.
        checkOnSave_enable: bool                         = "true",
        /// Check with all features (`--all-features`).
        /// Defaults to `#rust-analyzer.cargo.allFeatures#`.
        checkOnSave_allFeatures: Option<bool>            = "null",
        /// Check all targets and tests (`--all-targets`).
        checkOnSave_allTargets: bool                     = "true",
        /// Cargo command to use for `cargo check`.
        checkOnSave_command: String                      = "\"check\"",
        /// Do not activate the `default` feature.
        checkOnSave_noDefaultFeatures: Option<bool>      = "null",
        /// Check for a specific target. Defaults to
        /// `#rust-analyzer.cargo.target#`.
        checkOnSave_target: Option<String>               = "null",
        /// Extra arguments for `cargo check`.
        checkOnSave_extraArgs: Vec<String>               = "[]",
        /// List of features to activate. Defaults to
        /// `#rust-analyzer.cargo.features#`.
        checkOnSave_features: Option<Vec<String>>        = "null",
        /// Advanced option, fully override the command rust-analyzer uses for
        /// checking. The command should include `--message-format=json` or
        /// similar option.
        checkOnSave_overrideCommand: Option<Vec<String>> = "null",

        /// Whether to add argument snippets when completing functions.
        /// Only applies when `#rust-analyzer.completion.addCallParenthesis#` is set.
        completion_addCallArgumentSnippets: bool = "true",
        /// Whether to add parenthesis when completing functions.
        completion_addCallParenthesis: bool      = "true",
        /// Whether to show postfix snippets like `dbg`, `if`, `not`, etc.
        completion_postfix_enable: bool          = "true",
        /// Toggles the additional completions that automatically add imports when completed.
        /// Note that your client must specify the `additionalTextEdits` LSP client capability to truly have this feature enabled.
        completion_autoimport_enable: bool       = "true",
        /// Toggles the additional completions that automatically show method calls and field accesses
        /// with `self` prefixed to them when inside a method.
        completion_autoself_enable: bool       = "true",

        /// Whether to show native rust-analyzer diagnostics.
        diagnostics_enable: bool                = "true",
        /// Whether to show experimental rust-analyzer diagnostics that might
        /// have more false positives than usual.
        diagnostics_enableExperimental: bool    = "true",
        /// List of rust-analyzer diagnostics to disable.
        diagnostics_disabled: FxHashSet<String> = "[]",
        /// Map of prefixes to be substituted when parsing diagnostic file paths.
        /// This should be the reverse mapping of what is passed to `rustc` as `--remap-path-prefix`.
        diagnostics_remapPrefix: FxHashMap<String, String> = "{}",
        /// List of warnings that should be displayed with hint severity.
        ///
        /// The warnings will be indicated by faded text or three dots in code
        /// and will not show up in the `Problems Panel`.
        diagnostics_warningsAsHint: Vec<String> = "[]",
        /// List of warnings that should be displayed with info severity.
        ///
        /// The warnings will be indicated by a blue squiggly underline in code
        /// and a blue icon in the `Problems Panel`.
        diagnostics_warningsAsInfo: Vec<String> = "[]",

        /// Expand attribute macros.
        experimental_procAttrMacros: bool = "false",

        /// Controls file watching implementation.
        files_watcher: String = "\"client\"",
        /// These directories will be ignored by rust-analyzer.
        files_excludeDirs: Vec<PathBuf> = "[]",

        /// Use semantic tokens for strings.
        ///
        /// In some editors (e.g. vscode) semantic tokens override other highlighting grammars.
        /// By disabling semantic tokens for strings, other grammars can be used to highlight
        /// their contents.
        highlighting_strings: bool = "true",

        /// Whether to show documentation on hover.
        hover_documentation: bool       = "true",
        /// Use markdown syntax for links in hover.
        hover_linksInHover |
        hoverActions_linksInHover: bool = "true",

        /// Whether to show `Debug` action. Only applies when
        /// `#rust-analyzer.hoverActions.enable#` is set.
        hoverActions_debug: bool           = "true",
        /// Whether to show HoverActions in Rust files.
        hoverActions_enable: bool          = "true",
        /// Whether to show `Go to Type Definition` action. Only applies when
        /// `#rust-analyzer.hoverActions.enable#` is set.
        hoverActions_gotoTypeDef: bool     = "true",
        /// Whether to show `Implementations` action. Only applies when
        /// `#rust-analyzer.hoverActions.enable#` is set.
        hoverActions_implementations: bool = "true",
        /// Whether to show `References` action. Only applies when
        /// `#rust-analyzer.hoverActions.enable#` is set.
        hoverActions_references: bool      = "false",
        /// Whether to show `Run` action. Only applies when
        /// `#rust-analyzer.hoverActions.enable#` is set.
        hoverActions_run: bool             = "true",

        /// Whether to show inlay type hints for method chains.
        inlayHints_chainingHints: bool      = "true",
        /// Maximum length for inlay hints. Set to null to have an unlimited length.
        inlayHints_maxLength: Option<usize> = "25",
        /// Whether to show function parameter name inlay hints at the call
        /// site.
        inlayHints_parameterHints: bool     = "true",
        /// Whether to show inlay type hints for variables.
        inlayHints_typeHints: bool          = "true",

        /// Join lines inserts else between consecutive ifs.
        joinLines_joinElseIf: bool = "true",
        /// Join lines removes trailing commas.
        joinLines_removeTrailingComma: bool = "true",
        /// Join lines unwraps trivial blocks.
        joinLines_unwrapTrivialBlock: bool = "true",

        /// Whether to show `Debug` lens. Only applies when
        /// `#rust-analyzer.lens.enable#` is set.
        lens_debug: bool            = "true",
        /// Whether to show CodeLens in Rust files.
        lens_enable: bool           = "true",
        /// Whether to show `Implementations` lens. Only applies when
        /// `#rust-analyzer.lens.enable#` is set.
        lens_implementations: bool  = "true",
        /// Whether to show `Run` lens. Only applies when
        /// `#rust-analyzer.lens.enable#` is set.
        lens_run: bool              = "true",
        /// Whether to show `Method References` lens. Only applies when
        /// `#rust-analyzer.lens.enable#` is set.
        lens_methodReferences: bool = "false",
        /// Whether to show `References` lens. Only applies when
        /// `#rust-analyzer.lens.enable#` is set.
        lens_references: bool = "false",

        /// Disable project auto-discovery in favor of explicitly specified set
        /// of projects.
        ///
        /// Elements must be paths pointing to `Cargo.toml`,
        /// `rust-project.json`, or JSON objects in `rust-project.json` format.
        linkedProjects: Vec<ManifestOrProjectJson> = "[]",

        /// Number of syntax trees rust-analyzer keeps in memory. Defaults to 128.
        lruCapacity: Option<usize>                 = "null",

        /// Whether to show `can't find Cargo.toml` error message.
        notifications_cargoTomlNotFound: bool      = "true",

        /// Enable support for procedural macros, implies `#rust-analyzer.cargo.runBuildScripts#`.
        procMacro_enable: bool                     = "true",
        /// Internal config, path to proc-macro server executable (typically,
        /// this is rust-analyzer itself, but we override this in tests).
        procMacro_server: Option<PathBuf>          = "null",

        /// Command to be executed instead of 'cargo' for runnables.
        runnables_overrideCargo: Option<String> = "null",
        /// Additional arguments to be passed to cargo for runnables such as
        /// tests or binaries. For example, it may be `--release`.
        runnables_cargoExtraArgs: Vec<String>   = "[]",

        /// Path to the Cargo.toml of the rust compiler workspace, for usage in rustc_private
        /// projects, or "discover" to try to automatically find it.
        ///
        /// Any project which uses rust-analyzer with the rustcPrivate
        /// crates must set `[package.metadata.rust-analyzer] rustc_private=true` to use it.
        ///
        /// This option is not reloaded automatically; you must restart rust-analyzer for it to take effect.
        rustcSource: Option<String> = "null",

        /// Additional arguments to `rustfmt`.
        rustfmt_extraArgs: Vec<String>               = "[]",
        /// Advanced option, fully override the command rust-analyzer uses for
        /// formatting.
        rustfmt_overrideCommand: Option<Vec<String>> = "null",
        /// Enables the use of rustfmt's unstable range formatting command for the
        /// `textDocument/rangeFormatting` request. The rustfmt option is unstable and only
        /// available on a nightly build.
        rustfmt_enableRangeFormatting: bool = "false",

        /// Workspace symbol search scope.
        workspace_symbol_search_scope: WorskpaceSymbolSearchScopeDef = "\"workspace\"",
        /// Workspace symbol search kind.
        workspace_symbol_search_kind: WorskpaceSymbolSearchKindDef = "\"only_types\"",
    }
}

impl Default for ConfigData {
    fn default() -> Self {
        ConfigData::from_json(serde_json::Value::Null)
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub caps: lsp_types::ClientCapabilities,
    data: ConfigData,
    detached_files: Vec<AbsPathBuf>,
    pub discovered_projects: Option<Vec<ProjectManifest>>,
    pub root_path: AbsPathBuf,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum LinkedProject {
    ProjectManifest(ProjectManifest),
    InlineJsonProject(ProjectJson),
}

impl From<ProjectManifest> for LinkedProject {
    fn from(v: ProjectManifest) -> Self {
        LinkedProject::ProjectManifest(v)
    }
}

impl From<ProjectJson> for LinkedProject {
    fn from(v: ProjectJson) -> Self {
        LinkedProject::InlineJsonProject(v)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LensConfig {
    pub run: bool,
    pub debug: bool,
    pub implementations: bool,
    pub method_refs: bool,
    pub refs: bool, // for Struct, Enum, Union and Trait
}

impl LensConfig {
    pub fn any(&self) -> bool {
        self.implementations || self.runnable() || self.references()
    }

    pub fn none(&self) -> bool {
        !self.any()
    }

    pub fn runnable(&self) -> bool {
        self.run || self.debug
    }

    pub fn references(&self) -> bool {
        self.method_refs || self.refs
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoverActionsConfig {
    pub implementations: bool,
    pub references: bool,
    pub run: bool,
    pub debug: bool,
    pub goto_type_def: bool,
}

impl HoverActionsConfig {
    pub const NO_ACTIONS: Self = Self {
        implementations: false,
        references: false,
        run: false,
        debug: false,
        goto_type_def: false,
    };

    pub fn any(&self) -> bool {
        self.implementations || self.references || self.runnable() || self.goto_type_def
    }

    pub fn none(&self) -> bool {
        !self.any()
    }

    pub fn runnable(&self) -> bool {
        self.run || self.debug
    }
}

#[derive(Debug, Clone)]
pub struct FilesConfig {
    pub watcher: FilesWatcher,
    pub exclude: Vec<AbsPathBuf>,
}

#[derive(Debug, Clone)]
pub enum FilesWatcher {
    Client,
    Notify,
}

#[derive(Debug, Clone)]
pub struct NotificationsConfig {
    pub cargo_toml_not_found: bool,
}

#[derive(Debug, Clone)]
pub enum RustfmtConfig {
    Rustfmt { extra_args: Vec<String>, enable_range_formatting: bool },
    CustomCommand { command: String, args: Vec<String> },
}

/// Configuration for runnable items, such as `main` function or tests.
#[derive(Debug, Clone)]
pub struct RunnablesConfig {
    /// Custom command to be executed instead of `cargo` for runnables.
    pub override_cargo: Option<String>,
    /// Additional arguments for the `cargo`, e.g. `--release`.
    pub cargo_extra_args: Vec<String>,
}

/// Configuration for workspace symbol search requests.
#[derive(Debug, Clone)]
pub struct WorkspaceSymbolConfig {
    /// In what scope should the symbol be searched in.
    pub search_scope: WorkspaceSymbolSearchScope,
    /// What kind of symbol is being search for.
    pub search_kind: WorkspaceSymbolSearchKind,
}

impl Config {
    pub fn new(root_path: AbsPathBuf, caps: ClientCapabilities) -> Self {
        Config {
            caps,
            data: ConfigData::default(),
            detached_files: Vec::new(),
            discovered_projects: None,
            root_path,
        }
    }
    pub fn update(&mut self, mut json: serde_json::Value) {
        log::info!("updating config from JSON: {:#}", json);
        if json.is_null() || json.as_object().map_or(false, |it| it.is_empty()) {
            return;
        }
        self.detached_files = get_field::<Vec<PathBuf>>(&mut json, "detachedFiles", None, "[]")
            .into_iter()
            .map(AbsPathBuf::assert)
            .collect();
        self.data = ConfigData::from_json(json);
    }

    pub fn json_schema() -> serde_json::Value {
        ConfigData::json_schema()
    }
}

macro_rules! try_ {
    ($expr:expr) => {
        || -> _ { Some($expr) }()
    };
}
macro_rules! try_or {
    ($expr:expr, $or:expr) => {
        try_!($expr).unwrap_or($or)
    };
}

impl Config {
    pub fn linked_projects(&self) -> Vec<LinkedProject> {
        if self.data.linkedProjects.is_empty() {
            self.discovered_projects
                .as_ref()
                .into_iter()
                .flatten()
                .cloned()
                .map(LinkedProject::from)
                .collect()
        } else {
            self.data
                .linkedProjects
                .iter()
                .filter_map(|linked_project| {
                    let res = match linked_project {
                        ManifestOrProjectJson::Manifest(it) => {
                            let path = self.root_path.join(it);
                            ProjectManifest::from_manifest_file(path)
                                .map_err(|e| log::error!("failed to load linked project: {}", e))
                                .ok()?
                                .into()
                        }
                        ManifestOrProjectJson::ProjectJson(it) => {
                            ProjectJson::new(&self.root_path, it.clone()).into()
                        }
                    };
                    Some(res)
                })
                .collect()
        }
    }

    pub fn detached_files(&self) -> &[AbsPathBuf] {
        &self.detached_files
    }

    pub fn did_save_text_document_dynamic_registration(&self) -> bool {
        let caps =
            try_or!(self.caps.text_document.as_ref()?.synchronization.clone()?, Default::default());
        caps.did_save == Some(true) && caps.dynamic_registration == Some(true)
    }
    pub fn did_change_watched_files_dynamic_registration(&self) -> bool {
        try_or!(
            self.caps.workspace.as_ref()?.did_change_watched_files.as_ref()?.dynamic_registration?,
            false
        )
    }

    pub fn location_link(&self) -> bool {
        try_or!(self.caps.text_document.as_ref()?.definition?.link_support?, false)
    }
    pub fn line_folding_only(&self) -> bool {
        try_or!(self.caps.text_document.as_ref()?.folding_range.as_ref()?.line_folding_only?, false)
    }
    pub fn hierarchical_symbols(&self) -> bool {
        try_or!(
            self.caps
                .text_document
                .as_ref()?
                .document_symbol
                .as_ref()?
                .hierarchical_document_symbol_support?,
            false
        )
    }
    pub fn code_action_literals(&self) -> bool {
        try_!(self
            .caps
            .text_document
            .as_ref()?
            .code_action
            .as_ref()?
            .code_action_literal_support
            .as_ref()?)
        .is_some()
    }
    pub fn work_done_progress(&self) -> bool {
        try_or!(self.caps.window.as_ref()?.work_done_progress?, false)
    }
    pub fn will_rename(&self) -> bool {
        try_or!(self.caps.workspace.as_ref()?.file_operations.as_ref()?.will_rename?, false)
    }
    pub fn change_annotation_support(&self) -> bool {
        try_!(self
            .caps
            .workspace
            .as_ref()?
            .workspace_edit
            .as_ref()?
            .change_annotation_support
            .as_ref()?)
        .is_some()
    }
    pub fn code_action_resolve(&self) -> bool {
        try_or!(
            self.caps
                .text_document
                .as_ref()?
                .code_action
                .as_ref()?
                .resolve_support
                .as_ref()?
                .properties
                .as_slice(),
            &[]
        )
        .iter()
        .any(|it| it == "edit")
    }
    pub fn signature_help_label_offsets(&self) -> bool {
        try_or!(
            self.caps
                .text_document
                .as_ref()?
                .signature_help
                .as_ref()?
                .signature_information
                .as_ref()?
                .parameter_information
                .as_ref()?
                .label_offset_support?,
            false
        )
    }
    pub fn offset_encoding(&self) -> OffsetEncoding {
        if supports_utf8(&self.caps) {
            OffsetEncoding::Utf8
        } else {
            OffsetEncoding::Utf16
        }
    }

    fn experimental(&self, index: &'static str) -> bool {
        try_or!(self.caps.experimental.as_ref()?.get(index)?.as_bool()?, false)
    }
    pub fn code_action_group(&self) -> bool {
        self.experimental("codeActionGroup")
    }
    pub fn experimental_hover_actions(&self) -> bool {
        self.experimental("hoverActions")
    }
    pub fn server_status_notification(&self) -> bool {
        self.experimental("serverStatusNotification")
    }

    pub fn publish_diagnostics(&self) -> bool {
        self.data.diagnostics_enable
    }
    pub fn diagnostics(&self) -> DiagnosticsConfig {
        DiagnosticsConfig {
            disable_experimental: !self.data.diagnostics_enableExperimental,
            disabled: self.data.diagnostics_disabled.clone(),
        }
    }
    pub fn diagnostics_map(&self) -> DiagnosticsMapConfig {
        DiagnosticsMapConfig {
            remap_prefix: self.data.diagnostics_remapPrefix.clone(),
            warnings_as_info: self.data.diagnostics_warningsAsInfo.clone(),
            warnings_as_hint: self.data.diagnostics_warningsAsHint.clone(),
        }
    }
    pub fn lru_capacity(&self) -> Option<usize> {
        self.data.lruCapacity
    }
    pub fn proc_macro_srv(&self) -> Option<(PathBuf, Vec<OsString>)> {
        if !self.data.procMacro_enable {
            return None;
        }

        let path = self.data.procMacro_server.clone().or_else(|| std::env::current_exe().ok())?;
        Some((path, vec!["proc-macro".into()]))
    }
    pub fn expand_proc_attr_macros(&self) -> bool {
        self.data.experimental_procAttrMacros
    }
    pub fn files(&self) -> FilesConfig {
        FilesConfig {
            watcher: match self.data.files_watcher.as_str() {
                "notify" => FilesWatcher::Notify,
                "client" | _ => FilesWatcher::Client,
            },
            exclude: self.data.files_excludeDirs.iter().map(|it| self.root_path.join(it)).collect(),
        }
    }
    pub fn notifications(&self) -> NotificationsConfig {
        NotificationsConfig { cargo_toml_not_found: self.data.notifications_cargoTomlNotFound }
    }
    pub fn cargo_autoreload(&self) -> bool {
        self.data.cargo_autoreload
    }
    pub fn run_build_scripts(&self) -> bool {
        self.data.cargo_runBuildScripts || self.data.procMacro_enable
    }
    pub fn wrap_rustc(&self) -> bool {
        self.data.cargo_useRustcWrapperForBuildScripts
    }
    pub fn cargo(&self) -> CargoConfig {
        let rustc_source = self.data.rustcSource.as_ref().map(|rustc_src| {
            if rustc_src == "discover" {
                RustcSource::Discover
            } else {
                RustcSource::Path(self.root_path.join(rustc_src))
            }
        });

        CargoConfig {
            no_default_features: self.data.cargo_noDefaultFeatures,
            all_features: self.data.cargo_allFeatures,
            features: self.data.cargo_features.clone(),
            target: self.data.cargo_target.clone(),
            rustc_source,
            no_sysroot: self.data.cargo_noSysroot,
            unset_test_crates: self.data.cargo_unsetTest.clone(),
        }
    }

    pub fn rustfmt(&self) -> RustfmtConfig {
        match &self.data.rustfmt_overrideCommand {
            Some(args) if !args.is_empty() => {
                let mut args = args.clone();
                let command = args.remove(0);
                RustfmtConfig::CustomCommand { command, args }
            }
            Some(_) | None => RustfmtConfig::Rustfmt {
                extra_args: self.data.rustfmt_extraArgs.clone(),
                enable_range_formatting: self.data.rustfmt_enableRangeFormatting,
            },
        }
    }
    pub fn flycheck(&self) -> Option<FlycheckConfig> {
        if !self.data.checkOnSave_enable {
            return None;
        }
        let flycheck_config = match &self.data.checkOnSave_overrideCommand {
            Some(args) if !args.is_empty() => {
                let mut args = args.clone();
                let command = args.remove(0);
                FlycheckConfig::CustomCommand { command, args }
            }
            Some(_) | None => FlycheckConfig::CargoCommand {
                command: self.data.checkOnSave_command.clone(),
                target_triple: self
                    .data
                    .checkOnSave_target
                    .clone()
                    .or_else(|| self.data.cargo_target.clone()),
                all_targets: self.data.checkOnSave_allTargets,
                no_default_features: self
                    .data
                    .checkOnSave_noDefaultFeatures
                    .unwrap_or(self.data.cargo_noDefaultFeatures),
                all_features: self
                    .data
                    .checkOnSave_allFeatures
                    .unwrap_or(self.data.cargo_allFeatures),
                features: self
                    .data
                    .checkOnSave_features
                    .clone()
                    .unwrap_or_else(|| self.data.cargo_features.clone()),
                extra_args: self.data.checkOnSave_extraArgs.clone(),
            },
        };
        Some(flycheck_config)
    }
    pub fn runnables(&self) -> RunnablesConfig {
        RunnablesConfig {
            override_cargo: self.data.runnables_overrideCargo.clone(),
            cargo_extra_args: self.data.runnables_cargoExtraArgs.clone(),
        }
    }
    pub fn inlay_hints(&self) -> InlayHintsConfig {
        InlayHintsConfig {
            type_hints: self.data.inlayHints_typeHints,
            parameter_hints: self.data.inlayHints_parameterHints,
            chaining_hints: self.data.inlayHints_chainingHints,
            max_length: self.data.inlayHints_maxLength,
        }
    }
    fn insert_use_config(&self) -> InsertUseConfig {
        InsertUseConfig {
            granularity: match self.data.assist_importGranularity {
                ImportGranularityDef::Preserve => ImportGranularity::Preserve,
                ImportGranularityDef::Item => ImportGranularity::Item,
                ImportGranularityDef::Crate => ImportGranularity::Crate,
                ImportGranularityDef::Module => ImportGranularity::Module,
            },
            enforce_granularity: self.data.assist_importEnforceGranularity,
            prefix_kind: match self.data.assist_importPrefix {
                ImportPrefixDef::Plain => PrefixKind::Plain,
                ImportPrefixDef::ByCrate => PrefixKind::ByCrate,
                ImportPrefixDef::BySelf => PrefixKind::BySelf,
            },
            group: self.data.assist_importGroup,
            skip_glob_imports: !self.data.assist_allowMergingIntoGlobImports,
        }
    }
    pub fn completion(&self) -> CompletionConfig {
        CompletionConfig {
            enable_postfix_completions: self.data.completion_postfix_enable,
            enable_imports_on_the_fly: self.data.completion_autoimport_enable
                && completion_item_edit_resolve(&self.caps),
            enable_self_on_the_fly: self.data.completion_autoself_enable,
            add_call_parenthesis: self.data.completion_addCallParenthesis,
            add_call_argument_snippets: self.data.completion_addCallArgumentSnippets,
            insert_use: self.insert_use_config(),
            snippet_cap: SnippetCap::new(try_or!(
                self.caps
                    .text_document
                    .as_ref()?
                    .completion
                    .as_ref()?
                    .completion_item
                    .as_ref()?
                    .snippet_support?,
                false
            )),
        }
    }
    pub fn assist(&self) -> AssistConfig {
        AssistConfig {
            snippet_cap: SnippetCap::new(self.experimental("snippetTextEdit")),
            allowed: None,
            insert_use: self.insert_use_config(),
        }
    }
    pub fn join_lines(&self) -> JoinLinesConfig {
        JoinLinesConfig {
            join_else_if: self.data.joinLines_joinElseIf,
            remove_trailing_comma: self.data.joinLines_removeTrailingComma,
            unwrap_trivial_blocks: self.data.joinLines_unwrapTrivialBlock,
        }
    }
    pub fn call_info_full(&self) -> bool {
        self.data.callInfo_full
    }
    pub fn lens(&self) -> LensConfig {
        LensConfig {
            run: self.data.lens_enable && self.data.lens_run,
            debug: self.data.lens_enable && self.data.lens_debug,
            implementations: self.data.lens_enable && self.data.lens_implementations,
            method_refs: self.data.lens_enable && self.data.lens_methodReferences,
            refs: self.data.lens_enable && self.data.lens_references,
        }
    }
    pub fn hover_actions(&self) -> HoverActionsConfig {
        HoverActionsConfig {
            implementations: self.data.hoverActions_enable
                && self.data.hoverActions_implementations,
            references: self.data.hoverActions_enable && self.data.hoverActions_references,
            run: self.data.hoverActions_enable && self.data.hoverActions_run,
            debug: self.data.hoverActions_enable && self.data.hoverActions_debug,
            goto_type_def: self.data.hoverActions_enable && self.data.hoverActions_gotoTypeDef,
        }
    }
    pub fn highlighting_strings(&self) -> bool {
        self.data.highlighting_strings
    }
    pub fn hover(&self) -> HoverConfig {
        HoverConfig {
            links_in_hover: self.data.hover_linksInHover,
            documentation: self.data.hover_documentation.then(|| {
                let is_markdown = try_or!(
                    self.caps
                        .text_document
                        .as_ref()?
                        .hover
                        .as_ref()?
                        .content_format
                        .as_ref()?
                        .as_slice(),
                    &[]
                )
                .contains(&MarkupKind::Markdown);
                if is_markdown {
                    HoverDocFormat::Markdown
                } else {
                    HoverDocFormat::PlainText
                }
            }),
        }
    }

    pub fn workspace_symbol(&self) -> WorkspaceSymbolConfig {
        WorkspaceSymbolConfig {
            search_scope: match self.data.workspace_symbol_search_scope {
                WorskpaceSymbolSearchScopeDef::Workspace => WorkspaceSymbolSearchScope::Workspace,
                WorskpaceSymbolSearchScopeDef::WorkspaceAndDependencies => {
                    WorkspaceSymbolSearchScope::WorkspaceAndDependencies
                }
            },
            search_kind: match self.data.workspace_symbol_search_kind {
                WorskpaceSymbolSearchKindDef::OnlyTypes => WorkspaceSymbolSearchKind::OnlyTypes,
                WorskpaceSymbolSearchKindDef::AllSymbols => WorkspaceSymbolSearchKind::AllSymbols,
            },
        }
    }

    pub fn semantic_tokens_refresh(&self) -> bool {
        try_or!(self.caps.workspace.as_ref()?.semantic_tokens.as_ref()?.refresh_support?, false)
    }
    pub fn code_lens_refresh(&self) -> bool {
        try_or!(self.caps.workspace.as_ref()?.code_lens.as_ref()?.refresh_support?, false)
    }
    pub fn insert_replace_support(&self) -> bool {
        try_or!(
            self.caps
                .text_document
                .as_ref()?
                .completion
                .as_ref()?
                .completion_item
                .as_ref()?
                .insert_replace_support?,
            false
        )
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
enum ManifestOrProjectJson {
    Manifest(PathBuf),
    ProjectJson(ProjectJsonData),
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
enum ImportGranularityDef {
    Preserve,
    #[serde(alias = "none")]
    Item,
    #[serde(alias = "full")]
    Crate,
    #[serde(alias = "last")]
    Module,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
enum ImportPrefixDef {
    Plain,
    #[serde(alias = "self")]
    BySelf,
    #[serde(alias = "crate")]
    ByCrate,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
enum WorskpaceSymbolSearchScopeDef {
    Workspace,
    WorkspaceAndDependencies,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
enum WorskpaceSymbolSearchKindDef {
    OnlyTypes,
    AllSymbols,
}

macro_rules! _config_data {
    (struct $name:ident {
        $(
            $(#[doc=$doc:literal])*
            $field:ident $(| $alias:ident)*: $ty:ty = $default:expr,
        )*
    }) => {
        #[allow(non_snake_case)]
        #[derive(Debug, Clone)]
        struct $name { $($field: $ty,)* }
        impl $name {
            fn from_json(mut json: serde_json::Value) -> $name {
                $name {$(
                    $field: get_field(
                        &mut json,
                        stringify!($field),
                        None$(.or(Some(stringify!($alias))))*,
                        $default,
                    ),
                )*}
            }

            fn json_schema() -> serde_json::Value {
                schema(&[
                    $({
                        let field = stringify!($field);
                        let ty = stringify!($ty);

                        (field, ty, &[$($doc),*], $default)
                    },)*
                ])
            }

            #[cfg(test)]
            fn manual() -> String {
                manual(&[
                    $({
                        let field = stringify!($field);
                        let ty = stringify!($ty);

                        (field, ty, &[$($doc),*], $default)
                    },)*
                ])
            }
        }
    };
}
use _config_data as config_data;

fn get_field<T: DeserializeOwned>(
    json: &mut serde_json::Value,
    field: &'static str,
    alias: Option<&'static str>,
    default: &str,
) -> T {
    let default = serde_json::from_str(default).unwrap();

    // XXX: check alias first, to work-around the VS Code where it pre-fills the
    // defaults instead of sending an empty object.
    alias
        .into_iter()
        .chain(iter::once(field))
        .find_map(move |field| {
            let mut pointer = field.replace('_', "/");
            pointer.insert(0, '/');
            json.pointer_mut(&pointer).and_then(|it| serde_json::from_value(it.take()).ok())
        })
        .unwrap_or(default)
}

fn schema(fields: &[(&'static str, &'static str, &[&str], &str)]) -> serde_json::Value {
    for ((f1, ..), (f2, ..)) in fields.iter().zip(&fields[1..]) {
        fn key(f: &str) -> &str {
            f.splitn(2, '_').next().unwrap()
        }
        assert!(key(f1) <= key(f2), "wrong field order: {:?} {:?}", f1, f2);
    }

    let map = fields
        .iter()
        .map(|(field, ty, doc, default)| {
            let name = field.replace("_", ".");
            let name = format!("rust-analyzer.{}", name);
            let props = field_props(field, ty, doc, default);
            (name, props)
        })
        .collect::<serde_json::Map<_, _>>();
    map.into()
}

fn field_props(field: &str, ty: &str, doc: &[&str], default: &str) -> serde_json::Value {
    let doc = doc_comment_to_string(doc);
    let doc = doc.trim_end_matches('\n');
    assert!(
        doc.ends_with('.') && doc.starts_with(char::is_uppercase),
        "bad docs for {}: {:?}",
        field,
        doc
    );
    let default = default.parse::<serde_json::Value>().unwrap();

    let mut map = serde_json::Map::default();
    macro_rules! set {
        ($($key:literal: $value:tt),*$(,)?) => {{$(
            map.insert($key.into(), serde_json::json!($value));
        )*}};
    }
    set!("markdownDescription": doc);
    set!("default": default);

    match ty {
        "bool" => set!("type": "boolean"),
        "String" => set!("type": "string"),
        "Vec<String>" => set! {
            "type": "array",
            "items": { "type": "string" },
        },
        "Vec<PathBuf>" => set! {
            "type": "array",
            "items": { "type": "string" },
        },
        "FxHashSet<String>" => set! {
            "type": "array",
            "items": { "type": "string" },
            "uniqueItems": true,
        },
        "FxHashMap<String, String>" => set! {
            "type": "object",
        },
        "Option<usize>" => set! {
            "type": ["null", "integer"],
            "minimum": 0,
        },
        "Option<String>" => set! {
            "type": ["null", "string"],
        },
        "Option<PathBuf>" => set! {
            "type": ["null", "string"],
        },
        "Option<bool>" => set! {
            "type": ["null", "boolean"],
        },
        "Option<Vec<String>>" => set! {
            "type": ["null", "array"],
            "items": { "type": "string" },
        },
        "MergeBehaviorDef" => set! {
            "type": "string",
            "enum": ["none", "crate", "module"],
            "enumDescriptions": [
                "Do not merge imports at all.",
                "Merge imports from the same crate into a single `use` statement.",
                "Merge imports from the same module into a single `use` statement."
            ],
        },
        "ImportGranularityDef" => set! {
            "type": "string",
            "enum": ["preserve", "crate", "module", "item"],
            "enumDescriptions": [
                "Do not change the granularity of any imports and preserve the original structure written by the developer.",
                "Merge imports from the same crate into a single use statement. Conversely, imports from different crates are split into separate statements.",
                "Merge imports from the same module into a single use statement. Conversely, imports from different modules are split into separate statements.",
                "Flatten imports so that each has its own use statement."
            ],
        },
        "ImportPrefixDef" => set! {
            "type": "string",
            "enum": [
                "plain",
                "self",
                "crate"
            ],
            "enumDescriptions": [
                "Insert import paths relative to the current module, using up to one `super` prefix if the parent module contains the requested item.",
                "Insert import paths relative to the current module, using up to one `super` prefix if the parent module contains the requested item. Prefixes `self` in front of the path if it starts with a module.",
                "Force import paths to be absolute by always starting them with `crate` or the extern crate name they come from."
            ],
        },
        "Vec<ManifestOrProjectJson>" => set! {
            "type": "array",
            "items": { "type": ["string", "object"] },
        },
        "WorskpaceSymbolSearchScopeDef" => set! {
            "type": "string",
            "enum": ["workspace", "workspace_and_dependencies"],
            "enumDescriptions": [
                "Search in current workspace only",
                "Search in current workspace and dependencies"
            ],
        },
        "WorskpaceSymbolSearchKindDef" => set! {
            "type": "string",
            "enum": ["only_types", "all_symbols"],
            "enumDescriptions": [
                "Search for types only",
                "Search for all symbols kinds"
            ],
        },
        _ => panic!("{}: {}", ty, default),
    }

    map.into()
}

#[cfg(test)]
fn manual(fields: &[(&'static str, &'static str, &[&str], &str)]) -> String {
    fields
        .iter()
        .map(|(field, _ty, doc, default)| {
            let name = format!("rust-analyzer.{}", field.replace("_", "."));
            let doc = doc_comment_to_string(*doc);
            format!("[[{}]]{} (default: `{}`)::\n+\n--\n{}--\n", name, name, default, doc)
        })
        .collect::<String>()
}

fn doc_comment_to_string(doc: &[&str]) -> String {
    doc.iter().map(|it| it.strip_prefix(' ').unwrap_or(it)).map(|it| format!("{}\n", it)).collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use test_utils::{ensure_file_contents, project_root};

    use super::*;

    #[test]
    fn generate_package_json_config() {
        let s = Config::json_schema();
        let schema = format!("{:#}", s);
        let mut schema = schema
            .trim_start_matches('{')
            .trim_end_matches('}')
            .replace("  ", "    ")
            .replace("\n", "\n            ")
            .trim_start_matches('\n')
            .trim_end()
            .to_string();
        schema.push_str(",\n");

        let package_json_path = project_root().join("editors/code/package.json");
        let mut package_json = fs::read_to_string(&package_json_path).unwrap();

        let start_marker = "                \"$generated-start\": {},\n";
        let end_marker = "                \"$generated-end\": {}\n";

        let start = package_json.find(start_marker).unwrap() + start_marker.len();
        let end = package_json.find(end_marker).unwrap();

        let p = remove_ws(&package_json[start..end]);
        let s = remove_ws(&schema);
        if !p.contains(&s) {
            package_json.replace_range(start..end, &schema);
            ensure_file_contents(&package_json_path, &package_json)
        }
    }

    #[test]
    fn generate_config_documentation() {
        let docs_path = project_root().join("docs/user/generated_config.adoc");
        let expected = ConfigData::manual();
        ensure_file_contents(&docs_path, &expected);
    }

    fn remove_ws(text: &str) -> String {
        text.replace(char::is_whitespace, "")
    }
}
