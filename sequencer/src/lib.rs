pub mod api;
pub mod block;
pub mod catchup;
mod chain_config;
pub mod context;
pub mod eth_signature_key;
mod header;
pub mod hotshot_commitment;
pub mod options;
pub mod state_signature;

use anyhow::Context;
use async_std::sync::RwLock;
use async_trait::async_trait;
use block::entry::TxTableEntryWord;
use catchup::{StateCatchup, StatePeers};
use context::SequencerContext;
use ethers::types::{Address, U256};

// Should move `STAKE_TABLE_CAPACITY` in the sequencer repo when we have variate stake table support

use l1_client::L1Client;

use state::FeeAccount;
use state_signature::static_stake_table_commitment;
use url::Url;
pub mod l1_client;
pub mod persistence;
pub mod state;
pub mod transaction;

use derivative::Derivative;
use hotshot::{
    traits::{
        election::static_committee::GeneralStaticCommittee,
        implementations::{
            derive_libp2p_peer_id, KeyPair, MemoryNetwork, NetworkingMetricsValue, PushCdnNetwork,
            Topic, WrappedSignatureKey,
        },
    },
    types::SignatureKey,
    Networks,
};
use hotshot_orchestrator::{
    client::{OrchestratorClient, ValidatorArgs},
    config::NetworkConfig,
};
use hotshot_types::{
    consensus::CommitmentMap,
    data::{DAProposal, VidDisperseShare, ViewNumber},
    event::HotShotAction,
    light_client::{StateKeyPair, StateSignKey},
    message::Proposal,
    signature_key::{BLSPrivKey, BLSPubKey},
    simple_certificate::QuorumCertificate,
    traits::{
        metrics::Metrics,
        network::ConnectedNetwork,
        node_implementation::{NodeImplementation, NodeType},
        states::InstanceState,
        storage::Storage,
    },
    utils::{BuilderCommitment, View},
    ValidatorConfig,
};
use persistence::{PersistenceOptions, SequencerPersistence};
use serde::{Deserialize, Serialize};
use snafu::Snafu;
use std::{collections::BTreeMap, fmt::Debug, marker::PhantomData, net::SocketAddr, sync::Arc};
use vbs::version::StaticVersionType;

#[cfg(feature = "libp2p")]
use std::time::Duration;

#[cfg(feature = "libp2p")]
use hotshot::traits::implementations::{CombinedNetworks, Libp2pNetwork};

pub use block::payload::Payload;
pub use chain_config::ChainConfig;
pub use header::Header;
pub use l1_client::L1BlockInfo;
pub use options::Options;
pub use state::ValidatedState;
pub use transaction::{NamespaceId, Transaction};
pub mod network;

/// The Sequencer node is generic over the hotshot CommChannel.
#[derive(Derivative, Serialize, Deserialize)]
#[derivative(
    Copy(bound = ""),
    Debug(bound = ""),
    Default(bound = ""),
    PartialEq(bound = ""),
    Eq(bound = ""),
    Hash(bound = "")
)]
pub struct Node<N: network::Type, P: SequencerPersistence>(PhantomData<fn(&N, &P)>);

// Using derivative to derive Clone triggers the clippy lint
// https://rust-lang.github.io/rust-clippy/master/index.html#/incorrect_clone_impl_on_copy_type
impl<N: network::Type, P: SequencerPersistence> Clone for Node<N, P> {
    fn clone(&self) -> Self {
        *self
    }
}

#[derive(
    Clone, Copy, Debug, Default, Hash, Eq, PartialEq, PartialOrd, Ord, Deserialize, Serialize,
)]
pub struct SeqTypes;

pub type Leaf = hotshot_types::data::Leaf<SeqTypes>;
pub type Event = hotshot::types::Event<SeqTypes>;

pub type PubKey = BLSPubKey;
pub type PrivKey = <PubKey as SignatureKey>::PrivateKey;

impl<N: network::Type, P: SequencerPersistence> NodeImplementation<SeqTypes> for Node<N, P> {
    type QuorumNetwork = N::QuorumChannel;
    type CommitteeNetwork = N::DAChannel;
    type Storage = Arc<RwLock<P>>;
}

#[async_trait]
impl<P: SequencerPersistence> Storage<SeqTypes> for Arc<RwLock<P>> {
    async fn append_vid(
        &self,
        proposal: &Proposal<SeqTypes, VidDisperseShare<SeqTypes>>,
    ) -> anyhow::Result<()> {
        self.write().await.append_vid(proposal).await
    }

    async fn append_da(
        &self,
        proposal: &Proposal<SeqTypes, DAProposal<SeqTypes>>,
    ) -> anyhow::Result<()> {
        self.write().await.append_da(proposal).await
    }
    async fn record_action(&self, view: ViewNumber, action: HotShotAction) -> anyhow::Result<()> {
        self.write().await.record_action(view, action).await
    }
    async fn update_high_qc(&self, _high_qc: QuorumCertificate<SeqTypes>) -> anyhow::Result<()> {
        Ok(())
    }

    async fn update_undecided_state(
        &self,
        _leaves: CommitmentMap<Leaf>,
        _state: BTreeMap<ViewNumber, View<SeqTypes>>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct NodeState {
    node_id: u64,
    chain_config: ChainConfig,
    l1_client: L1Client,
    peers: Arc<dyn StateCatchup>,
    genesis_state: ValidatedState,
    l1_genesis: Option<L1BlockInfo>,
}

impl NodeState {
    pub fn new(
        node_id: u64,
        chain_config: ChainConfig,
        l1_client: L1Client,
        catchup: impl StateCatchup + 'static,
    ) -> Self {
        Self {
            node_id,
            chain_config,
            l1_client,
            peers: Arc::new(catchup),
            genesis_state: Default::default(),
            l1_genesis: None,
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn mock() -> Self {
        Self::new(
            0,
            ChainConfig::default(),
            L1Client::new("http://localhost:3331".parse().unwrap(), Address::default()),
            catchup::mock::MockStateCatchup::default(),
        )
    }

    pub fn with_l1(mut self, l1_client: L1Client) -> Self {
        self.l1_client = l1_client;
        self
    }

    pub fn with_genesis(mut self, state: ValidatedState) -> Self {
        self.genesis_state = state;
        self
    }

    fn l1_client(&self) -> &L1Client {
        &self.l1_client
    }
}

// This allows us to turn on `Default` on InstanceState trait
// which is used in `HotShot` by `TestBuilderImplementation`.
#[cfg(any(test, feature = "testing"))]
impl Default for NodeState {
    fn default() -> Self {
        Self::new(
            1u64,
            ChainConfig::default(),
            L1Client::new("http://localhost:3331".parse().unwrap(), Address::default()),
            catchup::mock::MockStateCatchup::default(),
        )
    }
}

impl InstanceState for NodeState {}

impl NodeType for SeqTypes {
    type Time = ViewNumber;
    type BlockHeader = Header;
    type BlockPayload = Payload<TxTableEntryWord>;
    type SignatureKey = PubKey;
    type Transaction = Transaction;
    type InstanceState = NodeState;
    type ValidatedState = ValidatedState;
    type Membership = GeneralStaticCommittee<Self, PubKey>;
    type BuilderSignatureKey = FeeAccount;
}

#[derive(Clone, Debug, Snafu, Deserialize, Serialize)]
pub enum Error {
    // TODO: Can we nest these errors in a `ValidationError` to group them?

    // Parent state commitment of block doesn't match current state commitment
    IncorrectParent,

    // New view number isn't strictly after current view
    IncorrectView,

    // Genesis block either has zero or more than one transaction
    GenesisWrongSize,

    // Genesis transaction not present in genesis block
    MissingGenesis,

    // Genesis transaction in non-genesis block
    UnexpectedGenesis,

    // Merkle tree error
    MerkleTreeError { error: String },

    BlockBuilding,
}

#[derive(Clone, Debug)]
pub struct NetworkParams {
    /// The address where a CDN marshal is located
    pub cdn_endpoint: String,
    pub orchestrator_url: Url,
    pub state_relay_server_url: Url,
    pub private_staking_key: BLSPrivKey,
    pub private_state_key: StateSignKey,
    pub state_peers: Vec<Url>,
    /// The address to send to other Libp2p nodes to contact us
    pub libp2p_advertise_address: SocketAddr,
    /// The address to bind to for Libp2p
    pub libp2p_bind_address: SocketAddr,
}

#[derive(Clone, Debug)]
pub struct BuilderParams {
    pub prefunded_accounts: Vec<Address>,
}

pub struct L1Params {
    pub url: Url,
    pub finalized_block: Option<u64>,
}

#[allow(clippy::too_many_arguments)]
pub async fn init_node<P: PersistenceOptions, Ver: StaticVersionType + 'static>(
    network_params: NetworkParams,
    metrics: &dyn Metrics,
    persistence_opt: P,
    builder_params: BuilderParams,
    l1_params: L1Params,
    stake_table_capacity: usize,
    bind_version: Ver,
    chain_config: ChainConfig,
    is_da: bool,
) -> anyhow::Result<SequencerContext<network::Production, P::Persistence, Ver>> {
    // Expose git information via status API.
    metrics
        .create_label("git_rev".into())
        .set(env!("VERGEN_GIT_SHA").into());
    metrics
        .create_label("git_desc".into())
        .set(env!("VERGEN_GIT_DESCRIBE").into());
    metrics
        .create_label("git_timestamp".into())
        .set(env!("VERGEN_GIT_COMMIT_TIMESTAMP").into());

    // Orchestrator client
    let validator_args = ValidatorArgs {
        url: network_params.orchestrator_url,
        advertise_address: Some(network_params.libp2p_advertise_address),
        network_config_file: None,
    };
    let orchestrator_client = OrchestratorClient::new(validator_args);
    let state_key_pair = StateKeyPair::from_sign_key(network_params.private_state_key);
    let my_config = ValidatorConfig {
        public_key: BLSPubKey::from_private(&network_params.private_staking_key),
        private_key: network_params.private_staking_key,
        stake_value: 1,
        state_key_pair,
        is_da,
    };

    // Derive our Libp2p public key from our private key
    let libp2p_public_key =
        derive_libp2p_peer_id::<<SeqTypes as NodeType>::SignatureKey>(&my_config.private_key)
            .with_context(|| "Failed to derive Libp2p peer ID")?;

    let mut persistence = persistence_opt.clone().create().await?;
    let (config, wait_for_orchestrator) = match persistence.load_config().await? {
        Some(config) => {
            tracing::info!("loaded network config from storage, rejoining existing network");
            (config, false)
        }
        None => {
            tracing::info!("loading network config from orchestrator");
            tracing::error!(
                "waiting for other nodes to connect, DO NOT RESTART until fully connected"
            );
            let config = NetworkConfig::get_complete_config(
                &orchestrator_client,
                my_config.clone(),
                // Register in our Libp2p advertise address and public key so other nodes
                // can contact us on startup
                Some(network_params.libp2p_advertise_address),
                Some(libp2p_public_key),
            )
            .await?
            .0;

            tracing::info!(
                node_id = config.node_index,
                stake_table = ?config.config.known_nodes_with_stake,
                "loaded config",
            );
            persistence.save_config(&config).await?;
            tracing::error!("all nodes connected");
            (config, true)
        }
    };
    let node_index = config.node_index;

    // If we are a DA node, we need to subscribe to the DA topic
    let topics = {
        let mut topics = vec![Topic::Global];
        if is_da {
            topics.push(Topic::DA);
        }
        topics
    };

    // Initialize the push CDN network (and perform the initial connection)
    let cdn_network = PushCdnNetwork::new(
        network_params.cdn_endpoint,
        topics,
        KeyPair {
            public_key: WrappedSignatureKey(my_config.public_key),
            private_key: my_config.private_key.clone(),
        },
    )
    .with_context(|| "Failed to create CDN network")?;

    // Initialize the Libp2p network (if enabled)
    #[cfg(feature = "libp2p")]
    let p2p_network = Libp2pNetwork::from_config::<SeqTypes>(
        config.clone(),
        network_params.libp2p_bind_address,
        &my_config.public_key,
        // We need the private key so we can derive our Libp2p keypair
        // (using https://docs.rs/blake3/latest/blake3/fn.derive_key.html)
        &my_config.private_key,
    )
    .await
    .with_context(|| "Failed to create libp2p network")?;

    // Combine the communication channels
    #[cfg(feature = "libp2p")]
    let (da_network, quorum_network) = {
        (
            Arc::from(CombinedNetworks::new(
                cdn_network.clone(),
                p2p_network.clone(),
                Duration::from_secs(1),
            )),
            Arc::from(CombinedNetworks::new(
                cdn_network,
                p2p_network,
                Duration::from_secs(1),
            )),
        )
    };

    // Wait for the CDN network to be ready if we're not using the P2P network
    #[cfg(not(feature = "libp2p"))]
    let (da_network, quorum_network) = {
        tracing::warn!("Waiting for the CDN connection to be initialized");
        cdn_network.wait_for_ready().await;
        tracing::warn!("CDN connection initialized");
        (Arc::from(cdn_network.clone()), Arc::from(cdn_network))
    };

    // Convert to the sequencer-compatible type
    let networks = Networks {
        da_network,
        quorum_network,
        _pd: Default::default(),
    };

    // The web server network doesn't have any metrics. By creating and dropping a
    // `NetworkingMetricsValue`, we ensure the networking metrics are created, but just not
    // populated, so that monitoring software built to work with network-related metrics doesn't
    // crash horribly just because we're not using the P2P network yet.
    let _ = NetworkingMetricsValue::new(metrics);

    let mut genesis_state = ValidatedState::default();
    for address in builder_params.prefunded_accounts {
        tracing::info!("Prefunding account {:?} for demo", address);
        genesis_state.prefund_account(address.into(), U256::max_value().into());
    }

    let l1_client = L1Client::new(l1_params.url, Address::default());
    let l1_genesis = match l1_params.finalized_block {
        Some(block) => Some(l1_client.get_block(block).await?),
        None => None,
    };

    let instance_state = NodeState {
        chain_config,
        l1_client,
        genesis_state,
        l1_genesis,
        peers: catchup::local_and_remote(
            persistence_opt,
            StatePeers::<Ver>::from_urls(network_params.state_peers),
        )
        .await,
        node_id: node_index,
    };

    let mut ctx = SequencerContext::init(
        config.config,
        instance_state,
        persistence,
        networks,
        Some(network_params.state_relay_server_url),
        metrics,
        stake_table_capacity,
        bind_version,
    )
    .await?;
    if wait_for_orchestrator {
        ctx = ctx.wait_for_orchestrator(orchestrator_client);
    }
    Ok(ctx)
}

pub fn empty_builder_commitment() -> BuilderCommitment {
    BuilderCommitment::from_bytes([])
}

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;
    use crate::{
        catchup::mock::MockStateCatchup,
        eth_signature_key::EthKeyPair,
        persistence::no_storage::{self, NoStorage},
    };
    use committable::Committable;
    use ethers::utils::{Anvil, AnvilInstance};
    use futures::{
        future::join_all,
        stream::{Stream, StreamExt},
    };
    use hotshot::traits::{
        implementations::{MasterMap, MemoryNetwork},
        BlockPayload,
    };
    use hotshot::types::{EventType::Decide, Message};
    use hotshot_testing::block_builder::{
        BuilderTask, SimpleBuilderImplementation, TestBuilderImplementation,
    };
    use hotshot_types::{
        event::LeafInfo,
        light_client::StateKeyPair,
        traits::{
            block_contents::BlockHeader, metrics::NoMetrics, signature_key::BuilderSignatureKey,
        },
        ExecutionType, HotShotConfig, PeerConfig, ValidatorConfig,
    };
    use portpicker::pick_unused_port;
    use std::time::Duration;

    const STAKE_TABLE_CAPACITY_FOR_TEST: usize = 10;

    pub async fn run_test_builder() -> (Option<Box<dyn BuilderTask<SeqTypes>>>, Url) {
        <SimpleBuilderImplementation as TestBuilderImplementation<SeqTypes>>::start(
            TestConfig::NUM_NODES,
            (),
        )
        .await
    }

    #[derive(Clone)]
    pub struct TestConfig {
        config: HotShotConfig<PubKey>,
        priv_keys: Vec<BLSPrivKey>,
        state_key_pairs: Vec<StateKeyPair>,
        master_map: Arc<MasterMap<Message<SeqTypes>, PubKey>>,
        anvil: Arc<AnvilInstance>,
    }

    impl Default for TestConfig {
        fn default() -> Self {
            let num_nodes = Self::NUM_NODES;

            // Generate keys for the nodes.
            let seed = [0; 32];
            let (pub_keys, priv_keys): (Vec<_>, Vec<_>) = (0..num_nodes)
                .map(|i| <PubKey as SignatureKey>::generated_from_seed_indexed(seed, i as u64))
                .unzip();
            let state_key_pairs = (0..num_nodes)
                .map(|i| StateKeyPair::generate_from_seed_indexed(seed, i as u64))
                .collect::<Vec<_>>();
            let known_nodes_with_stake = pub_keys
                .iter()
                .zip(&state_key_pairs)
                .map(|(pub_key, state_key_pair)| PeerConfig::<PubKey> {
                    stake_table_entry: pub_key.get_stake_table_entry(1),
                    state_ver_key: state_key_pair.ver_key(),
                })
                .collect::<Vec<_>>();

            let master_map = MasterMap::new();

            let config: HotShotConfig<PubKey> = HotShotConfig {
                fixed_leader_for_gpuvid: 0,
                execution_type: ExecutionType::Continuous,
                num_nodes_with_stake: num_nodes.try_into().unwrap(),
                num_nodes_without_stake: 0,
                known_da_nodes: known_nodes_with_stake.clone(),
                known_nodes_with_stake: known_nodes_with_stake.clone(),
                known_nodes_without_stake: vec![],
                next_view_timeout: Duration::from_secs(5).as_millis() as u64,
                timeout_ratio: (10, 11),
                round_start_delay: Duration::from_millis(1).as_millis() as u64,
                start_delay: Duration::from_millis(1).as_millis() as u64,
                num_bootstrap: 1usize,
                da_staked_committee_size: num_nodes,
                da_non_staked_committee_size: 0,
                my_own_validator_config: Default::default(),
                view_sync_timeout: Duration::from_secs(1),
                data_request_delay: Duration::from_secs(1),
                //??
                builder_url: Url::parse(&format!(
                    "http://127.0.0.1:{}",
                    pick_unused_port().unwrap()
                ))
                .unwrap(),
                builder_timeout: Duration::from_secs(1),
                start_threshold: (
                    known_nodes_with_stake.clone().len() as u64,
                    known_nodes_with_stake.clone().len() as u64,
                ),
            };

            Self {
                config,
                priv_keys,
                state_key_pairs,
                master_map,
                anvil: Arc::new(Anvil::new().spawn()),
            }
        }
    }

    impl TestConfig {
        pub const NUM_NODES: usize = 4;

        pub fn num_nodes(&self) -> usize {
            self.priv_keys.len()
        }

        pub fn hotshot_config(&self) -> &HotShotConfig<PubKey> {
            &self.config
        }

        pub fn set_builder_url(&mut self, builder_url: Url) {
            self.config.builder_url = builder_url;
        }

        pub async fn init_nodes<Ver: StaticVersionType + 'static>(
            &self,
            bind_version: Ver,
        ) -> Vec<SequencerContext<network::Memory, NoStorage, Ver>> {
            join_all((0..self.num_nodes()).map(|i| async move {
                self.init_node(
                    i,
                    ValidatedState::default(),
                    no_storage::Options,
                    MockStateCatchup::default(),
                    &NoMetrics,
                    STAKE_TABLE_CAPACITY_FOR_TEST,
                    bind_version,
                    true,
                )
                .await
            }))
            .await
        }

        #[allow(clippy::too_many_arguments)]
        pub async fn init_node<Ver: StaticVersionType + 'static, P: PersistenceOptions>(
            &self,
            i: usize,
            mut state: ValidatedState,
            persistence_opt: P,
            catchup: impl StateCatchup + 'static,
            metrics: &dyn Metrics,
            stake_table_capacity: usize,
            bind_version: Ver,
            is_da: bool,
        ) -> SequencerContext<network::Memory, P::Persistence, Ver> {
            let mut config = self.config.clone();
            config.my_own_validator_config = ValidatorConfig {
                public_key: config.known_nodes_with_stake[i].stake_table_entry.stake_key,
                private_key: self.priv_keys[i].clone(),
                stake_value: config.known_nodes_with_stake[i]
                    .stake_table_entry
                    .stake_amount
                    .as_u64(),
                state_key_pair: self.state_key_pairs[i].clone(),
                is_da,
            };

            let network = Arc::new(MemoryNetwork::new(
                config.my_own_validator_config.public_key,
                NetworkingMetricsValue::new(metrics),
                self.master_map.clone(),
                None,
            ));
            let networks = Networks {
                da_network: network.clone(),
                quorum_network: network,
                _pd: Default::default(),
            };

            // Make sure the builder account is funded.
            let builder_account = Self::builder_key().fee_account();
            tracing::info!(%builder_account, "prefunding builder account");
            state.prefund_account(builder_account, U256::max_value().into());
            let node_state = NodeState::new(
                i as u64,
                ChainConfig::default(),
                L1Client::new(self.anvil.endpoint().parse().unwrap(), Address::default()),
                catchup::local_and_remote(persistence_opt.clone(), catchup).await,
            )
            .with_genesis(state);

            tracing::info!(
                i,
                key = %config.my_own_validator_config.public_key,
                state_key = %config.my_own_validator_config.state_key_pair.ver_key(),
                "starting node",
            );
            SequencerContext::init(
                config,
                node_state,
                persistence_opt.create().await.unwrap(),
                networks,
                None,
                metrics,
                stake_table_capacity,
                bind_version,
            )
            .await
            .unwrap()
        }

        pub fn builder_key() -> EthKeyPair {
            FeeAccount::generated_from_seed_indexed([1; 32], 0).1
        }
    }

    // Wait for decide event, make sure it matches submitted transaction. Return the block number
    // containing the transaction.
    pub async fn wait_for_decide_on_handle(
        events: &mut (impl Stream<Item = Event> + Unpin),
        submitted_txn: &Transaction,
    ) -> u64 {
        let commitment = submitted_txn.commit();

        // Keep getting events until we see a Decide event
        loop {
            let event = events.next().await.unwrap();
            tracing::info!("Received event from handle: {event:?}");

            if let Decide { leaf_chain, .. } = event.event {
                if let Some(height) = leaf_chain.iter().find_map(|LeafInfo { leaf, .. }| {
                    if leaf
                        .get_block_payload()
                        .as_ref()?
                        .transaction_commitments(leaf.get_block_header().metadata())
                        .contains(&commitment)
                    {
                        Some(leaf.get_block_header().block_number())
                    } else {
                        None
                    }
                }) {
                    return height;
                }
            } else {
                // Keep waiting
            }
        }
    }
}

#[cfg(test)]
mod test {

    use self::testing::run_test_builder;

    use super::*;
    use async_compatibility_layer::logging::{setup_backtrace, setup_logging};

    use es_version::SequencerVersion;
    use futures::StreamExt;
    use hotshot::types::EventType::Decide;
    use hotshot_types::{
        event::LeafInfo,
        traits::block_contents::{
            vid_commitment, BlockHeader, BlockPayload, GENESIS_VID_NUM_STORAGE_NODES,
        },
    };
    use testing::{wait_for_decide_on_handle, TestConfig};

    #[async_std::test]
    async fn test_skeleton_instantiation() {
        setup_logging();
        setup_backtrace();
        let ver = SequencerVersion::instance();
        // Assign `config` so it isn't dropped early.
        let mut config = TestConfig::default();

        let (builder_task, builder_url) = run_test_builder().await;

        config.set_builder_url(builder_url);

        let handles = config.init_nodes(ver).await;

        let handle_0 = &handles[0];

        // Hook the builder up to the event stream from the first node
        if let Some(builder_task) = builder_task {
            builder_task.start(Box::new(handle_0.get_event_stream()));
        }

        let mut events = handle_0.get_event_stream();

        for handle in handles.iter() {
            handle.start_consensus().await;
        }

        // Submit target transaction to handle
        let txn = Transaction::new(Default::default(), vec![1, 2, 3]);
        handles[0]
            .submit_transaction(txn.clone())
            .await
            .expect("Failed to submit transaction");
        tracing::info!("Submitted transaction to handle: {txn:?}");

        wait_for_decide_on_handle(&mut events, &txn).await;
    }

    #[async_std::test]
    async fn test_header_invariants() {
        setup_logging();
        setup_backtrace();

        let success_height = 30;
        let ver = SequencerVersion::instance();
        // Assign `config` so it isn't dropped early.
        let mut config = TestConfig::default();

        let (builder_task, builder_url) = run_test_builder().await;

        config.set_builder_url(builder_url);
        let handles = config.init_nodes(ver).await;

        let handle_0 = &handles[0];

        let mut events = handle_0.get_event_stream();

        // Hook the builder up to the event stream from the first node
        if let Some(builder_task) = builder_task {
            builder_task.start(Box::new(handle_0.get_event_stream()));
        }

        for handle in handles.iter() {
            handle.start_consensus().await;
        }

        let mut parent = {
            // TODO refactor repeated code from other tests
            let (genesis_payload, genesis_ns_table) =
                Payload::from_transactions([], &NodeState::mock()).unwrap();
            let genesis_commitment = {
                // TODO we should not need to collect payload bytes just to compute vid_commitment
                let payload_bytes = genesis_payload
                    .encode()
                    .expect("unable to encode genesis payload");
                vid_commitment(&payload_bytes, GENESIS_VID_NUM_STORAGE_NODES)
            };
            let genesis_state = NodeState::mock();
            Header::genesis(
                &genesis_state,
                genesis_commitment,
                empty_builder_commitment(),
                genesis_ns_table,
            )
        };

        loop {
            let event = events.next().await.unwrap();
            tracing::info!("Received event from handle: {event:?}");
            let Decide { leaf_chain, .. } = event.event else {
                continue;
            };
            tracing::info!("Got decide {leaf_chain:?}");

            // Check that each successive header satisfies invariants relative to its parent: all
            // the fields which should be monotonic are.
            for LeafInfo { leaf, .. } in leaf_chain.iter().rev() {
                let header = leaf.get_block_header().clone();
                if header.height == 0 {
                    parent = header;
                    continue;
                }
                assert_eq!(header.height, parent.height + 1);
                assert!(header.timestamp >= parent.timestamp);
                assert!(header.l1_head >= parent.l1_head);
                assert!(header.l1_finalized >= parent.l1_finalized);
                parent = header;
            }

            if parent.height >= success_height {
                break;
            }
        }
    }
}
