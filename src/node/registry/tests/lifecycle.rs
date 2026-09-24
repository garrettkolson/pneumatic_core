//! Evictor lifecycle tests: the evictor exits on drop and on
//! `stop_eviction`, and removes expired nodes past the cutoff.
use super::helpers::*;
use super::super::*;

// --- Phase 6.7: stop the evictor on shutdown (Drop + shutdown flag) ---
//
// `registry_with_capacity` builds a registry with `network: None`, so `init`
// does not spawn the evictor; each test starts it explicitly with the
// private `start_eviction` (accessible from this nested module) after
// overriding the poll interval to ~20 ms via `with_evict_interval`. Each
// test fails *fast* (panics, not hangs) when the fix is reverted: without
// the shutdown flag the loop never exits, so `is_finished()` stays false
// and the final assert in each test fails fast (panics, not hangs).

/// Headline regression: the eviction thread is running before shutdown and
/// has exited once the registry is dropped — proving the new [`Drop`] impl
/// actually stops it and releases the captured registry `Arc`s.
#[test]
fn evictor_exits_on_drop() {
    let mut reg = registry_with_capacity(&[(NodeRegistryType::Committer, 5)]);
    reg.with_evict_interval(Duration::from_millis(20));
    reg.start_eviction();

    let handle = reg.evictor_handle().expect("evictor thread started");
    assert!(!handle.is_finished(), "evictor must be running before shutdown");

    drop(reg); // Drop -> stop_eviction sets the flag -> thread exits within one poll

    // Wait (bounded) for the thread to observe the flag and exit. On revert
    // (no flag) it never exits, so this exhausts the deadline and fails fast.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(handle.is_finished(), "Drop must stop the eviction thread");
}

/// The explicit [`stop_eviction`] path sets the shutdown flag and returns
/// within one poll. It is gated on a receive timeout (so the test never
/// blocks), and the thread's exit is asserted via the cloned handle's
/// `is_finished()`; if the flag is missing the loop never exits and that
/// final assert fails fast.
#[test]
fn evictor_exits_on_stop_eviction() {
    let mut reg = registry_with_capacity(&[(NodeRegistryType::Committer, 5)]);
    reg.with_evict_interval(Duration::from_millis(20));
    reg.start_eviction();

    let running = reg.evictor_handle().expect("evictor thread started");
    assert!(!running.is_finished(), "evictor must be running before shutdown");

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let stopper = std::thread::spawn(move || {
        reg.stop_eviction();
        tx.send(()).ok();
    });
    rx.recv_timeout(Duration::from_secs(2))
        .expect("stop_eviction returned -> evictor exited");

    stopper.join().expect("stop thread");

    // Wait (bounded) for the thread to observe the flag and exit. On revert
    // it never exits, so this exhausts the deadline and fails fast.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !running.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(running.is_finished(), "stop_eviction must stop the eviction thread");
}

/// Positive control: the running loop actually evicts an expired node
/// (backdated past the 30 s cutoff) rather than being a no-op. This is not a
/// discriminator of the shutdown fix — the eviction logic is unchanged — but
/// confirms the thread is doing real work before it is stopped.
#[test]
fn evictor_removes_expired_nodes() {
    let mut reg = registry_with_capacity(&[(NodeRegistryType::Committer, 5)]);
    let identity = NodeIdentity::generate_in_memory();
    register_node(&reg, &identity, NodeRegistryType::Committer);
    let key = identity.ed25519.public_key().unwrap();

    // Backdate liveness well past the 30 s cutoff in `evict_expired`.
    {
        let mut nodes = reg.get_nodes(&NodeRegistryType::Committer).unwrap();
        let mut stored = nodes.get_mut(&key).expect("committed");
        stored.value_mut().last_seen = Instant::now() - Duration::from_secs(40);
    }

    reg.with_evict_interval(Duration::from_millis(20));
    reg.start_eviction();

    // Wait (bounded) for the loop to observe and remove the expired entry.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut evicted = false;
    while Instant::now() < deadline {
        let nodes = reg.get_nodes(&NodeRegistryType::Committer).unwrap();
        if !nodes.contains_key(&key) {
            evicted = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(evicted, "the eviction loop should remove nodes past the cutoff");

    reg.stop_eviction();
}
