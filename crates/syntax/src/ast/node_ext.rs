//! Various extension methods to ast Nodes, which are hard to code-generate.
//! Extensions for various expressions live in a sibling `expr_extensions` module.

use std::{borrow::Cow, fmt, iter::successors};

use itertools::Itertools;
use parser::SyntaxKind;
use rowan::{GreenNodeData, GreenTokenData, WalkEvent};

use crate::{
    ast::{self, support, AstChildren, AstNode, AstToken, AttrsOwner, NameOwner, SyntaxNode},
    NodeOrToken, SmolStr, SyntaxElement, SyntaxToken, TokenText, T,
};

impl ast::Lifetime {
    pub fn text(&self) -> TokenText<'_> {
        text_of_first_token(self.syntax())
    }
}

impl ast::Name {
    pub fn text(&self) -> TokenText<'_> {
        text_of_first_token(self.syntax())
    }
}

impl ast::NameRef {
    pub fn text(&self) -> TokenText<'_> {
        text_of_first_token(self.syntax())
    }

    pub fn as_tuple_field(&self) -> Option<usize> {
        self.text().parse().ok()
    }
}

fn text_of_first_token(node: &SyntaxNode) -> TokenText<'_> {
    fn first_token(green_ref: &GreenNodeData) -> &GreenTokenData {
        green_ref.children().next().and_then(NodeOrToken::into_token).unwrap()
    }

    match node.green() {
        Cow::Borrowed(green_ref) => TokenText::borrowed(first_token(green_ref).text()),
        Cow::Owned(green) => TokenText::owned(first_token(&green).to_owned()),
    }
}

impl ast::BlockExpr {
    pub fn items(&self) -> AstChildren<ast::Item> {
        support::children(self.syntax())
    }

    pub fn is_empty(&self) -> bool {
        self.statements().next().is_none() && self.tail_expr().is_none()
    }
}

impl ast::Expr {
    /// Preorder walk all the expression's child expressions.
    pub fn walk(&self, cb: &mut dyn FnMut(ast::Expr)) {
        let mut preorder = self.syntax().preorder();
        while let Some(event) = preorder.next() {
            let node = match event {
                WalkEvent::Enter(node) => node,
                WalkEvent::Leave(_) => continue,
            };
            match ast::Stmt::cast(node.clone()) {
                // recursively walk the initializer, skipping potential const pat expressions
                // let statements aren't usually nested too deeply so this is fine to recurse on
                Some(ast::Stmt::LetStmt(l)) => {
                    if let Some(expr) = l.initializer() {
                        expr.walk(cb);
                    }
                    preorder.skip_subtree();
                }
                // Don't skip subtree since we want to process the expression child next
                Some(ast::Stmt::ExprStmt(_)) => (),
                // skip inner items which might have their own expressions
                Some(ast::Stmt::Item(_)) => preorder.skip_subtree(),
                None => {
                    // skip const args, those expressions are a different context
                    if ast::GenericArg::can_cast(node.kind()) {
                        preorder.skip_subtree();
                    } else if let Some(expr) = ast::Expr::cast(node) {
                        let is_different_context = match &expr {
                            ast::Expr::EffectExpr(effect) => {
                                matches!(
                                    effect.effect(),
                                    ast::Effect::Async(_)
                                        | ast::Effect::Try(_)
                                        | ast::Effect::Const(_)
                                )
                            }
                            ast::Expr::ClosureExpr(__) => true,
                            _ => false,
                        };
                        cb(expr);
                        if is_different_context {
                            preorder.skip_subtree();
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Macro {
    MacroRules(ast::MacroRules),
    MacroDef(ast::MacroDef),
}

impl From<ast::MacroRules> for Macro {
    fn from(it: ast::MacroRules) -> Self {
        Macro::MacroRules(it)
    }
}

impl From<ast::MacroDef> for Macro {
    fn from(it: ast::MacroDef) -> Self {
        Macro::MacroDef(it)
    }
}

impl AstNode for Macro {
    fn can_cast(kind: SyntaxKind) -> bool {
        matches!(kind, SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF)
    }
    fn cast(syntax: SyntaxNode) -> Option<Self> {
        let res = match syntax.kind() {
            SyntaxKind::MACRO_RULES => Macro::MacroRules(ast::MacroRules { syntax }),
            SyntaxKind::MACRO_DEF => Macro::MacroDef(ast::MacroDef { syntax }),
            _ => return None,
        };
        Some(res)
    }
    fn syntax(&self) -> &SyntaxNode {
        match self {
            Macro::MacroRules(it) => it.syntax(),
            Macro::MacroDef(it) => it.syntax(),
        }
    }
}

impl NameOwner for Macro {
    fn name(&self) -> Option<ast::Name> {
        match self {
            Macro::MacroRules(mac) => mac.name(),
            Macro::MacroDef(mac) => mac.name(),
        }
    }
}

impl AttrsOwner for Macro {}

/// Basically an owned `dyn AttrsOwner` without extra boxing.
pub struct AttrsOwnerNode {
    node: SyntaxNode,
}

impl AttrsOwnerNode {
    pub fn new<N: AttrsOwner>(node: N) -> Self {
        AttrsOwnerNode { node: node.syntax().clone() }
    }
}

impl AttrsOwner for AttrsOwnerNode {}
impl AstNode for AttrsOwnerNode {
    fn can_cast(_: SyntaxKind) -> bool
    where
        Self: Sized,
    {
        false
    }
    fn cast(_: SyntaxNode) -> Option<Self>
    where
        Self: Sized,
    {
        None
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.node
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrKind {
    Inner,
    Outer,
}

impl AttrKind {
    /// Returns `true` if the attr_kind is [`Inner`].
    pub fn is_inner(&self) -> bool {
        matches!(self, Self::Inner)
    }

    /// Returns `true` if the attr_kind is [`Outer`].
    pub fn is_outer(&self) -> bool {
        matches!(self, Self::Outer)
    }
}

impl ast::Attr {
    pub fn as_simple_atom(&self) -> Option<SmolStr> {
        let meta = self.meta()?;
        if meta.eq_token().is_some() || meta.token_tree().is_some() {
            return None;
        }
        self.simple_name()
    }

    pub fn as_simple_call(&self) -> Option<(SmolStr, ast::TokenTree)> {
        let tt = self.meta()?.token_tree()?;
        Some((self.simple_name()?, tt))
    }

    pub fn simple_name(&self) -> Option<SmolStr> {
        let path = self.meta()?.path()?;
        match (path.segment(), path.qualifier()) {
            (Some(segment), None) => Some(segment.syntax().first_token()?.text().into()),
            _ => None,
        }
    }

    pub fn kind(&self) -> AttrKind {
        let first_token = self.syntax().first_token();
        let first_token_kind = first_token.as_ref().map(SyntaxToken::kind);
        let second_token_kind =
            first_token.and_then(|token| token.next_token()).as_ref().map(SyntaxToken::kind);

        match (first_token_kind, second_token_kind) {
            (Some(T![#]), Some(T![!])) => AttrKind::Inner,
            _ => AttrKind::Outer,
        }
    }

    pub fn path(&self) -> Option<ast::Path> {
        self.meta()?.path()
    }

    pub fn expr(&self) -> Option<ast::Expr> {
        self.meta()?.expr()
    }

    pub fn token_tree(&self) -> Option<ast::TokenTree> {
        self.meta()?.token_tree()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSegmentKind {
    Name(ast::NameRef),
    Type { type_ref: Option<ast::Type>, trait_ref: Option<ast::PathType> },
    SelfKw,
    SuperKw,
    CrateKw,
}

impl ast::PathSegment {
    pub fn parent_path(&self) -> ast::Path {
        self.syntax()
            .parent()
            .and_then(ast::Path::cast)
            .expect("segments are always nested in paths")
    }

    pub fn crate_token(&self) -> Option<SyntaxToken> {
        self.name_ref().and_then(|it| it.crate_token())
    }

    pub fn self_token(&self) -> Option<SyntaxToken> {
        self.name_ref().and_then(|it| it.self_token())
    }

    pub fn super_token(&self) -> Option<SyntaxToken> {
        self.name_ref().and_then(|it| it.super_token())
    }

    pub fn kind(&self) -> Option<PathSegmentKind> {
        let res = if let Some(name_ref) = self.name_ref() {
            match name_ref.syntax().first_token().map(|it| it.kind()) {
                Some(T![self]) => PathSegmentKind::SelfKw,
                Some(T![super]) => PathSegmentKind::SuperKw,
                Some(T![crate]) => PathSegmentKind::CrateKw,
                _ => PathSegmentKind::Name(name_ref),
            }
        } else {
            match self.syntax().first_child_or_token()?.kind() {
                T![<] => {
                    // <T> or <T as Trait>
                    // T is any TypeRef, Trait has to be a PathType
                    let mut type_refs =
                        self.syntax().children().filter(|node| ast::Type::can_cast(node.kind()));
                    let type_ref = type_refs.next().and_then(ast::Type::cast);
                    let trait_ref = type_refs.next().and_then(ast::PathType::cast);
                    PathSegmentKind::Type { type_ref, trait_ref }
                }
                _ => return None,
            }
        };
        Some(res)
    }
}

impl ast::Path {
    pub fn parent_path(&self) -> Option<ast::Path> {
        self.syntax().parent().and_then(ast::Path::cast)
    }

    pub fn as_single_segment(&self) -> Option<ast::PathSegment> {
        match self.qualifier() {
            Some(_) => None,
            None => self.segment(),
        }
    }

    pub fn as_single_name_ref(&self) -> Option<ast::NameRef> {
        match self.qualifier() {
            Some(_) => None,
            None => self.segment()?.name_ref(),
        }
    }

    pub fn first_qualifier_or_self(&self) -> ast::Path {
        successors(Some(self.clone()), ast::Path::qualifier).last().unwrap()
    }

    pub fn first_segment(&self) -> Option<ast::PathSegment> {
        self.first_qualifier_or_self().segment()
    }

    pub fn segments(&self) -> impl Iterator<Item = ast::PathSegment> + Clone {
        successors(self.first_segment(), |p| {
            p.parent_path().parent_path().and_then(|p| p.segment())
        })
    }

    pub fn qualifiers(&self) -> impl Iterator<Item = ast::Path> + Clone {
        successors(self.qualifier(), |p| p.qualifier())
    }

    pub fn top_path(&self) -> ast::Path {
        let mut this = self.clone();
        while let Some(path) = this.parent_path() {
            this = path;
        }
        this
    }
}

impl ast::Use {
    pub fn is_simple_glob(&self) -> bool {
        self.use_tree()
            .map(|use_tree| use_tree.use_tree_list().is_none() && use_tree.star_token().is_some())
            .unwrap_or(false)
    }
}

impl ast::UseTree {
    pub fn is_simple_path(&self) -> bool {
        self.use_tree_list().is_none() && self.star_token().is_none()
    }
}

impl ast::UseTreeList {
    pub fn parent_use_tree(&self) -> ast::UseTree {
        self.syntax()
            .parent()
            .and_then(ast::UseTree::cast)
            .expect("UseTreeLists are always nested in UseTrees")
    }

    pub fn has_inner_comment(&self) -> bool {
        self.syntax()
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .find_map(ast::Comment::cast)
            .is_some()
    }
}

impl ast::Impl {
    pub fn self_ty(&self) -> Option<ast::Type> {
        match self.target() {
            (Some(t), None) | (_, Some(t)) => Some(t),
            _ => None,
        }
    }

    pub fn trait_(&self) -> Option<ast::Type> {
        match self.target() {
            (Some(t), Some(_)) => Some(t),
            _ => None,
        }
    }

    fn target(&self) -> (Option<ast::Type>, Option<ast::Type>) {
        let mut types = support::children(self.syntax());
        let first = types.next();
        let second = types.next();
        (first, second)
    }

    pub fn for_trait_name_ref(name_ref: &ast::NameRef) -> Option<ast::Impl> {
        let this = name_ref.syntax().ancestors().find_map(ast::Impl::cast)?;
        if this.trait_()?.syntax().text_range().start() == name_ref.syntax().text_range().start() {
            Some(this)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StructKind {
    Record(ast::RecordFieldList),
    Tuple(ast::TupleFieldList),
    Unit,
}

impl StructKind {
    fn from_node<N: AstNode>(node: &N) -> StructKind {
        if let Some(nfdl) = support::child::<ast::RecordFieldList>(node.syntax()) {
            StructKind::Record(nfdl)
        } else if let Some(pfl) = support::child::<ast::TupleFieldList>(node.syntax()) {
            StructKind::Tuple(pfl)
        } else {
            StructKind::Unit
        }
    }
}

impl ast::Struct {
    pub fn kind(&self) -> StructKind {
        StructKind::from_node(self)
    }
}

impl ast::RecordExprField {
    pub fn for_field_name(field_name: &ast::NameRef) -> Option<ast::RecordExprField> {
        let candidate = Self::for_name_ref(field_name)?;
        if candidate.field_name().as_ref() == Some(field_name) {
            Some(candidate)
        } else {
            None
        }
    }

    pub fn for_name_ref(name_ref: &ast::NameRef) -> Option<ast::RecordExprField> {
        let syn = name_ref.syntax();
        syn.parent()
            .and_then(ast::RecordExprField::cast)
            .or_else(|| syn.ancestors().nth(4).and_then(ast::RecordExprField::cast))
    }

    /// Deals with field init shorthand
    pub fn field_name(&self) -> Option<ast::NameRef> {
        if let Some(name_ref) = self.name_ref() {
            return Some(name_ref);
        }
        self.expr()?.name_ref()
    }
}

#[derive(Debug, Clone)]
pub enum NameLike {
    NameRef(ast::NameRef),
    Name(ast::Name),
    Lifetime(ast::Lifetime),
}

impl NameLike {
    pub fn as_name_ref(&self) -> Option<&ast::NameRef> {
        match self {
            NameLike::NameRef(name_ref) => Some(name_ref),
            _ => None,
        }
    }
}

impl ast::AstNode for NameLike {
    fn can_cast(kind: SyntaxKind) -> bool {
        matches!(kind, SyntaxKind::NAME | SyntaxKind::NAME_REF | SyntaxKind::LIFETIME)
    }
    fn cast(syntax: SyntaxNode) -> Option<Self> {
        let res = match syntax.kind() {
            SyntaxKind::NAME => NameLike::Name(ast::Name { syntax }),
            SyntaxKind::NAME_REF => NameLike::NameRef(ast::NameRef { syntax }),
            SyntaxKind::LIFETIME => NameLike::Lifetime(ast::Lifetime { syntax }),
            _ => return None,
        };
        Some(res)
    }
    fn syntax(&self) -> &SyntaxNode {
        match self {
            NameLike::NameRef(it) => it.syntax(),
            NameLike::Name(it) => it.syntax(),
            NameLike::Lifetime(it) => it.syntax(),
        }
    }
}

mod __ {
    use super::{
        ast::{Lifetime, Name, NameRef},
        NameLike,
    };
    stdx::impl_from!(NameRef, Name, Lifetime for NameLike);
}

#[derive(Debug, Clone, PartialEq)]
pub enum NameOrNameRef {
    Name(ast::Name),
    NameRef(ast::NameRef),
}

impl fmt::Display for NameOrNameRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NameOrNameRef::Name(it) => fmt::Display::fmt(it, f),
            NameOrNameRef::NameRef(it) => fmt::Display::fmt(it, f),
        }
    }
}

impl NameOrNameRef {
    pub fn text(&self) -> TokenText<'_> {
        match self {
            NameOrNameRef::Name(name) => name.text(),
            NameOrNameRef::NameRef(name_ref) => name_ref.text(),
        }
    }
}

impl ast::RecordPatField {
    pub fn for_field_name_ref(field_name: &ast::NameRef) -> Option<ast::RecordPatField> {
        let candidate = field_name.syntax().parent().and_then(ast::RecordPatField::cast)?;
        match candidate.field_name()? {
            NameOrNameRef::NameRef(name_ref) if name_ref == *field_name => Some(candidate),
            _ => None,
        }
    }

    pub fn for_field_name(field_name: &ast::Name) -> Option<ast::RecordPatField> {
        let candidate =
            field_name.syntax().ancestors().nth(2).and_then(ast::RecordPatField::cast)?;
        match candidate.field_name()? {
            NameOrNameRef::Name(name) if name == *field_name => Some(candidate),
            _ => None,
        }
    }

    /// Deals with field init shorthand
    pub fn field_name(&self) -> Option<NameOrNameRef> {
        if let Some(name_ref) = self.name_ref() {
            return Some(NameOrNameRef::NameRef(name_ref));
        }
        match self.pat() {
            Some(ast::Pat::IdentPat(pat)) => {
                let name = pat.name()?;
                Some(NameOrNameRef::Name(name))
            }
            Some(ast::Pat::BoxPat(pat)) => match pat.pat() {
                Some(ast::Pat::IdentPat(pat)) => {
                    let name = pat.name()?;
                    Some(NameOrNameRef::Name(name))
                }
                _ => None,
            },
            _ => None,
        }
    }
}

impl ast::Variant {
    pub fn parent_enum(&self) -> ast::Enum {
        self.syntax()
            .parent()
            .and_then(|it| it.parent())
            .and_then(ast::Enum::cast)
            .expect("EnumVariants are always nested in Enums")
    }
    pub fn kind(&self) -> StructKind {
        StructKind::from_node(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldKind {
    Name(ast::NameRef),
    Index(SyntaxToken),
}

impl ast::FieldExpr {
    pub fn index_token(&self) -> Option<SyntaxToken> {
        self.syntax
            .children_with_tokens()
            // FIXME: Accepting floats here to reject them in validation later
            .find(|c| c.kind() == SyntaxKind::INT_NUMBER || c.kind() == SyntaxKind::FLOAT_NUMBER)
            .as_ref()
            .and_then(SyntaxElement::as_token)
            .cloned()
    }

    pub fn field_access(&self) -> Option<FieldKind> {
        if let Some(nr) = self.name_ref() {
            Some(FieldKind::Name(nr))
        } else {
            self.index_token().map(FieldKind::Index)
        }
    }
}

pub struct SlicePatComponents {
    pub prefix: Vec<ast::Pat>,
    pub slice: Option<ast::Pat>,
    pub suffix: Vec<ast::Pat>,
}

impl ast::SlicePat {
    pub fn components(&self) -> SlicePatComponents {
        let mut args = self.pats().peekable();
        let prefix = args
            .peeking_take_while(|p| match p {
                ast::Pat::RestPat(_) => false,
                ast::Pat::IdentPat(bp) => !matches!(bp.pat(), Some(ast::Pat::RestPat(_))),
                ast::Pat::RefPat(rp) => match rp.pat() {
                    Some(ast::Pat::RestPat(_)) => false,
                    Some(ast::Pat::IdentPat(bp)) => !matches!(bp.pat(), Some(ast::Pat::RestPat(_))),
                    _ => true,
                },
                _ => true,
            })
            .collect();
        let slice = args.next();
        let suffix = args.collect();

        SlicePatComponents { prefix, slice, suffix }
    }
}

impl ast::IdentPat {
    pub fn is_simple_ident(&self) -> bool {
        self.at_token().is_none()
            && self.mut_token().is_none()
            && self.ref_token().is_none()
            && self.pat().is_none()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum SelfParamKind {
    /// self
    Owned,
    /// &self
    Ref,
    /// &mut self
    MutRef,
}

impl ast::SelfParam {
    pub fn kind(&self) -> SelfParamKind {
        if self.amp_token().is_some() {
            if self.mut_token().is_some() {
                SelfParamKind::MutRef
            } else {
                SelfParamKind::Ref
            }
        } else {
            SelfParamKind::Owned
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TypeBoundKind {
    /// Trait
    PathType(ast::PathType),
    /// for<'a> ...
    ForType(ast::ForType),
    /// 'a
    Lifetime(ast::Lifetime),
}

impl ast::TypeBound {
    pub fn kind(&self) -> TypeBoundKind {
        if let Some(path_type) = support::children(self.syntax()).next() {
            TypeBoundKind::PathType(path_type)
        } else if let Some(for_type) = support::children(self.syntax()).next() {
            TypeBoundKind::ForType(for_type)
        } else if let Some(lifetime) = self.lifetime() {
            TypeBoundKind::Lifetime(lifetime)
        } else {
            unreachable!()
        }
    }
}

pub enum VisibilityKind {
    In(ast::Path),
    PubCrate,
    PubSuper,
    PubSelf,
    Pub,
}

impl ast::Visibility {
    pub fn kind(&self) -> VisibilityKind {
        match self.path() {
            Some(path) => {
                if let Some(segment) =
                    path.as_single_segment().filter(|it| it.coloncolon_token().is_none())
                {
                    if segment.crate_token().is_some() {
                        return VisibilityKind::PubCrate;
                    } else if segment.super_token().is_some() {
                        return VisibilityKind::PubSuper;
                    } else if segment.self_token().is_some() {
                        return VisibilityKind::PubSelf;
                    }
                }
                VisibilityKind::In(path)
            }
            None => VisibilityKind::Pub,
        }
    }

    pub fn is_eq_to(&self, other: &Self) -> bool {
        match (self.kind(), other.kind()) {
            (VisibilityKind::In(this), VisibilityKind::In(other)) => {
                stdx::iter_eq_by(this.segments(), other.segments(), |lhs, rhs| {
                    lhs.kind().zip(rhs.kind()).map_or(false, |it| match it {
                        (PathSegmentKind::CrateKw, PathSegmentKind::CrateKw)
                        | (PathSegmentKind::SelfKw, PathSegmentKind::SelfKw)
                        | (PathSegmentKind::SuperKw, PathSegmentKind::SuperKw) => true,
                        (PathSegmentKind::Name(lhs), PathSegmentKind::Name(rhs)) => {
                            lhs.text() == rhs.text()
                        }
                        _ => false,
                    })
                })
            }
            (VisibilityKind::PubSelf, VisibilityKind::PubSelf)
            | (VisibilityKind::PubSuper, VisibilityKind::PubSuper)
            | (VisibilityKind::PubCrate, VisibilityKind::PubCrate)
            | (VisibilityKind::Pub, VisibilityKind::Pub) => true,
            _ => false,
        }
    }
}

impl ast::LifetimeParam {
    pub fn lifetime_bounds(&self) -> impl Iterator<Item = SyntaxToken> {
        self.syntax()
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .skip_while(|x| x.kind() != T![:])
            .filter(|it| it.kind() == T![lifetime_ident])
    }
}

impl ast::Module {
    /// Returns the parent ast::Module, this is different than the semantic parent in that this only
    /// considers parent declarations in the AST
    pub fn parent(&self) -> Option<ast::Module> {
        self.syntax().ancestors().nth(2).and_then(ast::Module::cast)
    }
}

impl ast::RangePat {
    pub fn start(&self) -> Option<ast::Pat> {
        self.syntax()
            .children_with_tokens()
            .take_while(|it| !(it.kind() == T![..] || it.kind() == T![..=]))
            .filter_map(|it| it.into_node())
            .find_map(ast::Pat::cast)
    }

    pub fn end(&self) -> Option<ast::Pat> {
        self.syntax()
            .children_with_tokens()
            .skip_while(|it| !(it.kind() == T![..] || it.kind() == T![..=]))
            .filter_map(|it| it.into_node())
            .find_map(ast::Pat::cast)
    }
}

impl ast::TokenTree {
    pub fn left_delimiter_token(&self) -> Option<SyntaxToken> {
        self.syntax()
            .first_child_or_token()?
            .into_token()
            .filter(|it| matches!(it.kind(), T!['{'] | T!['('] | T!['[']))
    }

    pub fn right_delimiter_token(&self) -> Option<SyntaxToken> {
        self.syntax()
            .last_child_or_token()?
            .into_token()
            .filter(|it| matches!(it.kind(), T!['}'] | T![')'] | T![']']))
    }
}

impl ast::GenericParamList {
    pub fn lifetime_params(&self) -> impl Iterator<Item = ast::LifetimeParam> {
        self.generic_params().filter_map(|param| match param {
            ast::GenericParam::LifetimeParam(it) => Some(it),
            ast::GenericParam::TypeParam(_) | ast::GenericParam::ConstParam(_) => None,
        })
    }
    pub fn type_params(&self) -> impl Iterator<Item = ast::TypeParam> {
        self.generic_params().filter_map(|param| match param {
            ast::GenericParam::TypeParam(it) => Some(it),
            ast::GenericParam::LifetimeParam(_) | ast::GenericParam::ConstParam(_) => None,
        })
    }
    pub fn const_params(&self) -> impl Iterator<Item = ast::ConstParam> {
        self.generic_params().filter_map(|param| match param {
            ast::GenericParam::ConstParam(it) => Some(it),
            ast::GenericParam::TypeParam(_) | ast::GenericParam::LifetimeParam(_) => None,
        })
    }
}

impl ast::DocCommentsOwner for ast::SourceFile {}
impl ast::DocCommentsOwner for ast::Fn {}
impl ast::DocCommentsOwner for ast::Struct {}
impl ast::DocCommentsOwner for ast::Union {}
impl ast::DocCommentsOwner for ast::RecordField {}
impl ast::DocCommentsOwner for ast::TupleField {}
impl ast::DocCommentsOwner for ast::Enum {}
impl ast::DocCommentsOwner for ast::Variant {}
impl ast::DocCommentsOwner for ast::Trait {}
impl ast::DocCommentsOwner for ast::Module {}
impl ast::DocCommentsOwner for ast::Static {}
impl ast::DocCommentsOwner for ast::Const {}
impl ast::DocCommentsOwner for ast::TypeAlias {}
impl ast::DocCommentsOwner for ast::Impl {}
impl ast::DocCommentsOwner for ast::MacroRules {}
impl ast::DocCommentsOwner for ast::MacroDef {}
impl ast::DocCommentsOwner for ast::Macro {}
impl ast::DocCommentsOwner for ast::Use {}
