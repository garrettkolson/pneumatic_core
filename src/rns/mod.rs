//! RNS (Reticulum) transport layer: identity keystore, node config
//! construction, the network wrapper, and the `Connection` impl.
//!
//! rns-net is pinned exactly (see Cargo.toml) — `NodeConfig` has ~45 fields
//! and no `Default`, so any version bump is an API-migration event that is
//! contained to `config_builder`.

pub mod conn;
pub mod config_builder;
pub mod identity;
pub mod wrapper;
