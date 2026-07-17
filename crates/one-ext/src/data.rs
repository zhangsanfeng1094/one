//! Session-scoped type map for extension state (Codex `ExtensionData` analogue).

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Mutex;

/// Type-erased store keyed by `TypeId`.
///
/// Extensions insert/get their own config or runtime handles without coupling
/// the host to concrete types.
#[derive(Default)]
pub struct ExtensionData {
    inner: Mutex<HashMap<TypeId, Box<dyn Any + Send + Sync>>>,
}

impl ExtensionData {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert<T: Any + Send + Sync>(&self, value: T) {
        self.inner
            .lock()
            .expect("extension data lock")
            .insert(TypeId::of::<T>(), Box::new(value));
    }

    pub fn get<T: Any + Send + Sync>(&self) -> Option<T>
    where
        T: Clone,
    {
        self.inner
            .lock()
            .expect("extension data lock")
            .get(&TypeId::of::<T>())
            .and_then(|v| v.downcast_ref::<T>().cloned())
    }

    pub fn with<T: Any + Send + Sync, R>(&self, f: impl FnOnce(Option<&T>) -> R) -> R {
        let guard = self.inner.lock().expect("extension data lock");
        let val = guard
            .get(&TypeId::of::<T>())
            .and_then(|v| v.downcast_ref::<T>());
        f(val)
    }

    pub fn with_mut<T: Any + Send + Sync, R>(&self, f: impl FnOnce(Option<&mut T>) -> R) -> R {
        let mut guard = self.inner.lock().expect("extension data lock");
        let val = guard
            .get_mut(&TypeId::of::<T>())
            .and_then(|v| v.downcast_mut::<T>());
        f(val)
    }

    pub fn remove<T: Any + Send + Sync>(&self) -> Option<T> {
        self.inner
            .lock()
            .expect("extension data lock")
            .remove(&TypeId::of::<T>())
            .and_then(|v| v.downcast::<T>().ok().map(|b| *b))
    }

    pub fn clear(&self) {
        self.inner.lock().expect("extension data lock").clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_get_roundtrip() {
        let data = ExtensionData::new();
        data.insert(42u32);
        assert_eq!(data.get::<u32>(), Some(42));
        data.with_mut::<u32, _>(|v| {
            if let Some(n) = v {
                *n += 1;
            }
        });
        assert_eq!(data.get::<u32>(), Some(43));
    }
}
