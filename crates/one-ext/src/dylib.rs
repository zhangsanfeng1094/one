//! Optional dynamic extension loading via `libloading`.
//!
//! Extensions must export:
//! `extern "C" fn one_extension_name() -> *const c_char`

use std::ffi::CStr;
use std::path::Path;
use std::sync::Arc;

use libloading::{Library, Symbol};

use crate::builtin::StatusExtension;
use crate::traits::Extension;

pub fn load(path: &Path) -> crate::Result<Arc<dyn Extension>> {
    unsafe {
        let lib = Library::new(path).map_err(|e| crate::ExtError::Load(e.to_string()))?;
        let name_fn: Symbol<unsafe extern "C" fn() -> *const libc::c_char> =
            lib.get(b"one_extension_name").map_err(|e| crate::ExtError::Load(e.to_string()))?;
        let name_ptr = name_fn();
        if name_ptr.is_null() {
            return Err(crate::ExtError::Load("one_extension_name returned null".into()));
        }
        let name = CStr::from_ptr(name_ptr).to_string_lossy();
        match name.as_ref() {
            "status" => Ok(Arc::new(StatusExtension::new())),
            other => Err(crate::ExtError::Load(format!(
                "unknown dylib extension: {other}"
            ))),
        }
    }
}