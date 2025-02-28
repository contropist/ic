use ic_metrics::{
    buckets::{decimal_buckets, decimal_buckets_with_zero},
    MetricsRegistry,
};
use ic_types::{
    NumInstructions, NumMessages, NumSlices, MAX_STABLE_MEMORY_IN_BYTES, MAX_WASM_MEMORY_IN_BYTES,
};
use prometheus::{Histogram, IntCounter};
use std::{cell::RefCell, rc::Rc, time::Instant};

pub(crate) const QUERY_HANDLER_CRITICAL_ERROR: &str = "query_handler_critical_error";

pub(crate) struct QueryHandlerMetrics {
    pub query: ScopedMetrics,
    pub query_initial_call: ScopedMetrics,
    pub query_retry_call: ScopedMetrics,
    pub query_spawned_calls: ScopedMetrics,
    pub query_cache_hits: IntCounter,
    pub query_cache_misses: IntCounter,
    pub query_cache_evicted_entries: IntCounter,
    pub query_cache_invalidated_entries: IntCounter,
    pub query_cache_count_bytes: Histogram,
    pub query_critical_error: IntCounter,
}

impl QueryHandlerMetrics {
    pub fn new(metrics_registry: &MetricsRegistry) -> Self {
        Self {
            query: ScopedMetrics {
                duration: duration_histogram(
                    "execution_query_duration_seconds",
                    "The duration of query handling",
                    metrics_registry,
                ),
                instructions: instructions_histogram(
                    "execution_query_instructions",
                    "The number of instructions executed in query handling",
                    metrics_registry,
                ),
                slices: slices_histogram(
                    "execution_query_slices",
                    "The number of slices executed in query handling",
                    metrics_registry,
                ),
                messages: messages_histogram(
                    "execution_query_messages",
                    "The number of messages executed in query handling",
                    metrics_registry,
                ),
            },
            query_initial_call: ScopedMetrics {
                duration: duration_histogram(
                    "execution_query_initial_call_duration_seconds",
                    "The duration of the initial call in query handling",
                    metrics_registry,
                ),
                instructions: instructions_histogram(
                    "execution_query_initial_call_instructions",
                    "The number of instructions executed in the initial call \
                    in query handling",
                    metrics_registry,
                ),
                slices: slices_histogram(
                    "execution_query_initial_call_slices",
                    "The number of slices executed in the initial call in \
                    query handling",
                    metrics_registry,
                ),
                messages: messages_histogram(
                    "execution_query_initial_call_messages",
                    "The number of messages executed in the initial call in \
                    query handling",
                    metrics_registry,
                ),
            },
            query_retry_call: ScopedMetrics {
                duration: duration_histogram(
                    "execution_query_retry_call_duration_seconds",
                    "The duration of the retry call in query handling",
                    metrics_registry,
                ),
                instructions: instructions_histogram(
                    "execution_query_retry_call_instructions",
                    "The number of instructions executed in the retry call \
                    in query handling",
                    metrics_registry,
                ),
                slices: slices_histogram(
                    "execution_query_retry_call_slices",
                    "The number of slices executed in the retry call in \
                    query handling",
                    metrics_registry,
                ),
                messages: messages_histogram(
                    "execution_query_retry_call_messages",
                    "The number of messages executed in the retry call in \
                    query handling",
                    metrics_registry,
                ),
            },
            query_spawned_calls: ScopedMetrics {
                duration: duration_histogram(
                    "execution_query_spawned_calls_duration_seconds",
                    "The duration of executing all calls spawned by the \
                    initial call in query handling",
                    metrics_registry,
                ),
                instructions: instructions_histogram(
                    "execution_query_spawned_calls_instructions",
                    "The number of instructions executed in calls spawned \
                    by the initial call in query handling",
                    metrics_registry,
                ),
                slices: messages_histogram(
                    "execution_query_spawned_calls_slices",
                    "The number of slices executed in calls spawned by \
                    the initial calls in query handling",
                    metrics_registry,
                ),
                messages: messages_histogram(
                    "execution_query_spawned_calls_messages",
                    "The number of messages executed in calls spawned by \
                    the initial calls in query handling",
                    metrics_registry,
                ),
            },
            query_cache_hits: metrics_registry.int_counter(
                "execution_query_cache_hits",
                "The number of replica side query cache hits",
            ),
            query_cache_misses: metrics_registry.int_counter(
                "execution_query_cache_misses",
                "The number of replica side query cache misses",
            ),
            query_cache_evicted_entries: metrics_registry.int_counter(
                "execution_query_cache_evicted_entries",
                "The number of evicted entries in the replica side query cache",
            ),
            query_cache_invalidated_entries: metrics_registry.int_counter(
                "execution_query_cache_invalidated_entries",
                "The number of invalidated entries in the replica side query cache",
            ),
            query_cache_count_bytes: memory_histogram(
                "execution_query_cache_count_bytes",
                "The replica side query cache size in bytes",
                metrics_registry,
            ),
            query_critical_error: metrics_registry.error_counter(QUERY_HANDLER_CRITICAL_ERROR),
        }
    }
}

/// A common set of metrics for various phases of execution
///
/// Currently the set includes:
/// - the duration of the phase,
/// - the number of instructions executed in the phase,
/// - the number of slices executed in the phase,
/// - the number of messages executed in the phase.
/// Use `MeasurementScope` instead of observing the metrics manually.
#[derive(Debug)]
pub(crate) struct ScopedMetrics {
    pub duration: Histogram,
    pub instructions: Histogram,
    pub slices: Histogram,
    pub messages: Histogram,
}

/// A convenience helper for measuring `ScopedMetrics`
///
/// It simplifies measuring of metrics for a hierarchy of phases and sub-phases
/// by automatically propagating metrics from sub-phases to outer phases and
/// automatically observing the metrics on drop.
///
/// It is intended to be stored in a local variable on the stack. Do not heap
/// allocate it or refer to it from other objects.
///
/// Usage:
/// 1) Define `ScopedMetrics` for each phase.
/// 2) Add `let scope = MeasurementScope::root(...)` in the top-most phase.
/// 3) Add `let scope = MeasurementScope::nested(...)` in a sub-phase.
/// 4) Tell the scopes about executed instructions using `scope.add()`.
/// See the `example_usage()` test below for details.
#[must_use = "Keep the scope in a local variable"]
#[derive(Debug)]
pub(crate) struct MeasurementScope<'a> {
    // `Rc<RefCell<...>>` is needed because we want to keep a reference to
    // the outer scope. Ordinary references do not work here because the
    // recursive definition would force the same lifetime for all references.
    core: Rc<RefCell<MeasurementScopeCore<'a>>>,
}

impl<'a> MeasurementScope<'a> {
    /// Returns a new root scope for tracking the given metrics.
    pub fn root(metrics: &'a ScopedMetrics) -> MeasurementScope<'a> {
        Self {
            core: Rc::new(RefCell::new(MeasurementScopeCore {
                metrics,
                instructions: NumInstructions::from(0),
                slices: NumSlices::from(0),
                messages: NumMessages::from(0),
                outer: None,
                start_time: Instant::now(),
                record_zeros: true,
            })),
        }
    }

    /// Returns a new scope for tracking the given metrics that is
    /// nested in the given scope. It automatically propagates
    /// the instruction counter to the outer scope.
    pub fn nested(
        metrics: &'a ScopedMetrics,
        outer: &MeasurementScope<'a>,
    ) -> MeasurementScope<'a> {
        Self {
            core: Rc::new(RefCell::new(MeasurementScopeCore {
                metrics,
                instructions: NumInstructions::from(0),
                slices: NumSlices::from(0),
                messages: NumMessages::from(0),
                outer: Some(outer.clone()),
                start_time: Instant::now(),
                record_zeros: true,
            })),
        }
    }

    /// Disable recording of scopes with zero messages, zero instructions.
    pub fn dont_record_zeros(self) -> Self {
        self.core.borrow_mut().record_zeros = false;
        self
    }

    /// Increments the instruction and message counters.
    pub fn add(&self, instructions: NumInstructions, slices: NumSlices, messages: NumMessages) {
        let mut core = self.core.borrow_mut();
        core.instructions += instructions;
        core.slices += slices;
        core.messages += messages;
    }
}

impl<'a> Clone for MeasurementScope<'a> {
    fn clone(&self) -> MeasurementScope<'a> {
        Self {
            core: Rc::clone(&self.core),
        }
    }
}

/// Returns a histogram with buckets appropriate for durations.
pub fn duration_histogram<S: Into<String>>(
    name: S,
    help: S,
    metrics_registry: &MetricsRegistry,
) -> Histogram {
    let mut buckets = decimal_buckets_with_zero(-4, 1);
    buckets.push(100.0);
    // Buckets are [0, 100µs, 200µs, 500µs, ..., 10s, 20s, 50s, 100s].
    metrics_registry.histogram(name, help, buckets)
}

/// Returns buckets appropriate for instructions.
fn instructions_buckets() -> Vec<f64> {
    let mut buckets: Vec<NumInstructions> = decimal_buckets_with_zero(4, 11)
        .into_iter()
        .map(|x| NumInstructions::from(x as u64))
        .collect();

    // Add buckets for counting no-op and small messages.
    buckets.push(NumInstructions::from(10));
    buckets.push(NumInstructions::from(1000));
    for value in (1_000_000_000..10_000_000_000).step_by(1_000_000_000) {
        buckets.push(NumInstructions::from(value));
    }

    buckets.push(NumInstructions::from(1_000_000_000_000));

    // Ensure that all buckets are unique.
    buckets.sort_unstable();
    buckets.dedup();
    // Buckets are [0, 10, 1K, 10K, 20K, ..., 100B, 200B, 500B, 1T] + [1B, 2B, 3B, ..., 9B]
    buckets.into_iter().map(|x| x.get() as f64).collect()
}

/// Returns a histogram with buckets appropriate for dts pause/abort executions.
pub fn dts_pause_or_abort_histogram<S: Into<String>>(
    name: S,
    help: S,
    metrics_registry: &MetricsRegistry,
) -> Histogram {
    let mut buckets: Vec<f64> = (0..10).map(f64::from).collect();
    buckets.extend(decimal_buckets(1, 4));
    metrics_registry.histogram(name, help, buckets)
}

/// Returns a histogram with buckets appropriate for instructions.
pub fn instructions_histogram<S: Into<String>>(
    name: S,
    help: S,
    metrics_registry: &MetricsRegistry,
) -> Histogram {
    metrics_registry.histogram(name, help, instructions_buckets())
}

/// Returns a histogram with buckets appropriate for Cycles.
pub fn cycles_histogram<S: Into<String>>(
    name: S,
    help: S,
    metrics_registry: &MetricsRegistry,
) -> Histogram {
    metrics_registry.histogram(name, help, decimal_buckets_with_zero(6, 15))
}

/// Returns buckets appropriate for WASM and Stable memories
fn memory_buckets() -> Vec<f64> {
    const K: u64 = 1024;
    const M: u64 = K * 1024;
    const G: u64 = M * 1024;
    let mut buckets: Vec<_> = [
        0,
        4 * K,
        64 * K,
        M,
        10 * M,
        50 * M,
        100 * M,
        500 * M,
        G,
        2 * G,
        3 * G,
        4 * G,
        5 * G,
        6 * G,
        7 * G,
        8 * G,
    ]
    .iter()
    .chain([MAX_STABLE_MEMORY_IN_BYTES, MAX_WASM_MEMORY_IN_BYTES].iter())
    .cloned()
    .collect();
    // Ensure that all buckets are unique
    buckets.sort_unstable();
    buckets.dedup();
    buckets.into_iter().map(|x| x as f64).collect()
}

/// Returns a histogram with buckets appropriate for Canister memory.
pub fn memory_histogram<S: Into<String>>(
    name: S,
    help: S,
    metrics_registry: &MetricsRegistry,
) -> Histogram {
    metrics_registry.histogram(name, help, memory_buckets())
}

/// Returns a histogram with buckets appropriate for messages.
pub fn messages_histogram<S: Into<String>>(
    name: S,
    help: S,
    metrics_registry: &MetricsRegistry,
) -> Histogram {
    let mut buckets = decimal_buckets_with_zero(1, 4);
    buckets.push(100_000.0);
    // Buckets are [0, 10, 20, 50, ..., 10K, 20K, 50K].
    metrics_registry.histogram(name, help, buckets)
}

/// Returns a histogram with buckets appropriate for slices.
pub fn slices_histogram<S: Into<String>>(
    name: S,
    help: S,
    metrics_registry: &MetricsRegistry,
) -> Histogram {
    // Re-use the messages histogram.
    messages_histogram(name, help, metrics_registry)
}

#[derive(Debug)]
struct MeasurementScopeCore<'a> {
    metrics: &'a ScopedMetrics,
    instructions: NumInstructions,
    slices: NumSlices,
    messages: NumMessages,
    outer: Option<MeasurementScope<'a>>,
    start_time: Instant,
    record_zeros: bool,
}

impl<'a> Drop for MeasurementScopeCore<'a> {
    fn drop(&mut self) {
        if let Some(outer) = &self.outer {
            outer.add(self.instructions, self.slices, self.messages);
        }
        if self.record_zeros || self.messages.get() != 0 {
            self.metrics
                .instructions
                .observe(self.instructions.get() as f64);
            self.metrics.slices.observe(self.slices.get() as f64);
            self.metrics.messages.observe(self.messages.get() as f64);
            self.metrics
                .duration
                .observe(self.start_time.elapsed().as_secs_f64())
        }
    }
}

#[cfg(test)]
mod tests {
    use ic_types::NumMessages;

    use super::*;

    #[test]
    fn example_usage() {
        // Setup:
        let mr = MetricsRegistry::new();
        let round_metrics = ScopedMetrics {
            duration: duration_histogram("round_duration_seconds", "...", &mr),
            instructions: instructions_histogram("round_instructions", "...", &mr),
            slices: slices_histogram("round_slices", "...", &mr),
            messages: messages_histogram("round_messages", "...", &mr),
        };
        let canister_metrics = ScopedMetrics {
            duration: duration_histogram("canister_duration_seconds", "...", &mr),
            instructions: instructions_histogram("canister_instructions", "...", &mr),
            slices: messages_histogram("canister_slices", "...", &mr),
            messages: messages_histogram("canister_messages", "...", &mr),
        };

        // Round execution:
        {
            let scope = MeasurementScope::root(&round_metrics);
            for _ in 0..10 {
                // Canister execution:
                let scope = MeasurementScope::nested(&canister_metrics, &scope);
                scope.add(
                    NumInstructions::from(10),
                    NumSlices::from(2),
                    NumMessages::from(1),
                );
            }
        }

        // Results:
        // - durations were measured automatically.
        // - instructions propagated from canisters to round automatically.
        assert_eq!(10, canister_metrics.duration.get_sample_count());
        assert_eq!(10, canister_metrics.instructions.get_sample_count());
        assert_eq!(100, canister_metrics.instructions.get_sample_sum() as u64);
        assert_eq!(10, canister_metrics.slices.get_sample_count());
        assert_eq!(20, canister_metrics.slices.get_sample_sum() as u64);
        assert_eq!(10, canister_metrics.messages.get_sample_count());
        assert_eq!(10, canister_metrics.messages.get_sample_sum() as u64);
        assert_eq!(1, round_metrics.duration.get_sample_count());
        assert_eq!(1, round_metrics.instructions.get_sample_count());
        assert_eq!(100, round_metrics.instructions.get_sample_sum() as u64);
        assert_eq!(1, round_metrics.slices.get_sample_count());
        assert_eq!(20, round_metrics.slices.get_sample_sum() as u64);
        assert_eq!(1, round_metrics.messages.get_sample_count());
        assert_eq!(10, round_metrics.messages.get_sample_sum() as u64);
    }

    #[test]
    fn multiple_nested_scopes() {
        let mr = MetricsRegistry::new();
        let l1 = ScopedMetrics {
            duration: duration_histogram("l1_duration_seconds", "...", &mr),
            instructions: instructions_histogram("l1_instructions", "...", &mr),
            slices: slices_histogram("l1_slices", "...", &mr),
            messages: messages_histogram("l1_messages", "...", &mr),
        };
        let l2 = ScopedMetrics {
            duration: duration_histogram("l2_duration_seconds", "...", &mr),
            instructions: instructions_histogram("l2_instructions", "...", &mr),
            slices: slices_histogram("l2_slices", "...", &mr),
            messages: messages_histogram("l2_messages", "...", &mr),
        };
        let l3 = ScopedMetrics {
            duration: duration_histogram("l3_duration_seconds", "...", &mr),
            instructions: instructions_histogram("l3_instructions", "...", &mr),
            slices: slices_histogram("l3_slices", "...", &mr),
            messages: messages_histogram("l3_messages", "...", &mr),
        };

        {
            let scope = MeasurementScope::root(&l1);
            {
                let scope = MeasurementScope::nested(&l2, &scope);
                {
                    let scope = MeasurementScope::nested(&l3, &scope);
                    scope.add(
                        NumInstructions::from(10),
                        NumSlices::from(2),
                        NumMessages::from(1),
                    );
                }
            }
        }
        assert_eq!(1, l1.duration.get_sample_count());
        assert_eq!(1, l1.instructions.get_sample_count());
        assert_eq!(10, l1.instructions.get_sample_sum() as u64);
        assert_eq!(1, l1.slices.get_sample_count());
        assert_eq!(2, l1.slices.get_sample_sum() as u64);
        assert_eq!(1, l1.messages.get_sample_count());
        assert_eq!(1, l1.messages.get_sample_sum() as u64);
        assert_eq!(1, l2.duration.get_sample_count());
        assert_eq!(1, l2.instructions.get_sample_count());
        assert_eq!(10, l2.instructions.get_sample_sum() as u64);
        assert_eq!(1, l2.slices.get_sample_count());
        assert_eq!(2, l2.slices.get_sample_sum() as u64);
        assert_eq!(1, l2.messages.get_sample_count());
        assert_eq!(1, l2.messages.get_sample_sum() as u64);
        assert_eq!(1, l3.duration.get_sample_count());
        assert_eq!(1, l3.instructions.get_sample_count());
        assert_eq!(10, l3.instructions.get_sample_sum() as u64);
        assert_eq!(1, l3.slices.get_sample_count());
        assert_eq!(2, l3.slices.get_sample_sum() as u64);
        assert_eq!(1, l3.messages.get_sample_count());
        assert_eq!(1, l3.messages.get_sample_sum() as u64);
    }

    #[test]
    fn multiple_add_calls() {
        let mr = MetricsRegistry::new();
        let l1 = ScopedMetrics {
            duration: duration_histogram("l1_duration_seconds", "...", &mr),
            instructions: instructions_histogram("l1_instructions", "...", &mr),
            slices: slices_histogram("l1_slices", "...", &mr),
            messages: messages_histogram("l1_messages", "...", &mr),
        };
        let l2 = ScopedMetrics {
            duration: duration_histogram("l2_duration_seconds", "...", &mr),
            instructions: instructions_histogram("l2_instructions", "...", &mr),
            slices: slices_histogram("l2_slices", "...", &mr),
            messages: messages_histogram("l2_messages", "...", &mr),
        };

        {
            let scope = MeasurementScope::root(&l1);
            scope.add(
                NumInstructions::from(100),
                NumSlices::from(2),
                NumMessages::from(1),
            );
            {
                let scope = MeasurementScope::nested(&l2, &scope);
                scope.add(
                    NumInstructions::from(10),
                    NumSlices::from(2),
                    NumMessages::from(1),
                );
                scope.add(
                    NumInstructions::from(20),
                    NumSlices::from(2),
                    NumMessages::from(1),
                );
                scope.add(
                    NumInstructions::from(30),
                    NumSlices::from(2),
                    NumMessages::from(1),
                );
            }
            scope.add(
                NumInstructions::from(200),
                NumSlices::from(2),
                NumMessages::from(1),
            );
        }
        assert_eq!(1, l1.duration.get_sample_count());
        assert_eq!(1, l1.instructions.get_sample_count());
        assert_eq!(360, l1.instructions.get_sample_sum() as u64);
        assert_eq!(1, l1.slices.get_sample_count());
        assert_eq!(10, l1.slices.get_sample_sum() as u64);
        assert_eq!(1, l1.messages.get_sample_count());
        assert_eq!(5, l1.messages.get_sample_sum() as u64);
        assert_eq!(1, l2.duration.get_sample_count());
        assert_eq!(1, l2.instructions.get_sample_count());
        assert_eq!(60, l2.instructions.get_sample_sum() as u64);
        assert_eq!(1, l2.slices.get_sample_count());
        assert_eq!(6, l2.slices.get_sample_sum() as u64);
        assert_eq!(1, l2.messages.get_sample_count());
        assert_eq!(3, l2.messages.get_sample_sum() as u64);
    }

    #[test]
    fn dont_record_zeros() {
        let mr = MetricsRegistry::new();
        let outer = ScopedMetrics {
            duration: duration_histogram("l1_duration_seconds", "...", &mr),
            instructions: instructions_histogram("l1_instructions", "...", &mr),
            slices: slices_histogram("l1_slices", "...", &mr),
            messages: messages_histogram("l1_messages", "...", &mr),
        };
        let middle = ScopedMetrics {
            duration: duration_histogram("l2_duration_seconds", "...", &mr),
            instructions: instructions_histogram("l2_instructions", "...", &mr),
            slices: slices_histogram("l2_slices", "...", &mr),
            messages: messages_histogram("l2_messages", "...", &mr),
        };
        let inner = ScopedMetrics {
            duration: duration_histogram("l3_duration_seconds", "...", &mr),
            instructions: instructions_histogram("l3_instructions", "...", &mr),
            slices: slices_histogram("l3_slices", "...", &mr),
            messages: messages_histogram("l3_messages", "...", &mr),
        };

        {
            let outer_scope = MeasurementScope::root(&outer);
            let middle_scope = MeasurementScope::nested(&middle, &outer_scope).dont_record_zeros();
            let _inner_scope = MeasurementScope::nested(&inner, &middle_scope);
        }

        // Outer scope should have recorded one zero sample for each metric.
        assert_eq!(1, outer.duration.get_sample_count());
        assert_eq!(1, outer.instructions.get_sample_count());
        assert_eq!(0, outer.instructions.get_sample_sum() as u64);
        assert_eq!(1, outer.slices.get_sample_count());
        assert_eq!(0, outer.slices.get_sample_sum() as u64);
        assert_eq!(1, outer.messages.get_sample_count());
        assert_eq!(0, outer.messages.get_sample_sum() as u64);

        // Middle scope (with `dont_record_zeros()`) should not have recorded any
        // samples.
        assert_eq!(0, middle.duration.get_sample_count());
        assert_eq!(0, middle.instructions.get_sample_count());
        assert_eq!(0, middle.instructions.get_sample_sum() as u64);
        assert_eq!(0, middle.slices.get_sample_count());
        assert_eq!(0, middle.slices.get_sample_sum() as u64);
        assert_eq!(0, middle.messages.get_sample_count());
        assert_eq!(0, middle.messages.get_sample_sum() as u64);

        // Inner scope should have also recorded one zero sample for each metric.
        assert_eq!(1, inner.duration.get_sample_count());
        assert_eq!(1, inner.instructions.get_sample_count());
        assert_eq!(0, inner.instructions.get_sample_sum() as u64);
        assert_eq!(1, inner.slices.get_sample_count());
        assert_eq!(0, inner.slices.get_sample_sum() as u64);
        assert_eq!(1, inner.messages.get_sample_count());
        assert_eq!(0, inner.messages.get_sample_sum() as u64);
    }

    #[test]
    fn valid_instructions_buckets() {
        let buckets: std::collections::HashSet<_> = instructions_buckets()
            .into_iter()
            .map(|x| x as u64)
            .collect();
        assert!(!buckets.is_empty());
        let limits = [10, 1_000, 1_000_000_000, 200_000_000_000, 500_000_000_000];
        for l in limits {
            assert!(buckets.contains(&l));
        }
    }

    #[test]
    fn valid_memory_buckets() {
        let buckets: std::collections::HashSet<_> =
            memory_buckets().into_iter().map(|x| x as u64).collect();
        assert!(buckets.contains(&0));
        assert!(buckets.contains(&MAX_STABLE_MEMORY_IN_BYTES));
        assert!(buckets.contains(&MAX_WASM_MEMORY_IN_BYTES));
    }
}
