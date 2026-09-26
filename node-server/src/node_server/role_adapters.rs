//! Role adapters for the node-server: the `RoleHandler`/`RoleHost` impls
//! that forward each installed role's inbound bus actions to its real
//! handler and drive its lifecycle (epoch advance, shutdown).

use super::*;

// ---------------------------------------------------------------------------
// RoleHandler impls — each installed role forwards its inbound bus actions to
// its real handler. Committer/Executor/Sentinel delegate to their real inbound
// method; the Finalizer routes `Sign` to `handle_signature` (audit C1) and
// fails closed on any other inbound action.
// ---------------------------------------------------------------------------

impl RoleHandler for pneumatic_committer::Committer {
    fn role(&self) -> pneumatic_core::node::NodeRegistryType {
        pneumatic_core::node::NodeRegistryType::Committer
    }
    fn allowed_actions(&self) -> &'static [&'static str] {
        COMMITTER_ACTIONS
    }
    fn handle<'a>(
        &'a self,
        message: Message,
    ) -> std::pin::Pin<
        std::boxed::Box<dyn std::future::Future<Output = Result<(), RoleError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.handle_message(message)
                .await
                .map_err(|e| RoleError::Downstream(PneumaticError::Network(format!("{e:?}"))))
        })
    }
}

impl RoleHandler for pneumatic_executor::Executor {
    fn role(&self) -> pneumatic_core::node::NodeRegistryType {
        pneumatic_core::node::NodeRegistryType::Executor
    }
    fn allowed_actions(&self) -> &'static [&'static str] {
        EXECUTOR_ACTIONS
    }
    fn handle<'a>(
        &'a self,
        message: Message,
    ) -> std::pin::Pin<
        std::boxed::Box<dyn std::future::Future<Output = Result<(), RoleError>> + Send + 'a>,
    > {
        Box::pin(async move {
            // The sentinel's Preload body is the rmp-serialized `Transaction`,
            // not raw tx_id bytes (sentinel/src/transaction_notifier.rs:32).
            // `ingest_preload` decodes it, registers the tx in this executor's
            // own pending registry (execution reads from there), and starts
            // the run.
            self.ingest_preload(&message.body)
                .await
                .map_err(|e| RoleError::Downstream(PneumaticError::Network(format!("{e:?}"))))
        })
    }
}

impl RoleHandler for pneumatic_sentinel::Sentinel {
    fn role(&self) -> pneumatic_core::node::NodeRegistryType {
        pneumatic_core::node::NodeRegistryType::Sentinel
    }
    fn allowed_actions(&self) -> &'static [&'static str] {
        SENTINEL_ACTIONS
    }
    fn handle<'a>(
        &'a self,
        message: Message,
    ) -> std::pin::Pin<
        std::boxed::Box<dyn std::future::Future<Output = Result<(), RoleError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.on_data_received(message.body)
                .map_err(|e| RoleError::Downstream(PneumaticError::Network(format!("{e:?}"))))
        })
    }
}

impl RoleHandler for pneumatic_finalizer::Finalizer {
    fn role(&self) -> pneumatic_core::node::NodeRegistryType {
        pneumatic_core::node::NodeRegistryType::Finalizer
    }
    fn allowed_actions(&self) -> &'static [&'static str] {
        FINALIZER_ACTIONS
    }
    fn handle<'a>(
        &'a self,
        message: Message,
    ) -> std::pin::Pin<
        std::boxed::Box<dyn std::future::Future<Output = Result<(), RoleError>> + Send + 'a>,
    > {
        Box::pin(async move {
            match message.action.as_str() {
                // The voter inbound path: authenticate the executor's identity,
                // verify + accumulate its signature, or optimistic-finalize on
                // the first valid one. The real chokepoint for every voter
                // signature (audit C1).
                "Sign" => self
                    .handle_signature(&message)
                    .await
                    .map(|_| ())
                    .map_err(|e| RoleError::Downstream(PneumaticError::Network(format!("{e:?}")))),
                // Shielded-transfer vote request from the Sentinel (S5.2).
                // Body is a `ShieldedTransaction`; the finalizer re-validates
                // against its own pool view, signs the stx hash, and fans the
                // vote out to the other finalizers.
                "SignShielded" => self
                    .handle_sign_shielded(&message)
                    .await
                    .map(|_| ())
                    .map_err(|e| RoleError::Downstream(PneumaticError::Network(format!("{e:?}")))),
                // Shielded-transfer vote from a voting finalizer to the
                // collector Finalizer (S5.2). Body is a `TransactionSignature`;
                // quorum reached → assemble the shielded block and commit.
                "ShieldedVote" => self
                    .handle_shielded_vote(&message)
                    .await
                    .map(|_| ())
                    .map_err(|e| RoleError::Downstream(PneumaticError::Network(format!("{e:?}")))),
                // Any other action this role owns is not a handled inbound
                // action; fail closed with a protocol-level error rather than
                // silently accepting it.
                other => Err(RoleError::Downstream(PneumaticError::Network(format!(
                    "finalizer: unhandled inbound action {other:?}"
                )))),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// RoleHost impls — Phase 5 lifecycle. `role()`/`allowed_actions()`/`handle`
// come from each role's `impl RoleHandler` above; these impls supply only the
// two lifecycle methods. Inherent calls use fully-qualified `Self::` so the
// compiler resolves them to the *inherent* method (which has the same name)
// rather than recursing into the trait method.
// ---------------------------------------------------------------------------

impl RoleHost for pneumatic_committer::Committer {
    // The Committer self-drives its epoch via its own `run_epoch_loop` (the
    // single writer of the epoch number), so the coordinator must not advance
    // it — the fan-out visits the Committer, whose `advance_epoch` is a no-op.
    fn advance_epoch(&mut self, _epoch: u64) {}

    fn initiate_shutdown<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        std::boxed::Box<dyn std::future::Future<Output = ()> + Send + 'a>,
    > {
        // Inherent `Committer::initiate_shutdown(&self)`, reached via
        // fully-qualified `Self::` so it does not recurse into this trait method.
        Box::pin(async move { Self::initiate_shutdown(self).await; })
    }
}

impl RoleHost for pneumatic_executor::Executor {
    // The Executor has no epoch advance and no shutdown lifecycle — both no-ops.
    fn advance_epoch(&mut self, _epoch: u64) {}

    fn initiate_shutdown<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        std::boxed::Box<dyn std::future::Future<Output = ()> + Send + 'a>,
    > {
        Box::pin(async move {})
    }
}

impl RoleHost for pneumatic_sentinel::Sentinel {
    // Sentinel advances from the external epoch signal: guards monotonicity and
    // invalidates its per-epoch caches.
    fn advance_epoch(&mut self, epoch: u64) {
        Self::advance_epoch(self, epoch);
    }

    // Sentinel has no shutdown lifecycle — a graceful no-op.
    fn initiate_shutdown<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        std::boxed::Box<dyn std::future::Future<Output = ()> + Send + 'a>,
    > {
        Box::pin(async move {})
    }
}

impl RoleHost for pneumatic_finalizer::Finalizer {
    // The Finalizer bumps its internal epoch counter + invalidates its stake
    // cache from the external signal. `advance_epoch(&mut self)`: the boxed
    // host gives this the mutable handle it needs (no Mutex).
    fn advance_epoch(&mut self, _epoch: u64) {
        Self::advance_epoch(self);
    }

    fn initiate_shutdown<'a>(
        &'a mut self,
    ) -> std::pin::Pin<
        std::boxed::Box<dyn std::future::Future<Output = ()> + Send + 'a>,
    > {
        Box::pin(async move { Self::initiate_shutdown(self).await; })
    }
}
