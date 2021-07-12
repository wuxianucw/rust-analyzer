//! Extracts, resolves and rewrites links and intra-doc links in markdown documentation.

use std::{
    convert::{TryFrom, TryInto},
    iter::once,
};

use itertools::Itertools;
use pulldown_cmark::{BrokenLink, CowStr, Event, InlineStr, LinkType, Options, Parser, Tag};
use pulldown_cmark_to_cmark::{cmark_with_options, Options as CmarkOptions};
use url::Url;

use hir::{
    db::{DefDatabase, HirDatabase},
    Adt, AsAssocItem, AssocItem, AssocItemContainer, Crate, Field, HasAttrs, ItemInNs, ModuleDef,
};
use ide_db::{
    defs::{Definition, NameClass, NameRefClass},
    helpers::pick_best_token,
    RootDatabase,
};
use syntax::{ast, match_ast, AstNode, SyntaxKind::*, SyntaxNode, TextRange, T};

use crate::{FilePosition, Semantics};

pub(crate) type DocumentationLink = String;

/// Rewrite documentation links in markdown to point to an online host (e.g. docs.rs)
pub(crate) fn rewrite_links(db: &RootDatabase, markdown: &str, definition: &Definition) -> String {
    let mut cb = broken_link_clone_cb;
    let doc =
        Parser::new_with_broken_link_callback(markdown, Options::ENABLE_TASKLISTS, Some(&mut cb));

    let doc = map_links(doc, |target, title: &str| {
        // This check is imperfect, there's some overlap between valid intra-doc links
        // and valid URLs so we choose to be too eager to try to resolve what might be
        // a URL.
        if target.contains("://") {
            (target.to_string(), title.to_string())
        } else {
            // Two possibilities:
            // * path-based links: `../../module/struct.MyStruct.html`
            // * module-based links (AKA intra-doc links): `super::super::module::MyStruct`
            if let Some(rewritten) = rewrite_intra_doc_link(db, *definition, target, title) {
                return rewritten;
            }
            if let Definition::ModuleDef(def) = *definition {
                if let Some(target) = rewrite_url_link(db, def, target) {
                    return (target, title.to_string());
                }
            }

            (target.to_string(), title.to_string())
        }
    });
    let mut out = String::new();
    let mut options = CmarkOptions::default();
    options.code_block_backticks = 3;
    cmark_with_options(doc, &mut out, None, options).ok();
    out
}

/// Remove all links in markdown documentation.
pub(crate) fn remove_links(markdown: &str) -> String {
    let mut drop_link = false;

    let opts = Options::ENABLE_TASKLISTS | Options::ENABLE_FOOTNOTES;

    let mut cb = |_: BrokenLink| {
        let empty = InlineStr::try_from("").unwrap();
        Some((CowStr::Inlined(empty), CowStr::Inlined(empty)))
    };
    let doc = Parser::new_with_broken_link_callback(markdown, opts, Some(&mut cb));
    let doc = doc.filter_map(move |evt| match evt {
        Event::Start(Tag::Link(link_type, ref target, ref title)) => {
            if link_type == LinkType::Inline && target.contains("://") {
                Some(Event::Start(Tag::Link(link_type, target.clone(), title.clone())))
            } else {
                drop_link = true;
                None
            }
        }
        Event::End(_) if drop_link => {
            drop_link = false;
            None
        }
        _ => Some(evt),
    });

    let mut out = String::new();
    let mut options = CmarkOptions::default();
    options.code_block_backticks = 3;
    cmark_with_options(doc, &mut out, None, options).ok();
    out
}

/// Retrieve a link to documentation for the given symbol.
pub(crate) fn external_docs(
    db: &RootDatabase,
    position: &FilePosition,
) -> Option<DocumentationLink> {
    let sema = Semantics::new(db);
    let file = sema.parse(position.file_id).syntax().clone();
    let token = pick_best_token(file.token_at_offset(position.offset), |kind| match kind {
        IDENT | INT_NUMBER => 3,
        T!['('] | T![')'] => 2,
        kind if kind.is_trivia() => 0,
        _ => 1,
    })?;
    let token = sema.descend_into_macros(token);

    let node = token.parent()?;
    let definition = match_ast! {
        match node {
            ast::NameRef(name_ref) => match NameRefClass::classify(&sema, &name_ref)? {
                NameRefClass::Definition(def) => def,
                NameRefClass::FieldShorthand { local_ref: _, field_ref } => {
                    Definition::Field(field_ref)
                }
            },
            ast::Name(name) => match NameClass::classify(&sema, &name)? {
                NameClass::Definition(it) | NameClass::ConstReference(it) => it,
                NameClass::PatFieldShorthand { local_def: _, field_ref } => Definition::Field(field_ref),
            },
            _ => return None,
        }
    };

    get_doc_link(db, definition)
}

/// Extracts all links from a given markdown text.
pub(crate) fn extract_definitions_from_markdown(
    markdown: &str,
) -> Vec<(TextRange, String, Option<hir::Namespace>)> {
    Parser::new_with_broken_link_callback(
        markdown,
        Options::ENABLE_TASKLISTS,
        Some(&mut broken_link_clone_cb),
    )
    .into_offset_iter()
    .filter_map(|(event, range)| {
        if let Event::Start(Tag::Link(_, target, title)) = event {
            let link = if target.is_empty() { title } else { target };
            let (link, ns) = parse_intra_doc_link(&link);
            Some((
                TextRange::new(range.start.try_into().ok()?, range.end.try_into().ok()?),
                link.to_string(),
                ns,
            ))
        } else {
            None
        }
    })
    .collect()
}

pub(crate) fn resolve_doc_path_for_def(
    db: &dyn HirDatabase,
    def: Definition,
    link: &str,
    ns: Option<hir::Namespace>,
) -> Option<hir::ModuleDef> {
    match def {
        Definition::ModuleDef(def) => match def {
            hir::ModuleDef::Module(it) => it.resolve_doc_path(db, link, ns),
            hir::ModuleDef::Function(it) => it.resolve_doc_path(db, link, ns),
            hir::ModuleDef::Adt(it) => it.resolve_doc_path(db, link, ns),
            hir::ModuleDef::Variant(it) => it.resolve_doc_path(db, link, ns),
            hir::ModuleDef::Const(it) => it.resolve_doc_path(db, link, ns),
            hir::ModuleDef::Static(it) => it.resolve_doc_path(db, link, ns),
            hir::ModuleDef::Trait(it) => it.resolve_doc_path(db, link, ns),
            hir::ModuleDef::TypeAlias(it) => it.resolve_doc_path(db, link, ns),
            hir::ModuleDef::BuiltinType(_) => None,
        },
        Definition::Macro(it) => it.resolve_doc_path(db, link, ns),
        Definition::Field(it) => it.resolve_doc_path(db, link, ns),
        Definition::SelfType(_)
        | Definition::Local(_)
        | Definition::GenericParam(_)
        | Definition::Label(_) => None,
    }
}

pub(crate) fn doc_attributes(
    sema: &Semantics<RootDatabase>,
    node: &SyntaxNode,
) -> Option<(hir::AttrsWithOwner, Definition)> {
    match_ast! {
        match node {
            ast::SourceFile(it)  => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::ModuleDef(hir::ModuleDef::Module(def)))),
            ast::Module(it)      => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::ModuleDef(hir::ModuleDef::Module(def)))),
            ast::Fn(it)          => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::ModuleDef(hir::ModuleDef::Function(def)))),
            ast::Struct(it)      => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::ModuleDef(hir::ModuleDef::Adt(hir::Adt::Struct(def))))),
            ast::Union(it)       => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::ModuleDef(hir::ModuleDef::Adt(hir::Adt::Union(def))))),
            ast::Enum(it)        => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::ModuleDef(hir::ModuleDef::Adt(hir::Adt::Enum(def))))),
            ast::Variant(it)     => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::ModuleDef(hir::ModuleDef::Variant(def)))),
            ast::Trait(it)       => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::ModuleDef(hir::ModuleDef::Trait(def)))),
            ast::Static(it)      => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::ModuleDef(hir::ModuleDef::Static(def)))),
            ast::Const(it)       => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::ModuleDef(hir::ModuleDef::Const(def)))),
            ast::TypeAlias(it)   => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::ModuleDef(hir::ModuleDef::TypeAlias(def)))),
            ast::Impl(it)        => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::SelfType(def))),
            ast::RecordField(it) => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::Field(def))),
            ast::TupleField(it)  => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::Field(def))),
            ast::Macro(it)       => sema.to_def(&it).map(|def| (def.attrs(sema.db), Definition::Macro(def))),
            // ast::Use(it) => sema.to_def(&it).map(|def| (Box::new(it) as _, def.attrs(sema.db))),
            _ => None
        }
    }
}

fn broken_link_clone_cb<'a, 'b>(link: BrokenLink<'a>) -> Option<(CowStr<'b>, CowStr<'b>)> {
    // These allocations are actually unnecessary but the lifetimes on BrokenLinkCallback are wrong
    // this is fixed in the repo but not on the crates.io release yet
    Some((
        /*url*/ link.reference.to_owned().into(),
        /*title*/ link.reference.to_owned().into(),
    ))
}

// FIXME:
// BUG: For Option::Some
// Returns https://doc.rust-lang.org/nightly/core/prelude/v1/enum.Option.html#variant.Some
// Instead of https://doc.rust-lang.org/nightly/core/option/enum.Option.html
//
// This should cease to be a problem if RFC2988 (Stable Rustdoc URLs) is implemented
// https://github.com/rust-lang/rfcs/pull/2988
fn get_doc_link(db: &RootDatabase, definition: Definition) -> Option<String> {
    // Get the outermost definition for the module def. This is used to resolve the public path to the type,
    // then we can join the method, field, etc onto it if required.
    let target_def: ModuleDef = match definition {
        Definition::ModuleDef(def) => match def {
            ModuleDef::Function(f) => f
                .as_assoc_item(db)
                .and_then(|assoc| match assoc.container(db) {
                    AssocItemContainer::Trait(t) => Some(t.into()),
                    AssocItemContainer::Impl(impl_) => {
                        impl_.self_ty(db).as_adt().map(|adt| adt.into())
                    }
                })
                .unwrap_or_else(|| def),
            def => def,
        },
        Definition::Field(f) => f.parent_def(db).into(),
        // FIXME: Handle macros
        _ => return None,
    };

    let ns = ItemInNs::from(target_def);

    let krate = match definition {
        // Definition::module gives back the parent module, we don't want that as it fails for root modules
        Definition::ModuleDef(ModuleDef::Module(module)) => module.krate(),
        _ => definition.module(db)?.krate(),
    };
    // FIXME: using import map doesn't make sense here. What we want here is
    // canonical path. What import map returns is the shortest path suitable for
    // import. See this test:
    cov_mark::hit!(test_reexport_order);
    let import_map = db.import_map(krate.into());

    let mut base = krate.display_name(db)?.to_string();
    let is_root_module = matches!(
        definition,
        Definition::ModuleDef(ModuleDef::Module(module)) if krate.root_module(db) == module
    );
    if !is_root_module {
        base = once(base)
            .chain(import_map.path_of(ns)?.segments.iter().map(|name| name.to_string()))
            .join("/");
    }
    base += "/";

    let filename = get_symbol_filename(db, &target_def);
    let fragment = match definition {
        Definition::ModuleDef(def) => match def {
            ModuleDef::Function(f) => {
                get_symbol_fragment(db, &FieldOrAssocItem::AssocItem(AssocItem::Function(f)))
            }
            ModuleDef::Const(c) => {
                get_symbol_fragment(db, &FieldOrAssocItem::AssocItem(AssocItem::Const(c)))
            }
            ModuleDef::TypeAlias(ty) => {
                get_symbol_fragment(db, &FieldOrAssocItem::AssocItem(AssocItem::TypeAlias(ty)))
            }
            _ => None,
        },
        Definition::Field(field) => get_symbol_fragment(db, &FieldOrAssocItem::Field(field)),
        _ => None,
    };

    get_doc_url(db, &krate)?
        .join(&base)
        .ok()
        .and_then(|mut url| {
            if !matches!(definition, Definition::ModuleDef(ModuleDef::Module(..))) {
                url.path_segments_mut().ok()?.pop();
            };
            Some(url)
        })
        .and_then(|url| url.join(filename.as_deref()?).ok())
        .and_then(
            |url| if let Some(fragment) = fragment { url.join(&fragment).ok() } else { Some(url) },
        )
        .map(|url| url.into())
}

fn rewrite_intra_doc_link(
    db: &RootDatabase,
    def: Definition,
    target: &str,
    title: &str,
) -> Option<(String, String)> {
    let link = if target.is_empty() { title } else { target };
    let (link, ns) = parse_intra_doc_link(link);
    let resolved = resolve_doc_path_for_def(db, def, link, ns)?;
    let krate = resolved.module(db)?.krate();
    let canonical_path = resolved.canonical_path(db)?;
    let mut new_url = get_doc_url(db, &krate)?
        .join(&format!("{}/", krate.display_name(db)?))
        .ok()?
        .join(&canonical_path.replace("::", "/"))
        .ok()?
        .join(&get_symbol_filename(db, &resolved)?)
        .ok()?;

    if let ModuleDef::Trait(t) = resolved {
        if let Some(assoc_item) = t.items(db).into_iter().find_map(|assoc_item| {
            if let Some(name) = assoc_item.name(db) {
                if *link == format!("{}::{}", canonical_path, name) {
                    return Some(assoc_item);
                }
            }
            None
        }) {
            if let Some(fragment) =
                get_symbol_fragment(db, &FieldOrAssocItem::AssocItem(assoc_item))
            {
                new_url = new_url.join(&fragment).ok()?;
            }
        };
    }

    Some((new_url.into(), strip_prefixes_suffixes(title).to_string()))
}

/// Try to resolve path to local documentation via path-based links (i.e. `../gateway/struct.Shard.html`).
fn rewrite_url_link(db: &RootDatabase, def: ModuleDef, target: &str) -> Option<String> {
    if !(target.contains('#') || target.contains(".html")) {
        return None;
    }

    let module = def.module(db)?;
    let krate = module.krate();
    let canonical_path = def.canonical_path(db)?;
    let base = format!("{}/{}", krate.display_name(db)?, canonical_path.replace("::", "/"));

    get_doc_url(db, &krate)
        .and_then(|url| url.join(&base).ok())
        .and_then(|url| {
            get_symbol_filename(db, &def).as_deref().map(|f| url.join(f).ok()).flatten()
        })
        .and_then(|url| url.join(target).ok())
        .map(|url| url.into())
}

/// Rewrites a markdown document, applying 'callback' to each link.
fn map_links<'e>(
    events: impl Iterator<Item = Event<'e>>,
    callback: impl Fn(&str, &str) -> (String, String),
) -> impl Iterator<Item = Event<'e>> {
    let mut in_link = false;
    let mut link_target: Option<CowStr> = None;

    events.map(move |evt| match evt {
        Event::Start(Tag::Link(_link_type, ref target, _)) => {
            in_link = true;
            link_target = Some(target.clone());
            evt
        }
        Event::End(Tag::Link(link_type, _target, _)) => {
            in_link = false;
            Event::End(Tag::Link(link_type, link_target.take().unwrap(), CowStr::Borrowed("")))
        }
        Event::Text(s) if in_link => {
            let (link_target_s, link_name) = callback(&link_target.take().unwrap(), &s);
            link_target = Some(CowStr::Boxed(link_target_s.into()));
            Event::Text(CowStr::Boxed(link_name.into()))
        }
        Event::Code(s) if in_link => {
            let (link_target_s, link_name) = callback(&link_target.take().unwrap(), &s);
            link_target = Some(CowStr::Boxed(link_target_s.into()));
            Event::Code(CowStr::Boxed(link_name.into()))
        }
        _ => evt,
    })
}

const TYPES: ([&str; 9], [&str; 0]) =
    (["type", "struct", "enum", "mod", "trait", "union", "module", "prim", "primitive"], []);
const VALUES: ([&str; 8], [&str; 1]) =
    (["value", "function", "fn", "method", "const", "static", "mod", "module"], ["()"]);
const MACROS: ([&str; 2], [&str; 1]) = (["macro", "derive"], ["!"]);

/// Extract the specified namespace from an intra-doc-link if one exists.
///
/// # Examples
///
/// * `struct MyStruct` -> ("MyStruct", `Namespace::Types`)
/// * `panic!` -> ("panic", `Namespace::Macros`)
/// * `fn@from_intra_spec` -> ("from_intra_spec", `Namespace::Values`)
fn parse_intra_doc_link(s: &str) -> (&str, Option<hir::Namespace>) {
    let s = s.trim_matches('`');

    [
        (hir::Namespace::Types, (TYPES.0.iter(), TYPES.1.iter())),
        (hir::Namespace::Values, (VALUES.0.iter(), VALUES.1.iter())),
        (hir::Namespace::Macros, (MACROS.0.iter(), MACROS.1.iter())),
    ]
    .iter()
    .cloned()
    .find_map(|(ns, (mut prefixes, mut suffixes))| {
        if let Some(prefix) = prefixes.find(|&&prefix| {
            s.starts_with(prefix)
                && s.chars().nth(prefix.len()).map_or(false, |c| c == '@' || c == ' ')
        }) {
            Some((&s[prefix.len() + 1..], ns))
        } else {
            suffixes.find_map(|&suffix| s.strip_suffix(suffix).zip(Some(ns)))
        }
    })
    .map_or((s, None), |(s, ns)| (s, Some(ns)))
}

fn strip_prefixes_suffixes(s: &str) -> &str {
    [
        (TYPES.0.iter(), TYPES.1.iter()),
        (VALUES.0.iter(), VALUES.1.iter()),
        (MACROS.0.iter(), MACROS.1.iter()),
    ]
    .iter()
    .cloned()
    .find_map(|(mut prefixes, mut suffixes)| {
        if let Some(prefix) = prefixes.find(|&&prefix| {
            s.starts_with(prefix)
                && s.chars().nth(prefix.len()).map_or(false, |c| c == '@' || c == ' ')
        }) {
            Some(&s[prefix.len() + 1..])
        } else {
            suffixes.find_map(|&suffix| s.strip_suffix(suffix))
        }
    })
    .unwrap_or(s)
}

/// Get the root URL for the documentation of a crate.
///
/// ```
/// https://doc.rust-lang.org/std/iter/trait.Iterator.html#tymethod.next
/// ^^^^^^^^^^^^^^^^^^^^^^^^^^
/// ```
fn get_doc_url(db: &RootDatabase, krate: &Crate) -> Option<Url> {
    krate
        .get_html_root_url(db)
        .or_else(|| {
            // Fallback to docs.rs. This uses `display_name` and can never be
            // correct, but that's what fallbacks are about.
            //
            // FIXME: clicking on the link should just open the file in the editor,
            // instead of falling back to external urls.
            Some(format!("https://docs.rs/{}/*/", krate.display_name(db)?))
        })
        .and_then(|s| Url::parse(&s).ok())
}

/// Get the filename and extension generated for a symbol by rustdoc.
///
/// ```
/// https://doc.rust-lang.org/std/iter/trait.Iterator.html#tymethod.next
///                                    ^^^^^^^^^^^^^^^^^^^
/// ```
fn get_symbol_filename(db: &dyn HirDatabase, definition: &ModuleDef) -> Option<String> {
    Some(match definition {
        ModuleDef::Adt(adt) => match adt {
            Adt::Struct(s) => format!("struct.{}.html", s.name(db)),
            Adt::Enum(e) => format!("enum.{}.html", e.name(db)),
            Adt::Union(u) => format!("union.{}.html", u.name(db)),
        },
        ModuleDef::Module(_) => "index.html".to_string(),
        ModuleDef::Trait(t) => format!("trait.{}.html", t.name(db)),
        ModuleDef::TypeAlias(t) => format!("type.{}.html", t.name(db)),
        ModuleDef::BuiltinType(t) => format!("primitive.{}.html", t.name()),
        ModuleDef::Function(f) => format!("fn.{}.html", f.name(db)),
        ModuleDef::Variant(ev) => {
            format!("enum.{}.html#variant.{}", ev.parent_enum(db).name(db), ev.name(db))
        }
        ModuleDef::Const(c) => format!("const.{}.html", c.name(db)?),
        ModuleDef::Static(s) => format!("static.{}.html", s.name(db)?),
    })
}

enum FieldOrAssocItem {
    Field(Field),
    AssocItem(AssocItem),
}

/// Get the fragment required to link to a specific field, method, associated type, or associated constant.
///
/// ```
/// https://doc.rust-lang.org/std/iter/trait.Iterator.html#tymethod.next
///                                                       ^^^^^^^^^^^^^^
/// ```
fn get_symbol_fragment(db: &dyn HirDatabase, field_or_assoc: &FieldOrAssocItem) -> Option<String> {
    Some(match field_or_assoc {
        FieldOrAssocItem::Field(field) => format!("#structfield.{}", field.name(db)),
        FieldOrAssocItem::AssocItem(assoc) => match assoc {
            AssocItem::Function(function) => {
                let is_trait_method = function
                    .as_assoc_item(db)
                    .and_then(|assoc| assoc.containing_trait(db))
                    .is_some();
                // This distinction may get more complicated when specialization is available.
                // Rustdoc makes this decision based on whether a method 'has defaultness'.
                // Currently this is only the case for provided trait methods.
                if is_trait_method && !function.has_body(db) {
                    format!("#tymethod.{}", function.name(db))
                } else {
                    format!("#method.{}", function.name(db))
                }
            }
            AssocItem::Const(constant) => format!("#associatedconstant.{}", constant.name(db)?),
            AssocItem::TypeAlias(ty) => format!("#associatedtype.{}", ty.name(db)),
        },
    })
}

#[cfg(test)]
mod tests {
    use expect_test::{expect, Expect};

    use crate::fixture;

    fn check(ra_fixture: &str, expect: Expect) {
        let (analysis, position) = fixture::position(ra_fixture);
        let url = analysis.external_docs(position).unwrap().expect("could not find url for symbol");

        expect.assert_eq(&url)
    }

    #[test]
    fn test_doc_url_crate() {
        check(
            r#"
//- /main.rs crate:main deps:test
use test$0::Foo;
//- /lib.rs crate:test
pub struct Foo;
"#,
            expect![[r#"https://docs.rs/test/*/test/index.html"#]],
        );
    }

    #[test]
    fn test_doc_url_struct() {
        check(
            r#"
pub struct Fo$0o;
"#,
            expect![[r#"https://docs.rs/test/*/test/struct.Foo.html"#]],
        );
    }

    #[test]
    fn test_doc_url_fn() {
        check(
            r#"
pub fn fo$0o() {}
"#,
            expect![[r##"https://docs.rs/test/*/test/fn.foo.html#method.foo"##]],
        );
    }

    #[test]
    fn test_doc_url_inherent_method() {
        check(
            r#"
pub struct Foo;

impl Foo {
    pub fn met$0hod() {}
}

"#,
            expect![[r##"https://docs.rs/test/*/test/struct.Foo.html#method.method"##]],
        );
    }

    #[test]
    fn test_doc_url_trait_provided_method() {
        check(
            r#"
pub trait Bar {
    fn met$0hod() {}
}

"#,
            expect![[r##"https://docs.rs/test/*/test/trait.Bar.html#method.method"##]],
        );
    }

    #[test]
    fn test_doc_url_trait_required_method() {
        check(
            r#"
pub trait Foo {
    fn met$0hod();
}

"#,
            expect![[r##"https://docs.rs/test/*/test/trait.Foo.html#tymethod.method"##]],
        );
    }

    #[test]
    fn test_doc_url_field() {
        check(
            r#"
pub struct Foo {
    pub fie$0ld: ()
}

"#,
            expect![[r##"https://docs.rs/test/*/test/struct.Foo.html#structfield.field"##]],
        );
    }

    #[test]
    fn test_module() {
        check(
            r#"
pub mod foo {
    pub mod ba$0r {}
}
        "#,
            expect![[r#"https://docs.rs/test/*/test/foo/bar/index.html"#]],
        )
    }

    #[test]
    fn test_reexport_order() {
        cov_mark::check!(test_reexport_order);
        // FIXME: This should return
        //
        //    https://docs.rs/test/*/test/wrapper/modulestruct.Item.html
        //
        // That is, we should point inside the module, rather than at the
        // re-export.
        check(
            r#"
pub mod wrapper {
    pub use module::Item;

    pub mod module {
        pub struct Item;
    }
}

fn foo() {
    let bar: wrapper::It$0em;
}
        "#,
            expect![[r#"https://docs.rs/test/*/test/wrapper/struct.Item.html"#]],
        )
    }
}
