pub mod builtin;
pub mod error;
pub mod loader;
pub mod runtime;
pub mod traits;
#[cfg(feature = "dylib")]
pub mod dylib;

pub use error::{ExtError, Result};
pub use loader::discover_extensions;
pub use runtime::ExtensionRuntime;
pub use traits::{Extension, ExtensionContext, ExtensionEvent};