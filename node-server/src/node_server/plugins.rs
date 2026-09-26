//! Role-plugin construction for the node-server: `build_role_plugin`
//! builds the boxed `RoleHost` for one selected role — the single
//! construction site of the S5.4 shared shielded pool.

use super::*;

/// Build the plugin for one selected role, or `None` for roles this host does
/// not install (`Archiver`, which has no plugin). The returned value is a boxed
/// `RoleHost` (a `RoleHandler` for inbound routing *and* a `RoleHost` for the
/// Phase-5 epoch-fan-out + shutdown) so the `RoleDispatcher` can route and
/// drive lifecycle through a single handle.
pub(crate) fn build_role_plugin(
    role: pneumatic_core::node::NodeRegistryType,
    config: Arc<Config>,
    env_data: Arc<EnvironmentMetadata>,
    data_provider: Arc<dyn DataProvider>,
    node_registry: Arc<NodeRegistry>,
    hash_provider: Arc<dyn HashProvider>,
    stake_store: Arc<StakeStore>,
    staking_manager: Arc<StakingManager>,
    epoch_reconciler: Arc<EpochReconciler>,
    leader_selector: Arc<LeaderSelector>,
    tokens: Arc<DashMap<Vec<u8>, pneumatic_core::tokens::Token>>,
    pending_registry: Arc<PendingTransactionRegistry>,
    epoch_boundary_detector: Arc<EpochBoundaryDetector>,
    block_proposer: Arc<BlockProposer>,
    block_services: Arc<BlockServices>,
    // S5.3: the global shielded pool (the committer arm's commit path).
    shielded_pool: Arc<pneumatic_committer::shielded_pool::ShieldedPool>,
) -> Option<Box<dyn RoleHost>> {
    use pneumatic_core::node::NodeRegistryType;
    // S5.4 (Decision 2, option a): the sentinel/finalizer arms consume the
    // SAME pool as their `ShieldedPoolView` — the role views read the
    // shared registry live; no separate view object is composed here (the
    // one that used to be built in `build_runtime` is gone).
    let shielded_pool_view: Arc<dyn pneumatic_core::shielded::ShieldedPoolView> =
        shielded_pool.clone() as Arc<dyn pneumatic_core::shielded::ShieldedPoolView>;
    match role {
        NodeRegistryType::Committer => {
            let gossiper = Arc::new(Gossiper::new(
                NodeRegistryType::Committer,
                config.as_ref().clone(),
                60,
                env_data.asym_crypto_provider.clone(),
            ));
            let committer = pneumatic_committer::Committer::new(
                env_data.clone(),
                config.public_key.clone(),
                config.identity.clone(),
                gossiper,
                block_services,
                node_registry,
                tokens,
                pending_registry,
                stake_store,
                staking_manager,
                epoch_reconciler,
                leader_selector,
                data_provider,
                0,
                Some((*epoch_boundary_detector).clone()),
                block_proposer,
                300,
                5000,
                Arc::new(CandidateRegistry::new()),
                shielded_pool,
            );
            Some(Box::new(committer))
        }
        NodeRegistryType::Executor => {
            let executor = pneumatic_executor::Executor::new(
                env_data.environment_id.clone(),
                config.public_key.clone(),
                config.identity.clone(),
                node_registry,
                data_provider,
                pending_registry,
                hash_provider,
                100,
            );
            Some(Box::new(executor))
        }
        NodeRegistryType::Sentinel => {
            let gossiper = Arc::new(Gossiper::new(
                NodeRegistryType::Sentinel,
                config.as_ref().clone(),
                60,
                env_data.asym_crypto_provider.clone(),
            ));
            let sentinel = pneumatic_sentinel::Sentinel::new(
                config.as_ref().clone(),
                env_data.clone(),
                node_registry,
                pending_registry,
                gossiper,
                data_provider,
                shielded_pool_view,
            );
            Some(Box::new(sentinel))
        }
        NodeRegistryType::Finalizer => {
            // Phase-3 construction: the finalizer's inbound is wired in Phase 4
            // (`initialize` is a stub), so the keys/quorum are the node's own
            // identity key + the bootstrap quorum. The handler forwards to that
            // stub.
            // The finalizer signs blocks with its identity's hybrid provider;
            // the verifying key is derived from the identity's Ed25519 public
            // key so `finalizer_addr` matches the signing key.
            let verifying_key: VerifyingKey = {
                let pk = config.public_key.clone();
                let pk_bytes: [u8; 32] = pk
                    .try_into()
                    .expect("finalizer public key must be 32 bytes");
                VerifyingKey::from_bytes(&pk_bytes).expect("valid verifying key")
            };
            let signature_registry = Arc::new(TransactionSignatureRegistry::new());
            let finalizer = pneumatic_finalizer::Finalizer::new(
                env_data.environment_id.clone(),
                config.public_key.clone(),
                config.identity.clone(),
                node_registry,
                pending_registry,
                signature_registry,
                66.6,
                4,
                config.identity.clone(),
                verifying_key,
                hash_provider,
                vec![],
                0,
                vec![],
                1,
                data_provider,
                env_data.token_partition_id.clone(),
                // S5.2: the finalizer re-runs the shielded validation
                // against the same pool-view seam the sentinel arm consumes
                // (one view per environment — the S5.4 swap site).
                shielded_pool_view,
                env_data.clone(),
            );
            Some(Box::new(finalizer))
        }
        // Archiver has no role-plugin this host hosts.
        NodeRegistryType::Archiver => None,
    }
}
