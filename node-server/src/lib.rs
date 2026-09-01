//! pneumatic_node_server — the composite node-server runtime.
//!
//! A single process that is the runtime; Committer, Sentinel, Executor, and
//! Finalizer are role-plugins it hosts. RNS is the external wire between nodes.
//! The headline new behavior is role-selection-by-stake: an in-process
//! `RoleSelector` decides which role-plugins actually install from the node's
//! own stake against the protocol + per-type floors.
//!
//! The in-process dispatch backbone (`RoleDispatcher`) is a fresh layer —
//! deliberately not the dead `pneumatic_core` `ThreadPool` and not RNS.
//!
//! Plan: create-an-implementation-plan-shimmering-gosling.md

pub mod boot;
pub mod role_selector;
pub mod role_dispatcher;
pub mod node_server;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    /// Walk every implementation `.rs` file under this crate's `src/`
    /// (recursively, including `src/bin/`) — excluding the harness `lib.rs`
    /// (which is where this gate test itself lives) — and collect any
    /// `use`-line or `::server::ThreadPool` reference to the dead
    /// `pneumatic_core::server::ThreadPool`.
    fn find_threadpool_refs(dir: &Path) -> Vec<(String, usize, String)> {
        let mut hits = Vec::new();
        let Ok(rd) = fs::read_dir(dir) else { return hits; };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                hits.extend(find_threadpool_refs(&p));
                continue;
            }
            // Skip the harness file: it intentionally mentions `::server::ThreadPool`
            // in its own doc comments and check-strings, which would trip itself.
            if p.file_name().and_then(|n| n.to_str()) == Some("lib.rs") {
                continue;
            }
            if p.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let Ok(content) = fs::read_to_string(&p) else { continue };
            for (i, line) in content.lines().enumerate() {
                let trimmed = line.trim();
                // Skip comment lines: a doc/reference to the dead pool's path in
                // prose (explaining *why* it is avoided) is not a dependency edge.
                // Real code — a `use` line or a `::server::ThreadPool` reference —
                // never begins with `//`, so this does not weaken the gate.
                if trimmed.starts_with("//") {
                    continue;
                }
                let is_pool_use = trimmed.starts_with("use") && trimmed.contains("ThreadPool");
                let is_server_ref = trimmed.contains("::server::ThreadPool")
                    || trimmed.contains("pneumatic_core::server::ThreadPool");
                if is_pool_use || is_server_ref {
                    hits.push((
                        p.to_string_lossy().into_owned(),
                        i + 1,
                        trimmed.to_string(),
                    ));
                }
            }
        }
        hits
    }

    /// Gate: `node-server` must not depend on `pneumatic_core`'s dead `ThreadPool`
    /// (`src/server.rs` — zero external call sites). Asserts no source in this
    /// crate references it. The assertion message and this comment are the only
    /// occurrences of the word, and they are neither `use`-lines nor
    /// `::server::ThreadPool`, so they don't trip the gate.
    #[test]
    fn no_threadpool_dependency() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let hits = find_threadpool_refs(&src);
        assert!(
            hits.is_empty(),
            "node-server references dead ThreadPool (expected none): {hits:?}"
        );
    }
}
