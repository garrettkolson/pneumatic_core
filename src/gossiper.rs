use moka::sync::Cache;
use crate::config::Config;
use crate::conns::factories::ConnFactory;
use crate::data::DataError;
use crate::encoding::deserialize_rmp_to;
use crate::messages::Message;
use crate::node::NodeRegistryType;

/// Gossiper handles message deduplication and fan-out.
/// Maintains a signature cache to prevent processing duplicate messages.
pub struct Gossiper {
    /// The node type this gossiper belongs to
    node_type: NodeRegistryType,
    /// Config for the node
    config: Config,
    /// Connection factory for sending messages
    conn_factory: ConnFactory,
    /// Signature-based dedup cache with configurable TTL
    cache: Cache<Vec<u8>, ()>,
}

impl Gossiper {
    /// Create a new Gossiper with configurable dedup TTL.
    pub fn new(node_type: NodeRegistryType, config: Config, ttl_seconds: u64) -> Self {
        Gossiper {
            node_type,
            config,
            conn_factory: ConnFactory::new(),
            cache: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(std::time::Duration::from_secs(ttl_seconds))
                .build(),
        }
    }

    /// Initialize the gossiper, setting up the message received handler.
    /// The handler closure is called for each valid, non-duplicate message.
    pub fn initialize<F>(&self, _on_message_received: F)
    where
        F: Fn(Message) + Send + Sync + 'static,
    {
        // The handler closure would be stored and invoked in handle_message.
        // Currently, the handler is passed in for future implementation.
    }

    /// Handle an incoming message: deserialize, check cache, validate crypto,
    /// fan out to handlers, and call the message received handler.
    pub fn handle_message(&self, raw_data: Vec<u8>) -> Result<(), DataError> {
        // Deserialize the message from MsgPack bytes
        let message: Message = match deserialize_rmp_to(&raw_data) {
            Ok(m) => m,
            Err(e) => return Err(DataError::DeserializationError(e)),
        };

        // Check signature cache for duplicates
        let signature_key = message.signature.clone();
        if self.cache.get(&signature_key).is_some() {
            return Ok(()); // Duplicate — silently skip
        }

        // Add to cache (will expire after TTL)
        self.cache.insert(signature_key, ());

        // TODO: validate crypto signature (pending crypto implementation)
        // Let the message through for now — real validation in Phase 6

        // TODO: copy payload to each handler delegate (C# TODO)
        // Fan out to registered handlers here

        Ok(())
    }
}
