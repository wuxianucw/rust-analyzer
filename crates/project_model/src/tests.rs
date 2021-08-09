use std::{
    ops::Deref,
    path::{Path, PathBuf},
};

use base_db::{CrateGraph, FileId};
use expect_test::{expect, Expect};
use paths::AbsPath;
use serde::de::DeserializeOwned;

use crate::{
    CargoWorkspace, CfgOverrides, ProjectJson, ProjectJsonData, ProjectWorkspace, Sysroot,
    WorkspaceBuildScripts,
};

fn load_cargo(file: &str) -> CrateGraph {
    let meta = get_test_json_file(file);
    let cargo_workspace = CargoWorkspace::new(meta);
    let project_workspace = ProjectWorkspace::Cargo {
        cargo: cargo_workspace,
        build_scripts: WorkspaceBuildScripts::default(),
        sysroot: Sysroot::default(),
        rustc: None,
        rustc_cfg: Vec::new(),
        cfg_overrides: CfgOverrides::default(),
    };
    to_crate_graph(project_workspace)
}

fn load_rust_project(file: &str) -> CrateGraph {
    let data = get_test_json_file(file);
    let project = rooted_project_json(data);
    let sysroot = Some(get_fake_sysroot());
    let project_workspace = ProjectWorkspace::Json { project, sysroot, rustc_cfg: Vec::new() };
    to_crate_graph(project_workspace)
}

fn get_test_json_file<T: DeserializeOwned>(file: &str) -> T {
    let file = get_test_path(file);
    let data = std::fs::read_to_string(file).unwrap();
    let mut json = data.parse::<serde_json::Value>().unwrap();
    fixup_paths(&mut json);
    return serde_json::from_value(json).unwrap();

    fn fixup_paths(val: &mut serde_json::Value) -> () {
        match val {
            serde_json::Value::String(s) => replace_root(s, true),
            serde_json::Value::Array(vals) => vals.iter_mut().for_each(fixup_paths),
            serde_json::Value::Object(kvals) => kvals.values_mut().for_each(fixup_paths),
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
                ()
            }
        }
    }
}

fn replace_root(s: &mut String, direction: bool) {
    if direction {
        let root = if cfg!(windows) { r#"C:\\ROOT\"# } else { "/ROOT/" };
        *s = s.replace("$ROOT$", root)
    } else {
        let root = if cfg!(windows) { r#"C:\\\\ROOT\\"# } else { "/ROOT/" };
        *s = s.replace(root, "$ROOT$")
    }
}

fn get_test_path(file: &str) -> PathBuf {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    base.join("test_data").join(file)
}

fn get_fake_sysroot() -> Sysroot {
    let sysroot_path = get_test_path("fake-sysroot");
    let sysroot_src_dir = AbsPath::assert(&sysroot_path);
    Sysroot::load(&sysroot_src_dir).unwrap()
}

fn rooted_project_json(data: ProjectJsonData) -> ProjectJson {
    let mut root = "$ROOT$".to_string();
    replace_root(&mut root, true);
    let path = Path::new(&root);
    let base = AbsPath::assert(path);
    ProjectJson::new(base, data)
}

fn to_crate_graph(project_workspace: ProjectWorkspace) -> CrateGraph {
    project_workspace.to_crate_graph(None, {
        let mut counter = 0;
        &mut move |_path| {
            counter += 1;
            Some(FileId(counter))
        }
    })
}

fn check_crate_graph(crate_graph: CrateGraph, expect: Expect) {
    let mut crate_graph = format!("{:#?}", crate_graph);
    replace_root(&mut crate_graph, false);
    expect.assert_eq(&crate_graph);
}

#[test]
fn cargo_hello_world_project_model() {
    let crate_graph = load_cargo("hello-world-metadata.json");
    check_crate_graph(
        crate_graph,
        expect![[r#"
            CrateGraph {
                arena: {
                    CrateId(
                        0,
                    ): CrateData {
                        root_file_id: FileId(
                            1,
                        ),
                        edition: Edition2018,
                        display_name: Some(
                            CrateDisplayName {
                                crate_name: CrateName(
                                    "hello_world",
                                ),
                                canonical_name: "hello-world",
                            },
                        ),
                        cfg_options: CfgOptions(
                            [
                                "debug_assertions",
                                "test",
                            ],
                        ),
                        potential_cfg_options: CfgOptions(
                            [
                                "debug_assertions",
                                "test",
                            ],
                        ),
                        env: Env {
                            entries: {
                                "CARGO_PKG_LICENSE": "",
                                "CARGO_PKG_VERSION_MAJOR": "0",
                                "CARGO_MANIFEST_DIR": "$ROOT$hello-world",
                                "CARGO_PKG_VERSION": "0.1.0",
                                "CARGO_PKG_AUTHORS": "",
                                "CARGO_CRATE_NAME": "hello_world",
                                "CARGO_PKG_LICENSE_FILE": "",
                                "CARGO_PKG_HOMEPAGE": "",
                                "CARGO_PKG_DESCRIPTION": "",
                                "CARGO_PKG_NAME": "hello-world",
                                "CARGO_PKG_VERSION_PATCH": "0",
                                "CARGO": "cargo",
                                "CARGO_PKG_REPOSITORY": "",
                                "CARGO_PKG_VERSION_MINOR": "1",
                                "CARGO_PKG_VERSION_PRE": "",
                            },
                        },
                        dependencies: [
                            Dependency {
                                crate_id: CrateId(
                                    4,
                                ),
                                name: CrateName(
                                    "libc",
                                ),
                            },
                        ],
                        proc_macro: [],
                    },
                    CrateId(
                        5,
                    ): CrateData {
                        root_file_id: FileId(
                            6,
                        ),
                        edition: Edition2015,
                        display_name: Some(
                            CrateDisplayName {
                                crate_name: CrateName(
                                    "const_fn",
                                ),
                                canonical_name: "const_fn",
                            },
                        ),
                        cfg_options: CfgOptions(
                            [
                                "debug_assertions",
                                "feature=default",
                                "feature=std",
                                "test",
                            ],
                        ),
                        potential_cfg_options: CfgOptions(
                            [
                                "debug_assertions",
                                "feature=align",
                                "feature=const-extern-fn",
                                "feature=default",
                                "feature=extra_traits",
                                "feature=rustc-dep-of-std",
                                "feature=std",
                                "feature=use_std",
                                "test",
                            ],
                        ),
                        env: Env {
                            entries: {
                                "CARGO_PKG_LICENSE": "",
                                "CARGO_PKG_VERSION_MAJOR": "0",
                                "CARGO_MANIFEST_DIR": "$ROOT$.cargo/registry/src/github.com-1ecc6299db9ec823/libc-0.2.98",
                                "CARGO_PKG_VERSION": "0.2.98",
                                "CARGO_PKG_AUTHORS": "",
                                "CARGO_CRATE_NAME": "libc",
                                "CARGO_PKG_LICENSE_FILE": "",
                                "CARGO_PKG_HOMEPAGE": "",
                                "CARGO_PKG_DESCRIPTION": "",
                                "CARGO_PKG_NAME": "libc",
                                "CARGO_PKG_VERSION_PATCH": "98",
                                "CARGO": "cargo",
                                "CARGO_PKG_REPOSITORY": "",
                                "CARGO_PKG_VERSION_MINOR": "2",
                                "CARGO_PKG_VERSION_PRE": "",
                            },
                        },
                        dependencies: [
                            Dependency {
                                crate_id: CrateId(
                                    4,
                                ),
                                name: CrateName(
                                    "libc",
                                ),
                            },
                        ],
                        proc_macro: [],
                    },
                    CrateId(
                        2,
                    ): CrateData {
                        root_file_id: FileId(
                            3,
                        ),
                        edition: Edition2018,
                        display_name: Some(
                            CrateDisplayName {
                                crate_name: CrateName(
                                    "an_example",
                                ),
                                canonical_name: "an-example",
                            },
                        ),
                        cfg_options: CfgOptions(
                            [
                                "debug_assertions",
                                "test",
                            ],
                        ),
                        potential_cfg_options: CfgOptions(
                            [
                                "debug_assertions",
                                "test",
                            ],
                        ),
                        env: Env {
                            entries: {
                                "CARGO_PKG_LICENSE": "",
                                "CARGO_PKG_VERSION_MAJOR": "0",
                                "CARGO_MANIFEST_DIR": "$ROOT$hello-world",
                                "CARGO_PKG_VERSION": "0.1.0",
                                "CARGO_PKG_AUTHORS": "",
                                "CARGO_CRATE_NAME": "hello_world",
                                "CARGO_PKG_LICENSE_FILE": "",
                                "CARGO_PKG_HOMEPAGE": "",
                                "CARGO_PKG_DESCRIPTION": "",
                                "CARGO_PKG_NAME": "hello-world",
                                "CARGO_PKG_VERSION_PATCH": "0",
                                "CARGO": "cargo",
                                "CARGO_PKG_REPOSITORY": "",
                                "CARGO_PKG_VERSION_MINOR": "1",
                                "CARGO_PKG_VERSION_PRE": "",
                            },
                        },
                        dependencies: [
                            Dependency {
                                crate_id: CrateId(
                                    0,
                                ),
                                name: CrateName(
                                    "hello_world",
                                ),
                            },
                            Dependency {
                                crate_id: CrateId(
                                    4,
                                ),
                                name: CrateName(
                                    "libc",
                                ),
                            },
                        ],
                        proc_macro: [],
                    },
                    CrateId(
                        4,
                    ): CrateData {
                        root_file_id: FileId(
                            5,
                        ),
                        edition: Edition2015,
                        display_name: Some(
                            CrateDisplayName {
                                crate_name: CrateName(
                                    "libc",
                                ),
                                canonical_name: "libc",
                            },
                        ),
                        cfg_options: CfgOptions(
                            [
                                "debug_assertions",
                                "feature=default",
                                "feature=std",
                                "test",
                            ],
                        ),
                        potential_cfg_options: CfgOptions(
                            [
                                "debug_assertions",
                                "feature=align",
                                "feature=const-extern-fn",
                                "feature=default",
                                "feature=extra_traits",
                                "feature=rustc-dep-of-std",
                                "feature=std",
                                "feature=use_std",
                                "test",
                            ],
                        ),
                        env: Env {
                            entries: {
                                "CARGO_PKG_LICENSE": "",
                                "CARGO_PKG_VERSION_MAJOR": "0",
                                "CARGO_MANIFEST_DIR": "$ROOT$.cargo/registry/src/github.com-1ecc6299db9ec823/libc-0.2.98",
                                "CARGO_PKG_VERSION": "0.2.98",
                                "CARGO_PKG_AUTHORS": "",
                                "CARGO_CRATE_NAME": "libc",
                                "CARGO_PKG_LICENSE_FILE": "",
                                "CARGO_PKG_HOMEPAGE": "",
                                "CARGO_PKG_DESCRIPTION": "",
                                "CARGO_PKG_NAME": "libc",
                                "CARGO_PKG_VERSION_PATCH": "98",
                                "CARGO": "cargo",
                                "CARGO_PKG_REPOSITORY": "",
                                "CARGO_PKG_VERSION_MINOR": "2",
                                "CARGO_PKG_VERSION_PRE": "",
                            },
                        },
                        dependencies: [],
                        proc_macro: [],
                    },
                    CrateId(
                        1,
                    ): CrateData {
                        root_file_id: FileId(
                            2,
                        ),
                        edition: Edition2018,
                        display_name: Some(
                            CrateDisplayName {
                                crate_name: CrateName(
                                    "hello_world",
                                ),
                                canonical_name: "hello-world",
                            },
                        ),
                        cfg_options: CfgOptions(
                            [
                                "debug_assertions",
                                "test",
                            ],
                        ),
                        potential_cfg_options: CfgOptions(
                            [
                                "debug_assertions",
                                "test",
                            ],
                        ),
                        env: Env {
                            entries: {
                                "CARGO_PKG_LICENSE": "",
                                "CARGO_PKG_VERSION_MAJOR": "0",
                                "CARGO_MANIFEST_DIR": "$ROOT$hello-world",
                                "CARGO_PKG_VERSION": "0.1.0",
                                "CARGO_PKG_AUTHORS": "",
                                "CARGO_CRATE_NAME": "hello_world",
                                "CARGO_PKG_LICENSE_FILE": "",
                                "CARGO_PKG_HOMEPAGE": "",
                                "CARGO_PKG_DESCRIPTION": "",
                                "CARGO_PKG_NAME": "hello-world",
                                "CARGO_PKG_VERSION_PATCH": "0",
                                "CARGO": "cargo",
                                "CARGO_PKG_REPOSITORY": "",
                                "CARGO_PKG_VERSION_MINOR": "1",
                                "CARGO_PKG_VERSION_PRE": "",
                            },
                        },
                        dependencies: [
                            Dependency {
                                crate_id: CrateId(
                                    0,
                                ),
                                name: CrateName(
                                    "hello_world",
                                ),
                            },
                            Dependency {
                                crate_id: CrateId(
                                    4,
                                ),
                                name: CrateName(
                                    "libc",
                                ),
                            },
                        ],
                        proc_macro: [],
                    },
                    CrateId(
                        6,
                    ): CrateData {
                        root_file_id: FileId(
                            7,
                        ),
                        edition: Edition2015,
                        display_name: Some(
                            CrateDisplayName {
                                crate_name: CrateName(
                                    "build_script_build",
                                ),
                                canonical_name: "build-script-build",
                            },
                        ),
                        cfg_options: CfgOptions(
                            [
                                "debug_assertions",
                                "feature=default",
                                "feature=std",
                                "test",
                            ],
                        ),
                        potential_cfg_options: CfgOptions(
                            [
                                "debug_assertions",
                                "feature=align",
                                "feature=const-extern-fn",
                                "feature=default",
                                "feature=extra_traits",
                                "feature=rustc-dep-of-std",
                                "feature=std",
                                "feature=use_std",
                                "test",
                            ],
                        ),
                        env: Env {
                            entries: {
                                "CARGO_PKG_LICENSE": "",
                                "CARGO_PKG_VERSION_MAJOR": "0",
                                "CARGO_MANIFEST_DIR": "$ROOT$.cargo/registry/src/github.com-1ecc6299db9ec823/libc-0.2.98",
                                "CARGO_PKG_VERSION": "0.2.98",
                                "CARGO_PKG_AUTHORS": "",
                                "CARGO_CRATE_NAME": "libc",
                                "CARGO_PKG_LICENSE_FILE": "",
                                "CARGO_PKG_HOMEPAGE": "",
                                "CARGO_PKG_DESCRIPTION": "",
                                "CARGO_PKG_NAME": "libc",
                                "CARGO_PKG_VERSION_PATCH": "98",
                                "CARGO": "cargo",
                                "CARGO_PKG_REPOSITORY": "",
                                "CARGO_PKG_VERSION_MINOR": "2",
                                "CARGO_PKG_VERSION_PRE": "",
                            },
                        },
                        dependencies: [],
                        proc_macro: [],
                    },
                    CrateId(
                        3,
                    ): CrateData {
                        root_file_id: FileId(
                            4,
                        ),
                        edition: Edition2018,
                        display_name: Some(
                            CrateDisplayName {
                                crate_name: CrateName(
                                    "it",
                                ),
                                canonical_name: "it",
                            },
                        ),
                        cfg_options: CfgOptions(
                            [
                                "debug_assertions",
                                "test",
                            ],
                        ),
                        potential_cfg_options: CfgOptions(
                            [
                                "debug_assertions",
                                "test",
                            ],
                        ),
                        env: Env {
                            entries: {
                                "CARGO_PKG_LICENSE": "",
                                "CARGO_PKG_VERSION_MAJOR": "0",
                                "CARGO_MANIFEST_DIR": "$ROOT$hello-world",
                                "CARGO_PKG_VERSION": "0.1.0",
                                "CARGO_PKG_AUTHORS": "",
                                "CARGO_CRATE_NAME": "hello_world",
                                "CARGO_PKG_LICENSE_FILE": "",
                                "CARGO_PKG_HOMEPAGE": "",
                                "CARGO_PKG_DESCRIPTION": "",
                                "CARGO_PKG_NAME": "hello-world",
                                "CARGO_PKG_VERSION_PATCH": "0",
                                "CARGO": "cargo",
                                "CARGO_PKG_REPOSITORY": "",
                                "CARGO_PKG_VERSION_MINOR": "1",
                                "CARGO_PKG_VERSION_PRE": "",
                            },
                        },
                        dependencies: [
                            Dependency {
                                crate_id: CrateId(
                                    0,
                                ),
                                name: CrateName(
                                    "hello_world",
                                ),
                            },
                            Dependency {
                                crate_id: CrateId(
                                    4,
                                ),
                                name: CrateName(
                                    "libc",
                                ),
                            },
                        ],
                        proc_macro: [],
                    },
                },
            }"#]],
    )
}

#[test]
fn rust_project_hello_world_project_model() {
    let crate_graph = load_rust_project("hello-world-project.json");
    check_crate_graph(
        crate_graph,
        expect![[r#"
            CrateGraph {
                arena: {
                    CrateId(
                        0,
                    ): CrateData {
                        root_file_id: FileId(
                            1,
                        ),
                        edition: Edition2018,
                        display_name: Some(
                            CrateDisplayName {
                                crate_name: CrateName(
                                    "alloc",
                                ),
                                canonical_name: "alloc",
                            },
                        ),
                        cfg_options: CfgOptions(
                            [],
                        ),
                        potential_cfg_options: CfgOptions(
                            [],
                        ),
                        env: Env {
                            entries: {},
                        },
                        dependencies: [
                            Dependency {
                                crate_id: CrateId(
                                    1,
                                ),
                                name: CrateName(
                                    "core",
                                ),
                            },
                        ],
                        proc_macro: [],
                    },
                    CrateId(
                        10,
                    ): CrateData {
                        root_file_id: FileId(
                            11,
                        ),
                        edition: Edition2018,
                        display_name: Some(
                            CrateDisplayName {
                                crate_name: CrateName(
                                    "unwind",
                                ),
                                canonical_name: "unwind",
                            },
                        ),
                        cfg_options: CfgOptions(
                            [],
                        ),
                        potential_cfg_options: CfgOptions(
                            [],
                        ),
                        env: Env {
                            entries: {},
                        },
                        dependencies: [],
                        proc_macro: [],
                    },
                    CrateId(
                        7,
                    ): CrateData {
                        root_file_id: FileId(
                            8,
                        ),
                        edition: Edition2018,
                        display_name: Some(
                            CrateDisplayName {
                                crate_name: CrateName(
                                    "std_detect",
                                ),
                                canonical_name: "std_detect",
                            },
                        ),
                        cfg_options: CfgOptions(
                            [],
                        ),
                        potential_cfg_options: CfgOptions(
                            [],
                        ),
                        env: Env {
                            entries: {},
                        },
                        dependencies: [],
                        proc_macro: [],
                    },
                    CrateId(
                        4,
                    ): CrateData {
                        root_file_id: FileId(
                            5,
                        ),
                        edition: Edition2018,
                        display_name: Some(
                            CrateDisplayName {
                                crate_name: CrateName(
                                    "proc_macro",
                                ),
                                canonical_name: "proc_macro",
                            },
                        ),
                        cfg_options: CfgOptions(
                            [],
                        ),
                        potential_cfg_options: CfgOptions(
                            [],
                        ),
                        env: Env {
                            entries: {},
                        },
                        dependencies: [
                            Dependency {
                                crate_id: CrateId(
                                    6,
                                ),
                                name: CrateName(
                                    "std",
                                ),
                            },
                        ],
                        proc_macro: [],
                    },
                    CrateId(
                        1,
                    ): CrateData {
                        root_file_id: FileId(
                            2,
                        ),
                        edition: Edition2018,
                        display_name: Some(
                            CrateDisplayName {
                                crate_name: CrateName(
                                    "core",
                                ),
                                canonical_name: "core",
                            },
                        ),
                        cfg_options: CfgOptions(
                            [],
                        ),
                        potential_cfg_options: CfgOptions(
                            [],
                        ),
                        env: Env {
                            entries: {},
                        },
                        dependencies: [],
                        proc_macro: [],
                    },
                    CrateId(
                        11,
                    ): CrateData {
                        root_file_id: FileId(
                            12,
                        ),
                        edition: Edition2018,
                        display_name: Some(
                            CrateDisplayName {
                                crate_name: CrateName(
                                    "hello_world",
                                ),
                                canonical_name: "hello_world",
                            },
                        ),
                        cfg_options: CfgOptions(
                            [],
                        ),
                        potential_cfg_options: CfgOptions(
                            [],
                        ),
                        env: Env {
                            entries: {},
                        },
                        dependencies: [
                            Dependency {
                                crate_id: CrateId(
                                    1,
                                ),
                                name: CrateName(
                                    "core",
                                ),
                            },
                            Dependency {
                                crate_id: CrateId(
                                    0,
                                ),
                                name: CrateName(
                                    "alloc",
                                ),
                            },
                            Dependency {
                                crate_id: CrateId(
                                    6,
                                ),
                                name: CrateName(
                                    "std",
                                ),
                            },
                        ],
                        proc_macro: [],
                    },
                    CrateId(
                        8,
                    ): CrateData {
                        root_file_id: FileId(
                            9,
                        ),
                        edition: Edition2018,
                        display_name: Some(
                            CrateDisplayName {
                                crate_name: CrateName(
                                    "term",
                                ),
                                canonical_name: "term",
                            },
                        ),
                        cfg_options: CfgOptions(
                            [],
                        ),
                        potential_cfg_options: CfgOptions(
                            [],
                        ),
                        env: Env {
                            entries: {},
                        },
                        dependencies: [],
                        proc_macro: [],
                    },
                    CrateId(
                        5,
                    ): CrateData {
                        root_file_id: FileId(
                            6,
                        ),
                        edition: Edition2018,
                        display_name: Some(
                            CrateDisplayName {
                                crate_name: CrateName(
                                    "profiler_builtins",
                                ),
                                canonical_name: "profiler_builtins",
                            },
                        ),
                        cfg_options: CfgOptions(
                            [],
                        ),
                        potential_cfg_options: CfgOptions(
                            [],
                        ),
                        env: Env {
                            entries: {},
                        },
                        dependencies: [],
                        proc_macro: [],
                    },
                    CrateId(
                        2,
                    ): CrateData {
                        root_file_id: FileId(
                            3,
                        ),
                        edition: Edition2018,
                        display_name: Some(
                            CrateDisplayName {
                                crate_name: CrateName(
                                    "panic_abort",
                                ),
                                canonical_name: "panic_abort",
                            },
                        ),
                        cfg_options: CfgOptions(
                            [],
                        ),
                        potential_cfg_options: CfgOptions(
                            [],
                        ),
                        env: Env {
                            entries: {},
                        },
                        dependencies: [],
                        proc_macro: [],
                    },
                    CrateId(
                        9,
                    ): CrateData {
                        root_file_id: FileId(
                            10,
                        ),
                        edition: Edition2018,
                        display_name: Some(
                            CrateDisplayName {
                                crate_name: CrateName(
                                    "test",
                                ),
                                canonical_name: "test",
                            },
                        ),
                        cfg_options: CfgOptions(
                            [],
                        ),
                        potential_cfg_options: CfgOptions(
                            [],
                        ),
                        env: Env {
                            entries: {},
                        },
                        dependencies: [],
                        proc_macro: [],
                    },
                    CrateId(
                        6,
                    ): CrateData {
                        root_file_id: FileId(
                            7,
                        ),
                        edition: Edition2018,
                        display_name: Some(
                            CrateDisplayName {
                                crate_name: CrateName(
                                    "std",
                                ),
                                canonical_name: "std",
                            },
                        ),
                        cfg_options: CfgOptions(
                            [],
                        ),
                        potential_cfg_options: CfgOptions(
                            [],
                        ),
                        env: Env {
                            entries: {},
                        },
                        dependencies: [
                            Dependency {
                                crate_id: CrateId(
                                    0,
                                ),
                                name: CrateName(
                                    "alloc",
                                ),
                            },
                            Dependency {
                                crate_id: CrateId(
                                    1,
                                ),
                                name: CrateName(
                                    "core",
                                ),
                            },
                            Dependency {
                                crate_id: CrateId(
                                    2,
                                ),
                                name: CrateName(
                                    "panic_abort",
                                ),
                            },
                            Dependency {
                                crate_id: CrateId(
                                    3,
                                ),
                                name: CrateName(
                                    "panic_unwind",
                                ),
                            },
                            Dependency {
                                crate_id: CrateId(
                                    5,
                                ),
                                name: CrateName(
                                    "profiler_builtins",
                                ),
                            },
                            Dependency {
                                crate_id: CrateId(
                                    7,
                                ),
                                name: CrateName(
                                    "std_detect",
                                ),
                            },
                            Dependency {
                                crate_id: CrateId(
                                    8,
                                ),
                                name: CrateName(
                                    "term",
                                ),
                            },
                            Dependency {
                                crate_id: CrateId(
                                    9,
                                ),
                                name: CrateName(
                                    "test",
                                ),
                            },
                            Dependency {
                                crate_id: CrateId(
                                    10,
                                ),
                                name: CrateName(
                                    "unwind",
                                ),
                            },
                        ],
                        proc_macro: [],
                    },
                    CrateId(
                        3,
                    ): CrateData {
                        root_file_id: FileId(
                            4,
                        ),
                        edition: Edition2018,
                        display_name: Some(
                            CrateDisplayName {
                                crate_name: CrateName(
                                    "panic_unwind",
                                ),
                                canonical_name: "panic_unwind",
                            },
                        ),
                        cfg_options: CfgOptions(
                            [],
                        ),
                        potential_cfg_options: CfgOptions(
                            [],
                        ),
                        env: Env {
                            entries: {},
                        },
                        dependencies: [],
                        proc_macro: [],
                    },
                },
            }"#]],
    );
}

#[test]
fn rust_project_is_proc_macro_has_proc_macro_dep() {
    let crate_graph = load_rust_project("is-proc-macro-project.json");
    // Since the project only defines one crate (outside the sysroot crates),
    // it should be the one with the biggest Id.
    let crate_id = crate_graph.iter().max().unwrap();
    let crate_data = &crate_graph[crate_id];
    // Assert that the project crate with `is_proc_macro` has a dependency
    // on the proc_macro sysroot crate.
    crate_data.dependencies.iter().find(|&dep| dep.name.deref() == "proc_macro").unwrap();
}
