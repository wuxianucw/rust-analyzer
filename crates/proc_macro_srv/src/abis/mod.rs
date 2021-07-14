//! Procedural macros are implemented by compiling the macro providing crate
//! to a dynamic library with a particular ABI which the compiler uses to expand
//! macros. Unfortunately this ABI is not specified and can change from version
//! to version of the compiler. To support this we copy the ABI from the rust
//! compiler into submodules of this module (e.g proc_macro_srv::abis::abi_1_47).
//!
//! All of these ABIs are subsumed in the `Abi` enum, which exposes a simple
//! interface the rest of rust analyzer can use to talk to the macro
//! provider.
//!
//! # Adding a new ABI
//!
//! To add a new ABI you'll need to copy the source of the target proc_macro
//! crate from the source tree of the Rust compiler into this directory tree.
//! Then you'll need to modify it
//! - Remove any feature! or other things which won't compile on stable
//! - change any absolute imports to relative imports within the ABI tree
//!
//! Then you'll need to add a branch to the `Abi` enum and an implementation of
//! `Abi::expand`, `Abi::list_macros` and `Abi::from_lib` for the new ABI. See
//! `proc_macro_srv/src/abis/abi_1_47/mod.rs` for an example. Finally you'll
//! need to update the conditionals in `Abi::from_lib` to return your new ABI
//! for the relevant versions of the rust compiler
//!

// pub(crate) so tests can use the TokenStream, more notes in test/utils.rs
pub(crate) mod abi_1_47;
mod abi_1_55;

use super::dylib::LoadProcMacroDylibError;
pub(crate) use abi_1_47::Abi as Abi_1_47;
pub(crate) use abi_1_55::Abi as Abi_1_55;
use libloading::Library;
use proc_macro_api::{ProcMacroKind, RustCInfo};

pub struct PanicMessage {
    message: Option<String>,
}

impl PanicMessage {
    pub fn as_str(&self) -> Option<String> {
        self.message.clone()
    }
}

pub(crate) enum Abi {
    Abi1_47(Abi_1_47),
    Abi1_55(Abi_1_55),
}

impl Abi {
    /// Load a new ABI.
    ///
    /// # Arguments
    ///
    /// *`lib` - The dynamic library containing the macro implementations
    /// *`symbol_name` - The symbol name the macros can be found attributes
    /// *`info` - RustCInfo about the compiler that was used to compile the
    ///           macro crate. This is the information we use to figure out
    ///           which ABI to return
    pub fn from_lib(
        lib: &Library,
        symbol_name: String,
        info: RustCInfo,
    ) -> Result<Abi, LoadProcMacroDylibError> {
        if info.version.0 != 1 {
            Err(LoadProcMacroDylibError::UnsupportedABI)
        } else if info.version.1 < 47 {
            Err(LoadProcMacroDylibError::UnsupportedABI)
        } else if info.version.1 < 54 {
            let inner = unsafe { Abi_1_47::from_lib(lib, symbol_name) }?;
            Ok(Abi::Abi1_47(inner))
        } else {
            let inner = unsafe { Abi_1_55::from_lib(lib, symbol_name) }?;
            Ok(Abi::Abi1_55(inner))
        }
    }

    pub fn expand(
        &self,
        macro_name: &str,
        macro_body: &tt::Subtree,
        attributes: Option<&tt::Subtree>,
    ) -> Result<tt::Subtree, PanicMessage> {
        match self {
            Self::Abi1_55(abi) => abi.expand(macro_name, macro_body, attributes),
            Self::Abi1_47(abi) => abi.expand(macro_name, macro_body, attributes),
        }
    }

    pub fn list_macros(&self) -> Vec<(String, ProcMacroKind)> {
        match self {
            Self::Abi1_47(abi) => abi.list_macros(),
            Self::Abi1_55(abi) => abi.list_macros(),
        }
    }
}
