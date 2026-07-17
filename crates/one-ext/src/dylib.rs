//! Optional dynamic extension loading via `libloading`.
//!
//! Extensions must export:
//! `extern "C" fn one_extension_name() -> *const c_char`
//!
//! Name is resolved against built-ins (stable ABI v0). Full out-of-tree
//! `Extension` trait objects are not yet supported across dylib boundaries
//! without a C ABI; this path loads known built-ins packaged as shared libs
//! for testing the loader.

use std::ffi::CStr;
use std::path::Path;
use std::sync::Arc;

use libloading::{Library, Symbol};

use crate::builtin::builtin_by_name;
use crate::traits::Extension;

pub fn load(path: &Path) -> crate::Result<Arc<dyn Extension>> {
    unsafe {
        let lib = Library::new(path).map_err(|e| crate::ExtError::Load(e.to_string()))?;
        let name_fn: Symbol<unsafe extern "C" fn() -> *const libc::c_char> = lib
            .get(b"one_extension_name")
            .map_err(|e| crate::ExtError::Load(e.to_string()))?;
        let name_ptr = name_fn();
        if name_ptr.is_null() {
            return Err(crate::ExtError::Load(
                "one_extension_name returned null".into(),
            ));
        }
        let name = CStr::from_ptr(name_ptr).to_string_lossy();
        // Keep library loaded for process lifetime by leaking (v0 ABI).
        std::mem::forget(lib);
        builtin_by_name(name.as_ref()).ok_or_else(|| {
            crate::ExtError::Load(format!("unknown dylib extension: {name}"))
        })
    }
}
