use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use xshell::{cmd, pushd, pushenv, read_file};

#[test]
fn check_code_formatting() {
    let _dir = pushd(sourcegen::project_root()).unwrap();
    let _e = pushenv("RUSTUP_TOOLCHAIN", "stable");

    let out = cmd!("rustfmt --version").read().unwrap();
    if !out.contains("stable") {
        panic!(
            "Failed to run rustfmt from toolchain 'stable'. \
                 Please run `rustup component add rustfmt --toolchain stable` to install it.",
        )
    }

    let res = cmd!("cargo fmt -- --check").run();
    if res.is_err() {
        let _ = cmd!("cargo fmt").run();
    }
    res.unwrap()
}

#[test]
fn check_lsp_extensions_docs() {
    let expected_hash = {
        let lsp_ext_rs =
            read_file(sourcegen::project_root().join("crates/rust-analyzer/src/lsp_ext.rs"))
                .unwrap();
        stable_hash(lsp_ext_rs.as_str())
    };

    let actual_hash = {
        let lsp_extensions_md =
            read_file(sourcegen::project_root().join("docs/dev/lsp-extensions.md")).unwrap();
        let text = lsp_extensions_md
            .lines()
            .find_map(|line| line.strip_prefix("lsp_ext.rs hash:"))
            .unwrap()
            .trim();
        u64::from_str_radix(text, 16).unwrap()
    };

    if actual_hash != expected_hash {
        panic!(
            "
lsp_ext.rs was changed without touching lsp-extensions.md.

Expected hash: {:x}
Actual hash:   {:x}

Please adjust docs/dev/lsp-extensions.md.
",
            expected_hash, actual_hash
        )
    }
}

#[test]
fn files_are_tidy() {
    let files = sourcegen::list_files(&sourcegen::project_root().join("crates"));

    let mut tidy_docs = TidyDocs::default();
    let mut tidy_marks = TidyMarks::default();
    for path in files {
        let extension = path.extension().unwrap_or_default().to_str().unwrap_or_default();
        match extension {
            "rs" => {
                let text = read_file(&path).unwrap();
                check_todo(&path, &text);
                check_dbg(&path, &text);
                check_test_attrs(&path, &text);
                check_trailing_ws(&path, &text);
                deny_clippy(&path, &text);
                tidy_docs.visit(&path, &text);
                tidy_marks.visit(&path, &text);
            }
            "toml" => {
                let text = read_file(&path).unwrap();
                check_cargo_toml(&path, text);
            }
            _ => (),
        }
    }

    tidy_docs.finish();
    tidy_marks.finish();
}

fn check_cargo_toml(path: &Path, text: String) -> () {
    let mut section = None;
    for (line_no, text) in text.lines().enumerate() {
        let text = text.trim();
        if text.starts_with('[') {
            if !text.ends_with(']') {
                panic!(
                    "\nplease don't add comments or trailing whitespace in section lines.\n\
                        {}:{}\n",
                    path.display(),
                    line_no + 1
                )
            }
            section = Some(text);
            continue;
        }
        let text: String = text.split_whitespace().collect();
        if !text.contains("path=") {
            continue;
        }
        match section {
            Some(s) if s.contains("dev-dependencies") => {
                if text.contains("version") {
                    panic!(
                        "\ncargo internal dev-dependencies should not have a version.\n\
                        {}:{}\n",
                        path.display(),
                        line_no + 1
                    );
                }
            }
            Some(s) if s.contains("dependencies") => {
                if !text.contains("version") {
                    panic!(
                        "\ncargo internal dependencies should have a version.\n\
                        {}:{}\n",
                        path.display(),
                        line_no + 1
                    );
                }
            }
            _ => {}
        }
    }
}

#[test]
fn check_merge_commits() {
    let stdout = cmd!("git rev-list --merges --invert-grep --author 'bors\\[bot\\]' HEAD~19..")
        .read()
        .unwrap();
    if !stdout.is_empty() {
        panic!(
            "
Merge commits are not allowed in the history.

When updating a pull-request, please rebase your feature branch
on top of master by running `git rebase master`. If rebase fails,
you can re-apply your changes like this:

  # Just look around to see the current state.
  $ git status
  $ git log

  # Abort in-progress rebase and merges, if any.
  $ git rebase --abort
  $ git merge --abort

  # Make the branch point to the latest commit from master,
  # while maintaining your local changes uncommited.
  $ git reset --soft origin/master

  # Commit all changes in a single batch.
  $ git commit -am'My changes'

  # Verify that everything looks alright.
  $ git status
  $ git log

  # Push the changes. We did a rebase, so we need `--force` option.
  # `--force-with-lease` is a more safe (Rusty) version of `--force`.
  $ git push --force-with-lease

  # Verify that both local and remote branch point to the same commit.
  $ git log

And don't fear to mess something up during a rebase -- you can
always restore the previous state using `git ref-log`:

https://github.blog/2015-06-08-how-to-undo-almost-anything-with-git/#redo-after-undo-local
"
        );
    }
}

fn deny_clippy(path: &Path, text: &str) {
    let ignore = &[
        // The documentation in string literals may contain anything for its own purposes
        "ide_db/src/helpers/generated_lints.rs",
        // The tests test clippy lint hovers
        "ide/src/hover.rs",
    ];
    if ignore.iter().any(|p| path.ends_with(p)) {
        return;
    }

    if text.contains("\u{61}llow(clippy") {
        panic!(
            "\n\nallowing lints is forbidden: {}.
rust-analyzer intentionally doesn't check clippy on CI.
You can allow lint globally via `xtask clippy`.
See https://github.com/rust-lang/rust-clippy/issues/5537 for discussion.

",
            path.display()
        )
    }
}

#[test]
fn check_licenses() {
    let expected = "
0BSD OR MIT OR Apache-2.0
Apache-2.0
Apache-2.0 OR BSL-1.0
Apache-2.0 OR MIT
Apache-2.0/MIT
BSD-3-Clause
CC0-1.0 OR Artistic-2.0
ISC
MIT
MIT / Apache-2.0
MIT OR Apache-2.0
MIT OR Apache-2.0 OR Zlib
MIT OR Zlib OR Apache-2.0
MIT/Apache-2.0
Unlicense OR MIT
Unlicense/MIT
Zlib OR Apache-2.0 OR MIT
"
    .lines()
    .filter(|it| !it.is_empty())
    .collect::<Vec<_>>();

    let meta = cmd!("cargo metadata --format-version 1").read().unwrap();
    let mut licenses = meta
        .split(|c| c == ',' || c == '{' || c == '}')
        .filter(|it| it.contains(r#""license""#))
        .map(|it| it.trim())
        .map(|it| it[r#""license":"#.len()..].trim_matches('"'))
        .collect::<Vec<_>>();
    licenses.sort_unstable();
    licenses.dedup();
    if licenses != expected {
        let mut diff = String::new();

        diff.push_str("New Licenses:\n");
        for &l in licenses.iter() {
            if !expected.contains(&l) {
                diff += &format!("  {}\n", l)
            }
        }

        diff.push_str("\nMissing Licenses:\n");
        for &l in expected.iter() {
            if !licenses.contains(&l) {
                diff += &format!("  {}\n", l)
            }
        }

        panic!("different set of licenses!\n{}", diff);
    }
    assert_eq!(licenses, expected);
}

fn check_todo(path: &Path, text: &str) {
    let need_todo = &[
        // This file itself obviously needs to use todo (<- like this!).
        "tests/tidy.rs",
        // Some of our assists generate `todo!()`.
        "handlers/add_turbo_fish.rs",
        "handlers/generate_function.rs",
        "handlers/fill_match_arms.rs",
        // To support generating `todo!()` in assists, we have `expr_todo()` in
        // `ast::make`.
        "ast/make.rs",
        // The documentation in string literals may contain anything for its own purposes
        "ide_db/src/helpers/generated_lints.rs",
    ];
    if need_todo.iter().any(|p| path.ends_with(p)) {
        return;
    }
    if text.contains("TODO") || text.contains("TOOD") || text.contains("todo!") {
        // Generated by an assist
        if text.contains("${0:todo!()}") {
            return;
        }

        panic!(
            "\nTODO markers or todo! macros should not be committed to the master branch,\n\
             use FIXME instead\n\
             {}\n",
            path.display(),
        )
    }
}

fn check_dbg(path: &Path, text: &str) {
    let need_dbg = &[
        // This file itself obviously needs to use dbg.
        "slow-tests/tidy.rs",
        // Assists to remove `dbg!()`
        "handlers/remove_dbg.rs",
        // We have .dbg postfix
        "ide_completion/src/completions/postfix.rs",
        // The documentation in string literals may contain anything for its own purposes
        "ide_completion/src/lib.rs",
        "ide_db/src/helpers/generated_lints.rs",
        // test for doc test for remove_dbg
        "src/tests/generated.rs",
    ];
    if need_dbg.iter().any(|p| path.ends_with(p)) {
        return;
    }
    if text.contains("dbg!") {
        panic!(
            "\ndbg! macros should not be committed to the master branch,\n\
             {}\n",
            path.display(),
        )
    }
}

fn check_test_attrs(path: &Path, text: &str) {
    let ignore_rule =
        "https://github.com/rust-analyzer/rust-analyzer/blob/master/docs/dev/style.md#ignore";
    let need_ignore: &[&str] = &[
        // This file.
        "slow-tests/tidy.rs",
        // Special case to run `#[ignore]` tests.
        "ide/src/runnables.rs",
        // A legit test which needs to be ignored, as it takes too long to run
        // :(
        "hir_def/src/nameres/collector.rs",
        // Long sourcegen test to generate lint completions.
        "ide_completion/src/tests/sourcegen.rs",
        // Obviously needs ignore.
        "ide_assists/src/handlers/toggle_ignore.rs",
        // See above.
        "ide_assists/src/tests/generated.rs",
    ];
    if text.contains("#[ignore") && !need_ignore.iter().any(|p| path.ends_with(p)) {
        panic!("\ndon't `#[ignore]` tests, see:\n\n    {}\n\n   {}\n", ignore_rule, path.display(),)
    }

    let panic_rule =
        "https://github.com/rust-analyzer/rust-analyzer/blob/master/docs/dev/style.md#should_panic";
    let need_panic: &[&str] = &[
        // This file.
        "slow-tests/tidy.rs",
        "test_utils/src/fixture.rs",
    ];
    if text.contains("#[should_panic") && !need_panic.iter().any(|p| path.ends_with(p)) {
        panic!(
            "\ndon't add `#[should_panic]` tests, see:\n\n    {}\n\n   {}\n",
            panic_rule,
            path.display(),
        )
    }
}

fn check_trailing_ws(path: &Path, text: &str) {
    if is_exclude_dir(path, &["test_data"]) {
        return;
    }
    for (line_number, line) in text.lines().enumerate() {
        if line.chars().last().map(char::is_whitespace) == Some(true) {
            panic!("Trailing whitespace in {} at line {}", path.display(), line_number + 1)
        }
    }
}

#[derive(Default)]
struct TidyDocs {
    missing_docs: Vec<String>,
    contains_fixme: Vec<PathBuf>,
}

impl TidyDocs {
    fn visit(&mut self, path: &Path, text: &str) {
        // Tests and diagnostic fixes don't need module level comments.
        if is_exclude_dir(path, &["tests", "test_data", "fixes", "grammar"]) {
            return;
        }

        if is_exclude_file(path) {
            return;
        }

        let first_line = match text.lines().next() {
            Some(it) => it,
            None => return,
        };

        if first_line.starts_with("//!") {
            if first_line.contains("FIXME") {
                self.contains_fixme.push(path.to_path_buf());
            }
        } else {
            if text.contains("// Feature:")
                || text.contains("// Assist:")
                || text.contains("// Diagnostic:")
            {
                return;
            }
            self.missing_docs.push(path.display().to_string());
        }

        fn is_exclude_file(d: &Path) -> bool {
            let file_names = ["tests.rs", "famous_defs_fixture.rs"];

            d.file_name()
                .unwrap_or_default()
                .to_str()
                .map(|f_n| file_names.iter().any(|name| *name == f_n))
                .unwrap_or(false)
        }
    }

    fn finish(self) {
        if !self.missing_docs.is_empty() {
            panic!(
                "\nMissing docs strings\n\n\
                 modules:\n{}\n\n",
                self.missing_docs.join("\n")
            )
        }

        for path in self.contains_fixme {
            panic!("FIXME doc in a fully-documented crate: {}", path.display())
        }
    }
}

fn is_exclude_dir(p: &Path, dirs_to_exclude: &[&str]) -> bool {
    p.strip_prefix(sourcegen::project_root())
        .unwrap()
        .components()
        .rev()
        .skip(1)
        .filter_map(|it| it.as_os_str().to_str())
        .any(|it| dirs_to_exclude.contains(&it))
}

#[derive(Default)]
struct TidyMarks {
    hits: HashSet<String>,
    checks: HashSet<String>,
}

impl TidyMarks {
    fn visit(&mut self, _path: &Path, text: &str) {
        for line in text.lines() {
            if let Some(mark) = find_mark(line, "hit") {
                self.hits.insert(mark.to_string());
            }
            if let Some(mark) = find_mark(line, "check") {
                self.checks.insert(mark.to_string());
            }
            if let Some(mark) = find_mark(line, "check_count") {
                self.checks.insert(mark.to_string());
            }
        }
    }

    fn finish(self) {
        assert!(!self.hits.is_empty());

        let diff: Vec<_> =
            self.hits.symmetric_difference(&self.checks).map(|it| it.as_str()).collect();

        if !diff.is_empty() {
            panic!("unpaired marks: {:?}", diff)
        }
    }
}

#[allow(deprecated)]
fn stable_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher, SipHasher};

    let text = text.replace('\r', "");
    let mut hasher = SipHasher::default();
    text.hash(&mut hasher);
    hasher.finish()
}

fn find_mark<'a>(text: &'a str, mark: &'static str) -> Option<&'a str> {
    let idx = text.find(mark)?;
    let text = text[idx + mark.len()..].strip_prefix("!(")?;
    let idx = text.find(|c: char| !(c.is_alphanumeric() || c == '_'))?;
    let text = &text[..idx];
    Some(text)
}
