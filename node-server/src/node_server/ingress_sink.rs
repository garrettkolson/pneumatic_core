//! Bridge from the client-facing HTTP ingress (core [`pneumatic_core::ingress`])
//! to this composite's role pipeline (rollout roadmap Phase 1, ADR-020).
//!
//! The ingress module authenticates at the edge and then hands the inner
//! `Process` envelope to a [`SubmitSink`]. This module supplies the sink a
//! composite node runs: it wraps the inner envelope in the node-identity-signed
//! outer `"Verify"` envelope — byte-for-byte the shape a submitting peer
//! relays on the RNS wire — and calls [`NodeServer::dispatch`], i.e. the very
//! in-process path the RNS bridge's `route_data_plane` uses. From the
//! dispatcher down, an HTTP submission is indistinguishable from a peer
//! submission; there is exactly one pipeline entry.
//!
//! Failure mapping is deliberate and honest to the client:
//!
//! * `UnknownAction` means **no Sentinel plugin is installed** on this node —
//!   the submission has no owner. The client gets `503`, never a silent
//!   accept: the node would otherwise answer 200 to work it cannot do.
//! * `AmbiguousAction` is a wiring bug on this host — also `503` (this host
//!   cannot be trusted to route the submission).
//! * Sentinel refusals arrive as `Downstream`. A duplicate id surfaces as
//!   `409` (match on the debug-fidelity string — `RoleError::Downstream`
//!   stringifies the underlying `SentinelError`, and `TransactionAlreadyExists`
//!   survives that round trip; see the unit test pinning it). Every other
//!   downstream refusal is `422`: the pipeline saw it and said no.

use std::sync::Arc;

use pneumatic_core::encoding::serialize_to_bytes_rmp;
use pneumatic_core::errors::PneumaticError;
use pneumatic_core::ingress::{IngressError, SubmitFuture, SubmitSink};
use pneumatic_core::messages::Message;

use crate::role_dispatcher::RoleError;
use super::{NodeServer, SENTINEL_ACTIONS};

impl NodeServer {
    /// Build the [`SubmitSink`] for this host's client ingress.
    ///
    /// The sink re-stamps nothing on the transaction: it forwards the exact
    /// inner envelope the ingress authenticated, inside a fresh outer
    /// `"Verify"` envelope signed by this node's identity (what a relaying
    /// peer's gossiper produces). Bind failures and draining are the HTTP
    /// server's concern; this closure only routes and maps.
    pub fn ingress_sink(self: &Arc<Self>) -> SubmitSink {
        let env_id = self.config.main_environment_id.clone();
        let identity = self.config.identity.clone();
        let server = self.clone();
        Arc::new(move |inner: Message| {
            let server = server.clone();
            let env_id = env_id.clone();
            let identity = identity.clone();
            Box::pin(async move {
                if inner.chain_id != env_id {
                    return Err(IngressError::Rejected(format!(
                        "chain id {:?} is not this node's environment {:?}",
                        inner.chain_id, env_id
                    )));
                }
                let body = serialize_to_bytes_rmp(&inner)
                    .map_err(|e| IngressError::Rejected(format!("re-encoding the submission failed: {e}")))?;
                let outer = Message::signed(env_id, SENTINEL_ACTIONS[0], body, None, &identity)
                    .map_err(|e| IngressError::Rejected(format!("signing the relay envelope failed: {e}")))?;
                server.dispatch(outer).await.map_err(map_role_error)
            }) as SubmitFuture
        })
    }
}

/// Map a dispatcher refusal onto the client-facing status taxonomy.
fn map_role_error(e: RoleError) -> IngressError {
    match e {
        RoleError::UnknownAction(action) => IngressError::Unavailable(format!(
            "no installed role owns action {action:?} — transaction ingress requires the Sentinel role"
        )),
        RoleError::AmbiguousAction { action, roles } => IngressError::Unavailable(format!(
            "action {action:?} is claimed by multiple installed roles ({roles:?}) — refusing to guess"
        )),
        // The sentinel adapter stringifies SentinelError through `{:?}` when
        // wrapping it (role_adapters.rs), so `TransactionAlreadyExists(..)`
        // is the stable marker for "this id/nonce was already accepted".
        RoleError::Downstream(PneumaticError::Network(ref s)) if s.contains("TransactionAlreadyExists") => {
            IngressError::Duplicate(s.clone())
        }
        RoleError::Downstream(other) => IngressError::Rejected(format!("{other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_action_maps_to_unavailable_not_a_silent_accept() {
        let e = map_role_error(RoleError::UnknownAction("Process".into()));
        assert!(matches!(e, IngressError::Unavailable(_)), "{e:?}");
        assert_eq!(e.status(), 503);
    }

    #[test]
    fn the_duplicate_marker_survives_the_downstream_string_round_trip() {
        // Pin the exact fragility the mapping depends on: the sentinel
        // adapter wraps SentinelError via `format!("{e:?}")`. If that
        // wrapping ever stops naming the variant, this test fails BEFORE the
        // client silently starts seeing 422 for duplicates.
        let wrapped = RoleError::Downstream(PneumaticError::Network(format!(
            "{:?}",
            pneumatic_sentinel::sentinel_error::SentinelError::TransactionAlreadyExists("tx-1".to_string())
        )));
        let mapped = map_role_error(wrapped);
        assert!(matches!(mapped, IngressError::Duplicate(_)), "{mapped:?}");
    }

    #[test]
    fn any_other_downstream_refusal_is_rejected_422() {
        let e = map_role_error(RoleError::Downstream(PneumaticError::Network(
            "RiskThresholdExceeded".into(),
        )));
        assert!(matches!(e, IngressError::Rejected(_)), "{e:?}");
        assert_eq!(e.status(), 422);
    }
}
