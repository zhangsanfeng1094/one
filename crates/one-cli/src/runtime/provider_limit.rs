//! Global LLM concurrency permit (parent + all sub-agents share one pool).
//!
//! Logical `task` parallelism may be higher; physical LLM calls queue here so
//! local models (concurrency=1) and tight API slots do not deadlock.

use std::sync::{Arc, OnceLock};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Default physical LLM slots when `ONE_LLM_CONCURRENCY` is unset.
const DEFAULT_LLM_CONCURRENCY: usize = 4;

static GLOBAL: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn parse_limit() -> usize {
    std::env::var("ONE_LLM_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT_LLM_CONCURRENCY)
}

/// Process-wide semaphore for LLM completions.
pub fn global_llm_semaphore() -> Arc<Semaphore> {
    GLOBAL
        .get_or_init(|| Arc::new(Semaphore::new(parse_limit())))
        .clone()
}

/// Acquire one LLM slot (async). Hold the permit for the duration of a completion.
pub async fn acquire_llm_permit() -> OwnedSemaphorePermit {
    global_llm_semaphore()
        .acquire_owned()
        .await
        .expect("LLM semaphore closed")
}

/// Current configured concurrency (for status / tests).
pub fn llm_concurrency_limit() -> usize {
    parse_limit()
}
