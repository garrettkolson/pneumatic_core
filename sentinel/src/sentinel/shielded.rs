//! Shielded transfer handling for the Sentinel (Phase S5.1 real path).
//!
//! These are `impl Sentinel` methods, so they retain access to the struct's
//! private fields (child modules of `crate::sentinel`).

use super::*;

impl Sentinel {
    /// Handle a `"ShieldedTransfer"` inner action from the Sentinel's client
    /// (Phase S5.1 — the real path; the S3.2 stub is retired).
    ///
    /// Five fail-closed steps, in this order (*Decision 5*):
    ///
    /// 1. Canonical-rmp deserialize the `ShieldedTransaction` body — a
    ///    malformed body is an `Encoding` error before any other work.
    /// 2. **Advisory** shielded validation: look up the spec by
    ///    `ShieldedValidationSpec::NAME` (a deployment that never
    ///    `register_shielded` gets `Validation([UnsupportedAction])` — no
    ///    fallback, *Decision 3*), then run `validate_shielded` against
    ///    `ShieldedValidationDeps` backed by the shared pool view: its
    ///    nullifier set + root history + the env's
    ///    `shielded_root_recency`. The advisory gate rejects obvious
    ///    garbage before any finalizer work; it is **not** the consensus
    ///    gate — the committer re-runs the same spec authoritatively at
    ///    block-commit time (S5.3) and fires the reserved reasons
    ///    (`UnknownNullifier`, `ValueBalanceMismatch`) only there.
    /// 3. Token gates: `token_id` must resolve in this partition
    ///    (`TokenNotFound`), be self-verified (`NotSelfVerified`), and be
    ///    shielded opt-in (`NotShieldedOptIn`).
    /// 4. Atomic `register_shielded` in the parallel, never-evicted
    ///    `shielded_transactions` map. Duplicate id ⇒
    ///    `TransactionAlreadyExists`; because registration precedes the
    ///    send, a duplicate never produces a second outbound message.
    /// 5. `assign_finalizer_deterministic` (the same snapshot/domain/salt
    ///    machinery as standard txs) + `send_sign_shielded` — the
    ///    `"SignShielded"` body is the tx's canonical bytes, the S3.1
    ///    binding surface the finalizer signs in S5.2.
    ///
    /// Shielded transfers **bypass the Executor entirely** (roadmap 2.1 —
    /// there is nothing to execute: value moves in-circuit, and the token's
    /// blockchain is never written): no preload, no executor dispatch.
    pub(crate) fn handle_shielded_transfer(&self, message: Message) -> Result<(), SentinelError> {
        // 1. Canonical deserialize.
        let stx: ShieldedTransaction =
            deserialize_rmp_to(&message.body).map_err(SentinelError::Encoding)?;

        // 2. Advisory shielded validation against the shared pool view.
        //    Any failure short-circuits before any state is written.
        let spec = self
            .env_data
            .transaction_validation_specs
            .get(ShieldedValidationSpec::NAME)
            .ok_or_else(|| {
                SentinelError::Validation(PneumaticError::Validation(
                    vec![ValidationFailureReason::UnsupportedAction],
                ))
            })?;
        let deps = ShieldedValidationDeps {
            spent: self.pool_view.nullifier_set(),
            roots: self.pool_view.root_history(),
            recency_window: self.env_data.shielded_root_recency,
        };
        spec
            .validate_shielded(&stx, &self.env_data, &deps)
            .map_err(SentinelError::Validation)?;

        // 3. Token gates: resolvable, self-verified, shielded opt-in.
        let token = self
            .data_provider
            .get_token(&stx.token_id, &self.env_data.token_partition_id)
            .map_err(|_| {
                SentinelError::Validation(PneumaticError::Validation(
                    vec![ValidationFailureReason::TokenNotFound],
                ))
            })?;
        if !token.is_self_verified {
            return Err(SentinelError::Validation(PneumaticError::Validation(
                vec![ValidationFailureReason::NotSelfVerified],
            )));
        }
        if !token.is_shielded_opt_in() {
            return Err(SentinelError::Validation(PneumaticError::Validation(
                vec![ValidationFailureReason::NotShieldedOptIn],
            )));
        }

        // 4. Atomic registration — a duplicate id is rejected and, since
        //    registration precedes the send, produces no second message.
        self.registry
            .register_shielded(&stx)
            .map_err(|_| SentinelError::TransactionAlreadyExists(stx.id.clone()))?;

        // 5. Deterministic finalizer assignment + the one outbound
        //    `SignShielded`.
        let finalizer_key =
            self.assign_finalizer_deterministic(&stx.id, *self.current_epoch.lock())?;
        self.transaction_notifier
            .send_sign_shielded(&stx, &finalizer_key, &self.env_data)?;

        Ok(())
    }

}
