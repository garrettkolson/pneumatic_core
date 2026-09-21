---
id: note-threadpool-hybrid-pool
title: "ThreadPool: hybrid sync+async worker pool"
type: note
namespace: pneumatic
visibility: namespace
summary: "ThreadPool (server.rs:10) pairs each worker with one std thread (sync closures) and one tokio task (async futures) draining two mpsc channels; new() asserts size > 0."
auto_inject: false
applicable_when: "Touching worker-pool concurrency in node crates, or spawning sync/async work from node code"
confidence: 1.0
verified_at: "09/20/2026"
verified_by: "dsh-agent"
staleness_signal: "If Worker structure, channel types, or the size-0 panic/build behavior changes"
tags: [concurrency, thread-pool, tokio, workers]
edges:
  - target: event-node-server-composite
    type: related_to
    weight: 0.7
    note: "Composite node-server workloads are dispatched through this pool"
  - target: fact-workspace-layout
    type: related_to
    weight: 0.5
    note: "server.rs is a root-crate module shared by the worker-node crates"
related: []
source_url: "Empty"
---

# ThreadPool: hybrid sync+async worker pool

`ThreadPool` (`src/server.rs:10`) is architecturally notable for being **hybrid**: every `Worker` (lines 98-102) owns *two* lanes — a std thread running a blocking recv-loop over `Job` closures (lines 118-131) and a tokio task draining `AsyncJob` futures (lines 133-148). A pool of N threads therefore provides N sync and N async workers simultaneously. The async lane is a plain std `mpsc` channel wrapped in a `tokio::sync::Mutex` (lines 45-46), so `execute_async` is callable without an ambient runtime context.

API shape: `new(size)` asserts `size > 0` and panics on zero (lines 23-26; the zero-thread case is pinned by an active `#[should_panic]` test at line 170), while `build(size)` returns `PoolCreationError` for zero (lines 30-37) — the two constructors encode the same invariant with different failure policies. Jobs are `Box<dyn FnOnce() + Send>` and `Pin<Box<dyn Future<Output = ()> + Send + Unpin>>` (lines 148-149). `Drop` closes the sender and joins each sync worker (lines 79-96).

This is the shared concurrency substrate the four worker-node crates (sentinel/executor/finalizer/committer) and the composite node-server build on, which is why a single pool object carries both blocking and tokio work.
