//! See [`Name`].

use std::fmt;

use syntax::{ast, SmolStr};

/// `Name` is a wrapper around string, which is used in hir for both references
/// and declarations. In theory, names should also carry hygiene info, but we are
/// not there yet!
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Name(Repr);

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Repr {
    Text(SmolStr),
    TupleField(usize),
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.0 {
            Repr::Text(text) => fmt::Display::fmt(&text, f),
            Repr::TupleField(idx) => fmt::Display::fmt(&idx, f),
        }
    }
}

impl Name {
    /// Note: this is private to make creating name from random string hard.
    /// Hopefully, this should allow us to integrate hygiene cleaner in the
    /// future, and to switch to interned representation of names.
    const fn new_text(text: SmolStr) -> Name {
        Name(Repr::Text(text))
    }

    pub fn new_tuple_field(idx: usize) -> Name {
        Name(Repr::TupleField(idx))
    }

    pub fn new_lifetime(lt: &ast::Lifetime) -> Name {
        Self::new_text(lt.text().into())
    }

    /// Shortcut to create inline plain text name
    const fn new_inline(text: &str) -> Name {
        Name::new_text(SmolStr::new_inline(text))
    }

    /// Resolve a name from the text of token.
    fn resolve(raw_text: &str) -> Name {
        if let Some(text) = raw_text.strip_prefix("r#") {
            Name::new_text(SmolStr::new(text))
        } else {
            Name::new_text(raw_text.into())
        }
    }

    /// A fake name for things missing in the source code.
    ///
    /// For example, `impl Foo for {}` should be treated as a trait impl for a
    /// type with a missing name. Similarly, `struct S { : u32 }` should have a
    /// single field with a missing name.
    ///
    /// Ideally, we want a `gensym` semantics for missing names -- each missing
    /// name is equal only to itself. It's not clear how to implement this in
    /// salsa though, so we punt on that bit for a moment.
    pub fn missing() -> Name {
        Name::new_text("[missing name]".into())
    }

    pub fn as_tuple_index(&self) -> Option<usize> {
        match self.0 {
            Repr::TupleField(idx) => Some(idx),
            _ => None,
        }
    }
}

pub trait AsName {
    fn as_name(&self) -> Name;
}

impl AsName for ast::NameRef {
    fn as_name(&self) -> Name {
        match self.as_tuple_field() {
            Some(idx) => Name::new_tuple_field(idx),
            None => Name::resolve(&self.text()),
        }
    }
}

impl AsName for ast::Name {
    fn as_name(&self) -> Name {
        Name::resolve(&self.text())
    }
}

impl AsName for ast::NameOrNameRef {
    fn as_name(&self) -> Name {
        match self {
            ast::NameOrNameRef::Name(it) => it.as_name(),
            ast::NameOrNameRef::NameRef(it) => it.as_name(),
        }
    }
}

impl AsName for tt::Ident {
    fn as_name(&self) -> Name {
        Name::resolve(&self.text)
    }
}

impl AsName for ast::FieldKind {
    fn as_name(&self) -> Name {
        match self {
            ast::FieldKind::Name(nr) => nr.as_name(),
            ast::FieldKind::Index(idx) => {
                let idx = idx.text().parse::<usize>().unwrap_or(0);
                Name::new_tuple_field(idx)
            }
        }
    }
}

impl AsName for base_db::Dependency {
    fn as_name(&self) -> Name {
        Name::new_text(SmolStr::new(&*self.name))
    }
}

pub mod known {
    macro_rules! known_names {
        ($($ident:ident),* $(,)?) => {
            $(
                #[allow(bad_style)]
                pub const $ident: super::Name =
                    super::Name::new_inline(stringify!($ident));
            )*
        };
    }

    known_names!(
        // Primitives
        isize,
        i8,
        i16,
        i32,
        i64,
        i128,
        usize,
        u8,
        u16,
        u32,
        u64,
        u128,
        f32,
        f64,
        bool,
        char,
        str,
        // Special names
        macro_rules,
        doc,
        cfg,
        cfg_attr,
        register_attr,
        register_tool,
        // Components of known path (value or mod name)
        std,
        core,
        alloc,
        iter,
        ops,
        future,
        result,
        boxed,
        option,
        prelude,
        rust_2015,
        rust_2018,
        rust_2021,
        v1,
        // Components of known path (type name)
        Iterator,
        IntoIterator,
        Item,
        Try,
        Ok,
        Future,
        Result,
        Option,
        Output,
        Target,
        Box,
        RangeFrom,
        RangeFull,
        RangeInclusive,
        RangeToInclusive,
        RangeTo,
        Range,
        Neg,
        Not,
        Index,
        // Components of known path (function name)
        filter_map,
        next,
        iter_mut,
        len,
        is_empty,
        // Builtin macros
        file,
        column,
        compile_error,
        line,
        module_path,
        assert,
        core_panic,
        std_panic,
        stringify,
        concat,
        concat_idents,
        include,
        include_bytes,
        include_str,
        format_args,
        format_args_nl,
        env,
        option_env,
        llvm_asm,
        asm,
        global_asm,
        // Builtin derives
        Copy,
        Clone,
        Default,
        Debug,
        Hash,
        Ord,
        PartialOrd,
        Eq,
        PartialEq,
        // Builtin attributes
        bench,
        cfg_accessible,
        cfg_eval,
        derive,
        global_allocator,
        test,
        test_case,
        // Safe intrinsics
        abort,
        add_with_overflow,
        bitreverse,
        bswap,
        caller_location,
        ctlz,
        ctpop,
        cttz,
        discriminant_value,
        forget,
        likely,
        maxnumf32,
        maxnumf64,
        min_align_of_val,
        min_align_of,
        minnumf32,
        minnumf64,
        mul_with_overflow,
        needs_drop,
        ptr_guaranteed_eq,
        ptr_guaranteed_ne,
        rotate_left,
        rotate_right,
        rustc_peek,
        saturating_add,
        saturating_sub,
        size_of_val,
        size_of,
        sub_with_overflow,
        type_id,
        type_name,
        unlikely,
        variant_count,
        wrapping_add,
        wrapping_mul,
        wrapping_sub,
    );

    // self/Self cannot be used as an identifier
    pub const SELF_PARAM: super::Name = super::Name::new_inline("self");
    pub const SELF_TYPE: super::Name = super::Name::new_inline("Self");

    pub const STATIC_LIFETIME: super::Name = super::Name::new_inline("'static");

    #[macro_export]
    macro_rules! name {
        (self) => {
            $crate::name::known::SELF_PARAM
        };
        (Self) => {
            $crate::name::known::SELF_TYPE
        };
        ('static) => {
            $crate::name::known::STATIC_LIFETIME
        };
        ($ident:ident) => {
            $crate::name::known::$ident
        };
    }
}

pub use crate::name;
