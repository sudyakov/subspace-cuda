use crate::cli::SubspaceCliPlaceholder;
use crate::domain::{DomainCli, DomainInstanceStarter};
use crate::{derive_pot_external_entropy, set_default_ss58_version, Error, PosTable};
use clap::Parser;
use cross_domain_message_gossip::GossipWorkerBuilder;
use domain_client_operator::Bootstrapper;
use domain_runtime_primitives::opaque::Block as DomainBlock;
use futures::FutureExt;
use sc_cli::{print_node_infos, CliConfiguration, RunCmd, Signals, SubstrateCli};
use sc_consensus_slots::SlotProportion;
use sc_network::config::NodeKeyConfig;
use sc_service::config::KeystoreConfig;
use sc_service::DatabaseSource;
use sc_storage_monitor::{StorageMonitorParams, StorageMonitorService};
use sc_transaction_pool_api::OffchainTransactionPoolFactory;
use sc_utils::mpsc::tracing_unbounded;
use sp_core::traits::SpawnEssentialNamed;
use sp_messenger::messages::ChainId;
use std::collections::HashSet;
use subspace_networking::libp2p::Multiaddr;
use subspace_runtime::{Block, ExecutorDispatch, RuntimeApi};
use subspace_service::{DsnConfig, SubspaceConfiguration, SubspaceNetworking};
use tokio::runtime::Handle;

fn parse_pot_external_entropy(s: &str) -> Result<Vec<u8>, hex::FromHexError> {
    hex::decode(s)
}

fn parse_timekeeper_cpu_cores(
    s: &str,
) -> Result<HashSet<usize>, Box<dyn std::error::Error + Send + Sync>> {
    if s.is_empty() {
        return Ok(HashSet::new());
    }

    let mut cpu_cores = HashSet::new();
    for s in s.split(',') {
        let mut parts = s.split('-');
        let range_start = parts
            .next()
            .ok_or("Bad string format, must be comma separated list of CPU cores or ranges")?
            .parse()?;
        if let Some(range_end) = parts.next() {
            let range_end = range_end.parse()?;

            cpu_cores.extend(range_start..=range_end);
        } else {
            cpu_cores.insert(range_start);
        }
    }

    Ok(cpu_cores)
}

/// Options for running a node
#[derive(Debug, Parser)]
pub struct RunOptions {
    /// Run a node
    #[clap(flatten)]
    run: RunCmd,

    /// External entropy, used initially when PoT chain starts to derive the first seed
    #[arg(long, value_parser = parse_pot_external_entropy)]
    pot_external_entropy: Option<Vec<u8>>,

    /// Options for DSN
    #[clap(flatten)]
    dsn_options: DsnOptions,

    /// Enables DSN-sync on startup.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    sync_from_dsn: bool,

    /// Parameters used to create the storage monitor.
    #[clap(flatten)]
    storage_monitor: StorageMonitorParams,

    #[clap(flatten)]
    timekeeper_options: TimekeeperOptions,

    /// Domain arguments
    ///
    /// The command-line arguments provided first will be passed to the embedded consensus node,
    /// while the arguments provided after `--` will be passed to the domain node.
    ///
    /// subspace-node [consensus-chain-args] -- [domain-args]
    #[arg(raw = true)]
    domain_args: Vec<String>,
}

/// Options for DSN
#[derive(Debug, Parser)]
struct DsnOptions {
    /// Where local DSN node will listen for incoming connections.
    // TODO: Add more DSN-related parameters
    #[arg(long, default_values_t = [
        "/ip4/0.0.0.0/udp/30433/quic-v1".parse::<Multiaddr>().expect("Manual setting"),
        "/ip4/0.0.0.0/tcp/30433".parse::<Multiaddr>().expect("Manual setting"),
    ])]
    dsn_listen_on: Vec<Multiaddr>,

    /// Bootstrap nodes for DSN.
    #[arg(long)]
    dsn_bootstrap_nodes: Vec<Multiaddr>,

    /// Reserved peers for DSN.
    #[arg(long)]
    dsn_reserved_peers: Vec<Multiaddr>,

    /// Defines max established incoming connection limit for DSN.
    #[arg(long, default_value_t = 50)]
    dsn_in_connections: u32,

    /// Defines max established outgoing swarm connection limit for DSN.
    #[arg(long, default_value_t = 150)]
    dsn_out_connections: u32,

    /// Defines max pending incoming connection limit for DSN.
    #[arg(long, default_value_t = 100)]
    dsn_pending_in_connections: u32,

    /// Defines max pending outgoing swarm connection limit for DSN.
    #[arg(long, default_value_t = 150)]
    dsn_pending_out_connections: u32,

    /// Determines whether we allow keeping non-global (private, shared, loopback..) addresses
    /// in Kademlia DHT for the DSN.
    #[arg(long, default_value_t = false)]
    dsn_enable_private_ips: bool,

    /// Defines whether we should run blocking Kademlia bootstrap() operation before other requests.
    #[arg(long, default_value_t = false)]
    dsn_disable_bootstrap_on_start: bool,

    /// Known external addresses
    #[arg(long, alias = "dsn-external-address")]
    dsn_external_addresses: Vec<Multiaddr>,
}

/// Options for timekeeper
#[derive(Debug, Parser)]
struct TimekeeperOptions {
    /// Assigned PoT role for this node.
    #[arg(long)]
    timekeeper: bool,

    /// CPU cores that timekeeper can use.
    ///
    /// At least 2 cores should be provided, if more cores than necessary are provided, random cores
    /// out of provided will be utilized, if not enough cores are provided timekeeper may occupy
    /// random CPU cores.
    ///
    /// Comma separated list of individual cores or ranges of cores.
    ///
    /// Examples:
    /// * `0,1` - use cores 0 and 1
    /// * `0-3` - use cores 0, 1, 2 and 3
    /// * `0,1,6-7` - use cores 0, 1, 6 and 7
    #[arg(long, default_value = "", value_parser = parse_timekeeper_cpu_cores, verbatim_doc_comment)]
    timekeeper_cpu_cores: HashSet<usize>,
}

/// Default run command for node
#[tokio::main]
pub async fn run(run_options: RunOptions) -> Result<(), Error> {
    let signals = Signals::capture()?;

    let RunOptions {
        mut run,
        pot_external_entropy,
        dsn_options,
        sync_from_dsn,
        storage_monitor,
        timekeeper_options,
        domain_args,
    } = run_options;
    let dev = run.shared_params.dev;

    // Force UTC logs for Subspace node
    run.shared_params.use_utc_log_time = true;
    let mut config = run.create_configuration(&SubspaceCliPlaceholder, Handle::current())?;
    run.init(
        &SubspaceCliPlaceholder::support_url(),
        &SubspaceCliPlaceholder::impl_version(),
        |_, _| {},
        &config,
    )?;

    set_default_ss58_version(&config.chain_spec);
    // Change default paths to Subspace structure
    {
        config.database = DatabaseSource::ParityDb {
            path: config.base_path.path().join("db"),
        };
        // Consensus node doesn't use keystore
        config.keystore = KeystoreConfig::InMemory;
        if let Some(net_config_path) = &mut config.network.net_config_path {
            *net_config_path = config.base_path.path().join("network");
            config.network.node_key = NodeKeyConfig::Ed25519(sc_network::config::Secret::File(
                net_config_path.join("secret_ed25519"),
            ));
        }
    }

    print_node_infos::<SubspaceCliPlaceholder>(&config);

    let mut task_manager = {
        let mut consensus_chain_config = config;

        // In case there are bootstrap nodes specified explicitly, ignore those that are in the
        // chain spec
        if !run.network_params.bootnodes.is_empty() {
            consensus_chain_config.network.boot_nodes = run.network_params.bootnodes;
        }

        let tokio_handle = consensus_chain_config.tokio_handle.clone();
        let base_path = consensus_chain_config.base_path.path().to_path_buf();
        let database_source = consensus_chain_config.database.clone();

        let domains_bootstrap_nodes: serde_json::map::Map<String, serde_json::Value> =
            consensus_chain_config
                .chain_spec
                .properties()
                .get("domainsBootstrapNodes")
                .map(|d| serde_json::from_value(d.clone()))
                .transpose()
                .map_err(|error| {
                    sc_service::Error::Other(format!(
                        "Failed to decode Domains bootstrap nodes: {error:?}"
                    ))
                })?
                .unwrap_or_default();

        let consensus_state_pruning_mode = consensus_chain_config
            .state_pruning
            .clone()
            .unwrap_or_default();
        let consensus_chain_node = {
            let span = sc_tracing::tracing::info_span!(
                sc_tracing::logging::PREFIX_LOG_SPAN,
                name = "Consensus"
            );
            let _enter = span.enter();

            let pot_external_entropy =
                derive_pot_external_entropy(&consensus_chain_config, pot_external_entropy)?;

            let dsn_config = {
                let network_keypair = consensus_chain_config
                    .network
                    .node_key
                    .clone()
                    .into_keypair()
                    .map_err(|error| {
                        sc_service::Error::Other(format!(
                            "Failed to convert network keypair: {error:?}"
                        ))
                    })?;

                let dsn_bootstrap_nodes = if dsn_options.dsn_bootstrap_nodes.is_empty() {
                    consensus_chain_config
                        .chain_spec
                        .properties()
                        .get("dsnBootstrapNodes")
                        .map(|d| serde_json::from_value(d.clone()))
                        .transpose()
                        .map_err(|error| {
                            sc_service::Error::Other(format!(
                                "Failed to decode DSN bootstrap nodes: {error:?}"
                            ))
                        })?
                        .unwrap_or_default()
                } else {
                    dsn_options.dsn_bootstrap_nodes
                };

                // TODO: Libp2p versions for Substrate and Subspace diverged.
                // We get type compatibility by encoding and decoding the original keypair.
                let encoded_keypair = network_keypair
                    .to_protobuf_encoding()
                    .expect("Keypair-to-protobuf encoding should succeed.");
                let keypair =
                    subspace_networking::libp2p::identity::Keypair::from_protobuf_encoding(
                        &encoded_keypair,
                    )
                    .expect("Keypair-from-protobuf decoding should succeed.");

                DsnConfig {
                    keypair,
                    base_path: base_path.clone(),
                    listen_on: dsn_options.dsn_listen_on,
                    bootstrap_nodes: dsn_bootstrap_nodes,
                    reserved_peers: dsn_options.dsn_reserved_peers,
                    // Override enabling private IPs with --dev
                    allow_non_global_addresses_in_dht: dsn_options.dsn_enable_private_ips || dev,
                    max_in_connections: dsn_options.dsn_in_connections,
                    max_out_connections: dsn_options.dsn_out_connections,
                    max_pending_in_connections: dsn_options.dsn_pending_in_connections,
                    max_pending_out_connections: dsn_options.dsn_pending_out_connections,
                    external_addresses: dsn_options.dsn_external_addresses,
                    // Override initial Kademlia bootstrapping  with --dev
                    disable_bootstrap_on_start: dsn_options.dsn_disable_bootstrap_on_start || dev,
                }
            };

            let consensus_chain_config = SubspaceConfiguration {
                base: consensus_chain_config,
                // Domain node needs slots notifications for bundle production.
                force_new_slot_notifications: !domain_args.is_empty(),
                subspace_networking: SubspaceNetworking::Create { config: dsn_config },
                sync_from_dsn,
                // Timekeeper is enabled if `--dev` is used
                is_timekeeper: timekeeper_options.timekeeper || dev,
                timekeeper_cpu_cores: timekeeper_options.timekeeper_cpu_cores,
            };

            let partial_components = subspace_service::new_partial::<
                PosTable,
                RuntimeApi,
                ExecutorDispatch,
            >(
                &consensus_chain_config.base, &pot_external_entropy
            )
            .map_err(|error| {
                sc_service::Error::Other(format!("Failed to build a full subspace node: {error:?}"))
            })?;

            subspace_service::new_full::<PosTable, _, _>(
                consensus_chain_config,
                partial_components,
                true,
                SlotProportion::new(3f32 / 4f32),
            )
            .await
            .map_err(|error| {
                sc_service::Error::Other(format!("Failed to build a full subspace node: {error:?}"))
            })?
        };

        StorageMonitorService::try_spawn(
            storage_monitor,
            database_source,
            &consensus_chain_node.task_manager.spawn_essential_handle(),
        )
        .map_err(|error| {
            sc_service::Error::Other(format!("Failed to start storage monitor: {error:?}"))
        })?;

        // Run a domain node.
        if !domain_args.is_empty() {
            let span = sc_tracing::tracing::info_span!(
                sc_tracing::logging::PREFIX_LOG_SPAN,
                name = "Domain"
            );
            let _enter = span.enter();

            let mut domain_cli = DomainCli::new(domain_args.into_iter());

            let domain_id = domain_cli.domain_id;

            if domain_cli.run.network_params.bootnodes.is_empty() {
                domain_cli.run.network_params.bootnodes = domains_bootstrap_nodes
                    .get(&format!("{}", domain_id))
                    .map(|d| serde_json::from_value(d.clone()))
                    .transpose()
                    .map_err(|error| {
                        sc_service::Error::Other(format!(
                            "Failed to decode Domain: {} bootstrap nodes: {error:?}",
                            domain_id
                        ))
                    })?
                    .unwrap_or_default();
            }

            // start relayer for consensus chain
            let mut xdm_gossip_worker_builder = GossipWorkerBuilder::new();
            {
                let span = sc_tracing::tracing::info_span!(
                    sc_tracing::logging::PREFIX_LOG_SPAN,
                    name = "Consensus"
                );
                let _enter = span.enter();

                let relayer_worker =
                    domain_client_message_relayer::worker::relay_consensus_chain_messages(
                        consensus_chain_node.client.clone(),
                        consensus_state_pruning_mode,
                        consensus_chain_node.sync_service.clone(),
                        xdm_gossip_worker_builder.gossip_msg_sink(),
                    );

                consensus_chain_node
                    .task_manager
                    .spawn_essential_handle()
                    .spawn_essential_blocking(
                        "consensus-chain-relayer",
                        None,
                        Box::pin(relayer_worker),
                    );

                let (consensus_msg_sink, consensus_msg_receiver) =
                    tracing_unbounded("consensus_message_channel", 100);

                // Start cross domain message listener for Consensus chain to receive messages from domains in the network
                let consensus_listener =
                    cross_domain_message_gossip::start_cross_chain_message_listener(
                        ChainId::Consensus,
                        consensus_chain_node.client.clone(),
                        consensus_chain_node.transaction_pool.clone(),
                        consensus_chain_node.network_service.clone(),
                        consensus_msg_receiver,
                    );

                consensus_chain_node
                    .task_manager
                    .spawn_essential_handle()
                    .spawn_essential_blocking(
                        "consensus-message-listener",
                        None,
                        Box::pin(consensus_listener),
                    );

                xdm_gossip_worker_builder
                    .push_chain_tx_pool_sink(ChainId::Consensus, consensus_msg_sink);
            }

            let bootstrapper =
                Bootstrapper::<DomainBlock, _, _>::new(consensus_chain_node.client.clone());

            let (domain_message_sink, domain_message_receiver) =
                tracing_unbounded("domain_message_channel", 100);

            xdm_gossip_worker_builder
                .push_chain_tx_pool_sink(ChainId::Domain(domain_id), domain_message_sink);

            let domain_starter = DomainInstanceStarter {
                domain_cli,
                base_path,
                tokio_handle,
                consensus_client: consensus_chain_node.client.clone(),
                consensus_offchain_tx_pool_factory: OffchainTransactionPoolFactory::new(
                    consensus_chain_node.transaction_pool.clone(),
                ),
                consensus_network: consensus_chain_node.network_service.clone(),
                block_importing_notification_stream: consensus_chain_node
                    .block_importing_notification_stream
                    .clone(),
                new_slot_notification_stream: consensus_chain_node
                    .new_slot_notification_stream
                    .clone(),
                consensus_sync_service: consensus_chain_node.sync_service.clone(),
                domain_message_receiver,
                gossip_message_sink: xdm_gossip_worker_builder.gossip_msg_sink(),
            };

            consensus_chain_node
                .task_manager
                .spawn_essential_handle()
                .spawn_essential_blocking(
                    "domain",
                    None,
                    Box::pin(async move {
                        let bootstrap_result =
                            match bootstrapper.fetch_domain_bootstrap_info(domain_id).await {
                                Err(err) => {
                                    log::error!("Domain bootstrapper exited with an error {err:?}");
                                    return;
                                }
                                Ok(res) => res,
                            };
                        if let Err(error) = domain_starter.start(bootstrap_result).await {
                            log::error!("Domain starter exited with an error {error:?}");
                        }
                    }),
                );

            let cross_domain_message_gossip_worker = xdm_gossip_worker_builder
                .build::<Block, _, _>(
                    consensus_chain_node.network_service.clone(),
                    consensus_chain_node.sync_service.clone(),
                );

            consensus_chain_node
                .task_manager
                .spawn_essential_handle()
                .spawn_essential_blocking(
                    "cross-domain-gossip-message-worker",
                    None,
                    Box::pin(cross_domain_message_gossip_worker.run()),
                );
        };

        consensus_chain_node.network_starter.start_network();

        consensus_chain_node.task_manager
    };

    signals
        .run_until_signal(task_manager.future().fuse())
        .await
        .map_err(Into::into)
}
