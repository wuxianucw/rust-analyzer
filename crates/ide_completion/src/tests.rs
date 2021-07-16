//! Tests and test utilities for completions.
//!
//! Most tests live in this module or its submodules unless for very specific completions like
//! `attributes` or `lifetimes` where the completed concept is a distinct thing.
//! Notable examples for completions that are being tested in this module's submodule are paths.

mod attribute;
mod item_list;
mod item;
mod pattern;
mod predicate;
mod type_pos;
mod use_tree;

mod sourcegen;

use std::mem;

use hir::{PrefixKind, Semantics};
use ide_db::{
    base_db::{fixture::ChangeFixture, FileLoader, FilePosition},
    helpers::{
        insert_use::{ImportGranularity, InsertUseConfig},
        SnippetCap,
    },
    RootDatabase,
};
use itertools::Itertools;
use stdx::{format_to, trim_indent};
use syntax::{AstNode, NodeOrToken, SyntaxElement};
use test_utils::assert_eq_text;

use crate::{item::CompletionKind, CompletionConfig, CompletionItem};

/// Lots of basic item definitions
const BASE_FIXTURE: &str = r#"
enum Enum { TupleV(u32), RecordV { field: u32 }, UnitV }
use self::Enum::TupleV;
mod module {}

trait Trait {}
static STATIC: Unit = Unit;
const CONST: Unit = Unit;
struct Record { field: u32 }
struct Tuple(u32);
struct Unit;
#[macro_export]
macro_rules! makro {}
#[rustc_builtin_macro]
pub macro Clone {}
"#;

pub(crate) const TEST_CONFIG: CompletionConfig = CompletionConfig {
    enable_postfix_completions: true,
    enable_imports_on_the_fly: true,
    enable_self_on_the_fly: true,
    add_call_parenthesis: true,
    add_call_argument_snippets: true,
    snippet_cap: SnippetCap::new(true),
    insert_use: InsertUseConfig {
        granularity: ImportGranularity::Crate,
        prefix_kind: PrefixKind::Plain,
        enforce_granularity: true,
        group: true,
        skip_glob_imports: true,
    },
};

pub(crate) fn completion_list(code: &str) -> String {
    completion_list_with_config(TEST_CONFIG, code)
}

fn completion_list_with_config(config: CompletionConfig, code: &str) -> String {
    // filter out all but one builtintype completion for smaller test outputs
    let items = get_all_items(config, code);
    let mut bt_seen = false;
    let items = items
        .into_iter()
        .filter(|it| {
            it.completion_kind != CompletionKind::BuiltinType || !mem::replace(&mut bt_seen, true)
        })
        .collect();
    render_completion_list(items)
}

/// Creates analysis from a multi-file fixture, returns positions marked with $0.
pub(crate) fn position(ra_fixture: &str) -> (RootDatabase, FilePosition) {
    let change_fixture = ChangeFixture::parse(ra_fixture);
    let mut database = RootDatabase::default();
    database.apply_change(change_fixture.change);
    let (file_id, range_or_offset) = change_fixture.file_position.expect("expected a marker ($0)");
    let offset = range_or_offset.expect_offset();
    (database, FilePosition { file_id, offset })
}

pub(crate) fn do_completion(code: &str, kind: CompletionKind) -> Vec<CompletionItem> {
    do_completion_with_config(TEST_CONFIG, code, kind)
}

pub(crate) fn do_completion_with_config(
    config: CompletionConfig,
    code: &str,
    kind: CompletionKind,
) -> Vec<CompletionItem> {
    get_all_items(config, code)
        .into_iter()
        .filter(|c| c.completion_kind == kind)
        .sorted_by(|l, r| l.label().cmp(r.label()))
        .collect()
}

pub(crate) fn filtered_completion_list(code: &str, kind: CompletionKind) -> String {
    filtered_completion_list_with_config(TEST_CONFIG, code, kind)
}

pub(crate) fn filtered_completion_list_with_config(
    config: CompletionConfig,
    code: &str,
    kind: CompletionKind,
) -> String {
    let kind_completions: Vec<CompletionItem> =
        get_all_items(config, code).into_iter().filter(|c| c.completion_kind == kind).collect();
    render_completion_list(kind_completions)
}

fn render_completion_list(completions: Vec<CompletionItem>) -> String {
    fn monospace_width(s: &str) -> usize {
        s.chars().count()
    }
    let label_width =
        completions.iter().map(|it| monospace_width(it.label())).max().unwrap_or_default().min(22);
    completions
        .into_iter()
        .map(|it| {
            let tag = it.kind().unwrap().tag();
            let var_name = format!("{} {}", tag, it.label());
            let mut buf = var_name;
            if let Some(detail) = it.detail() {
                let width = label_width.saturating_sub(monospace_width(it.label()));
                format_to!(buf, "{:width$} {}", "", detail, width = width);
            }
            if it.deprecated() {
                format_to!(buf, " DEPRECATED");
            }
            format_to!(buf, "\n");
            buf
        })
        .collect()
}

pub(crate) fn check_edit(what: &str, ra_fixture_before: &str, ra_fixture_after: &str) {
    check_edit_with_config(TEST_CONFIG, what, ra_fixture_before, ra_fixture_after)
}

pub(crate) fn check_edit_with_config(
    config: CompletionConfig,
    what: &str,
    ra_fixture_before: &str,
    ra_fixture_after: &str,
) {
    let ra_fixture_after = trim_indent(ra_fixture_after);
    let (db, position) = position(ra_fixture_before);
    let completions: Vec<CompletionItem> =
        crate::completions(&db, &config, position).unwrap().into();
    let (completion,) = completions
        .iter()
        .filter(|it| it.lookup() == what)
        .collect_tuple()
        .unwrap_or_else(|| panic!("can't find {:?} completion in {:#?}", what, completions));
    let mut actual = db.file_text(position.file_id).to_string();

    let mut combined_edit = completion.text_edit().to_owned();
    if let Some(import_text_edit) =
        completion.import_to_add().and_then(|edit| edit.to_text_edit(config.insert_use))
    {
        combined_edit.union(import_text_edit).expect(
            "Failed to apply completion resolve changes: change ranges overlap, but should not",
        )
    }

    combined_edit.apply(&mut actual);
    assert_eq_text!(&ra_fixture_after, &actual)
}

pub(crate) fn check_pattern_is_applicable(code: &str, check: impl FnOnce(SyntaxElement) -> bool) {
    let (db, pos) = position(code);

    let sema = Semantics::new(&db);
    let original_file = sema.parse(pos.file_id);
    let token = original_file.syntax().token_at_offset(pos.offset).left_biased().unwrap();
    assert!(check(NodeOrToken::Token(token)));
}

pub(crate) fn check_pattern_is_not_applicable(code: &str, check: fn(SyntaxElement) -> bool) {
    let (db, pos) = position(code);
    let sema = Semantics::new(&db);
    let original_file = sema.parse(pos.file_id);
    let token = original_file.syntax().token_at_offset(pos.offset).left_biased().unwrap();
    assert!(!check(NodeOrToken::Token(token)));
}

pub(crate) fn get_all_items(config: CompletionConfig, code: &str) -> Vec<CompletionItem> {
    let (db, position) = position(code);
    crate::completions(&db, &config, position).unwrap().into()
}

fn check_no_completion(ra_fixture: &str) {
    let (db, position) = position(ra_fixture);

    assert!(
        crate::completions(&db, &TEST_CONFIG, position).is_none(),
        "Completions were generated, but weren't expected"
    );
}

#[test]
fn test_no_completions_required() {
    cov_mark::check!(no_completion_required);
    check_no_completion(r#"fn foo() { for i i$0 }"#);
}
