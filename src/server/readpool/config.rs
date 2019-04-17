// Copyright 2018 TiKV Project Authors. Licensed under Apache-2.0.

use tikv_util::config::ReadableSize;

// Assume a request can be finished in 1ms, a request at position x will wait about
// 0.001 * x secs to be actual started. A server-is-busy error will trigger 2 seconds
// backoff. So when it needs to wait for more than 2 seconds, return error won't causse
// larger latency.
pub const DEFAULT_MAX_TASKS_PER_WORKER: usize = 2 as usize * 1000;

pub const DEFAULT_STACK_SIZE_MB: u64 = 10;

/// Configuration for the `ReadPool`.
#[derive(Debug, Clone)]
pub struct Config {
    pub high_concurrency: usize,
    pub normal_concurrency: usize,
    pub low_concurrency: usize,
    pub max_tasks_per_worker_high: usize,
    pub max_tasks_per_worker_normal: usize,
    pub max_tasks_per_worker_low: usize,
    pub stack_size: ReadableSize,
}

impl Config {
    /// A shortcut to construct Config with the specified concurrency.
    ///
    /// Note: it is only used in tests.
    #[doc(hidden)]
    pub fn default_with_concurrency(concurrency: usize) -> Self {
        Self {
            high_concurrency: concurrency,
            normal_concurrency: concurrency,
            low_concurrency: concurrency,
            max_tasks_per_worker_high: DEFAULT_MAX_TASKS_PER_WORKER,
            max_tasks_per_worker_normal: DEFAULT_MAX_TASKS_PER_WORKER,
            max_tasks_per_worker_low: DEFAULT_MAX_TASKS_PER_WORKER,
            stack_size: ReadableSize::mb(DEFAULT_STACK_SIZE_MB),
        }
    }

    #[doc(hidden)]
    pub fn default_for_test() -> Self {
        Self::default_with_concurrency(2)
    }
}
