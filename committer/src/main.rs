use std::sync::Arc;

use dashmap::DashMap;
use pneumatic_core::config::Config;
use pneumatic_core::crypto::BasicHashProvider;
use pneumatic_core::data::DefaultDataProvider;
use pneumatic_core::epoch::{BlockProposer, CandidateRegistry, Epoch, EpochBoundaryDetector};
use pneumatic_core::gossiper::Gossiper;
use pneumatic_core::logging::Logger;
use pneumatic_core::node::registry::NodeRegistry;
use pneumatic_core::registry::PendingTransactionRegistry;

use pneumatic_committer::block_services::BlockServices;
use pneumatic_committer::committer::Committer;
use pneumatic_committer::epoch_manager::{
    EpochReconciler, LeaderSelector, StakeStore, StakingManager,
};

#[tokio::main]
async fn main() {
    // 1. Build config and environment metadata
    let config = match Config::build() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Failed to build config: {:?}", e);
            return;
        }
    };

    // Get environment metadata for the main environment
    let env_id = &config.main_environment_id;
    let env_data = match config.environment_metadata.get(env_id) {
        Some(entry) => entry.value().clone(),
        None => {
            eprintln!("No environment metadata found for id: {}", env_id);
            return;
        }
    };

    // 2. Initialize NodeRegistry (consumes config)
    let conn_factory = pneumatic_core::conns::factories::ConnFactory::new();
    let on_received = Arc::new(|_data: Vec<u8>| {
        // TODO: handle incoming raw data
    });
    let node_registry = Arc::new(NodeRegistry::init(
        Arc::new(config),
        Box::new(conn_factory),
        on_received,
    ));

    // 3. Create Gossiper — build a fresh config since NodeRegistry consumed the original
    let gossiper = Arc::new(Gossiper::new(
        pneumatic_core::node::NodeRegistryType::Committer,
        Config::build().unwrap_or_else(|e| {
            eprintln!("Failed to build config for gossiper: {:?}", e);
            std::process::exit(1);
        }),
        60, // 60s TTL
        env_data.asym_crypto_provider.clone(),
    ));

    // 4. Create shared env_data Arc (clone for shared ownership)
    let env_data = Arc::new(env_data);

    // 5. Create shared logger
    let shared_logger: Arc<dyn Logger> = env_data.logger.clone();

    // 6. Create StakeStore and StakingManager
    let stake_store = Arc::new(StakeStore::new());
    let staking_manager = Arc::new(StakingManager::new(
        stake_store.clone(),
        shared_logger.clone(),
    ));

    // 7. Create EpochReconciler and LeaderSelector
    let data_provider = Arc::new(DefaultDataProvider::new());
    let candidate_registry = Arc::new(CandidateRegistry::new());
    let epoch_reconciler = Arc::new(EpochReconciler::new(
        stake_store.clone(),
        candidate_registry,
        data_provider.clone(),
        env_data.environment_id.clone(),
        vec![], // token IDs: populated dynamically via token distribution
    ));

    let hash_provider = Arc::new(BasicHashProvider::new());
    let leader_selector = Arc::new(LeaderSelector::new(hash_provider));

    // 8. Create Token cache
    let tokens = Arc::new(DashMap::new());

    // 9. Create PendingTransactionRegistry
    let pending_registry = Arc::new(PendingTransactionRegistry::new());

    // 9.5. Create epoch tracking components
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let epoch_duration = 300; // 5 minutes
    let initial_epoch = Epoch::new_with_leader(
        1,
        now,
        now + epoch_duration,
        leader_selector.as_ref(),
        &stake_store.to_stake_set(),
    );
    let epoch_detector = EpochBoundaryDetector::new(initial_epoch);
    let block_proposer = Arc::new(BlockProposer::new(vec![], 0, vec![]));

    // 10.5. Create CandidateRegistry for conflict detection
    let candidate_registry = Arc::new(CandidateRegistry::new());

    // 10. Create BlockServices
    let block_services = Arc::new(BlockServices::new(
        tokens.clone(),
        data_provider.clone(),
        node_registry.clone(),
        env_data.clone(),
        shared_logger.clone(),
    ));

    // 11. Create Committer
    let committer = Arc::new(Committer::new(
        env_data.clone(),
        vec![], // public_key: not yet available without crypto impl
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
        0, // current_epoch_number
        Some(epoch_detector),
        block_proposer,
        epoch_duration,
        5000, // proposal_interval_ms: check every 5 seconds
        candidate_registry,
    ));

    // 12. Wire up gossiper message handler
    let committer_clone = committer.clone();
    let logger_clone = shared_logger.clone();
    committer.initialize(move |message| {
        let committer = committer_clone.clone();
        let logger = logger_clone.clone();
        tokio::spawn(async move {
            if let Err(e) = committer.handle_message(message).await {
                logger.log(format!("Committer error: {:?}", e));
            }
        });
    });

    // 13. Start background epoch loop — polls for block proposals periodically
    let epoch_committer = committer.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = epoch_committer.run_epoch_loop().await {
                // Log but don't crash — epoch loop errors are non-fatal
                epoch_committer.logger()
                    .log(format!("Epoch loop error: {:?}", e));
            }
            tokio::time::sleep(std::time::Duration::from_millis(
                epoch_committer.proposal_interval_ms(),
            ))
            .await;
        }
    });

    // 14. Log startup and block on shutdown
    shared_logger.log("Committer node started".to_string());

    // Block the main thread indefinitely (node runs until killed)
    // In production, this would listen for a shutdown signal
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}
