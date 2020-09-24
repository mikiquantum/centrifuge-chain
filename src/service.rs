// This file is part of Substrate.

// Copyright (C) 2018-2020 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

#![warn(unused_extern_crates)]

//! Service implementation. Specialized wrapper over substrate service.

use std::sync::Arc;
use sc_consensus_babe;
use grandpa::{self, FinalityProofProvider as GrandpaFinalityProofProvider};
use node_primitives::Block;
use node_runtime::RuntimeApi;
use sc_service::{
    config::{Role, Configuration}, error::{Error as ServiceError},
    RpcHandlers, TaskManager,
};
use sp_inherents::InherentDataProviders;
use sc_network::{Event, NetworkService};
use sc_executor::native_executor_instance;
use sp_runtime::traits::Block as BlockT;
use futures::prelude::*;
use sc_client_api::{ExecutorProvider, RemoteBackend};
use sp_core::traits::BareCryptoStorePtr;

// Our native executor instance.
native_executor_instance!(
	pub Executor,
	node_runtime::api::dispatch,
	node_runtime::native_version,
	crate::child::storage::HostFunctions,
);

type FullClient = sc_service::TFullClient<Block, RuntimeApi, Executor>;
type FullBackend = sc_service::TFullBackend<Block>;
type FullSelectChain = sc_consensus::LongestChain<FullBackend, Block>;
type FullGrandpaBlockImport =
grandpa::GrandpaBlockImport<FullBackend, Block, FullClient, FullSelectChain>;
type LightClient = sc_service::TLightClient<Block, RuntimeApi, Executor>;

pub fn new_partial(config: &Configuration) -> Result<sc_service::PartialComponents<
    FullClient, FullBackend, FullSelectChain,
    sp_consensus::DefaultImportQueue<Block, FullClient>,
    sc_transaction_pool::FullPool<Block, FullClient>,
    (
        impl Fn(
            crate::rpc::DenyUnsafe,
            jsonrpc_pubsub::manager::SubscriptionManager
        ) -> crate::rpc::IoHandler,
        (
            sc_consensus_babe::BabeBlockImport<Block, FullClient, FullGrandpaBlockImport>,
            grandpa::LinkHalf<Block, FullClient, FullSelectChain>,
            sc_consensus_babe::BabeLink<Block>,
        ),
        grandpa::SharedVoterState,
    )
>, ServiceError> {
    let (client, backend, keystore, task_manager) =
        sc_service::new_full_parts::<Block, RuntimeApi, Executor>(&config)?;
    let client = Arc::new(client);

    let select_chain = sc_consensus::LongestChain::new(backend.clone());

    let transaction_pool = sc_transaction_pool::BasicPool::new_full(
        config.transaction_pool.clone(),
        config.prometheus_registry(),
        task_manager.spawn_handle(),
        client.clone(),
    );

    let (grandpa_block_import, grandpa_link) = grandpa::block_import(
        client.clone(), &(client.clone() as Arc<_>), select_chain.clone(),
    )?;
    let justification_import = grandpa_block_import.clone();

    let (block_import, babe_link) = sc_consensus_babe::block_import(
        sc_consensus_babe::Config::get_or_compute(&*client)?,
        grandpa_block_import,
        client.clone(),
    )?;

    let inherent_data_providers = sp_inherents::InherentDataProviders::new();

    let import_queue = sc_consensus_babe::import_queue(
        babe_link.clone(),
        block_import.clone(),
        Some(Box::new(justification_import)),
        None,
        client.clone(),
        select_chain.clone(),
        inherent_data_providers.clone(),
        &task_manager.spawn_handle(),
        config.prometheus_registry(),
        sp_consensus::CanAuthorWithNativeVersion::new(client.executor().clone()),
    )?;

    let import_setup = (block_import, grandpa_link, babe_link);

    let (rpc_extensions_builder, rpc_setup) = {
        let (_, grandpa_link, babe_link) = &import_setup;

        let justification_stream = grandpa_link.justification_stream();
        let shared_authority_set = grandpa_link.shared_authority_set().clone();
        let shared_voter_state = grandpa::SharedVoterState::empty();

        let rpc_setup = shared_voter_state.clone();

        let babe_config = babe_link.config().clone();
        let shared_epoch_changes = babe_link.epoch_changes().clone();

        let client = client.clone();
        let pool = transaction_pool.clone();
        let select_chain = select_chain.clone();
        let keystore = keystore.clone();

        let rpc_extensions_builder = move |deny_unsafe, subscriptions| {
            let deps = crate::rpc::FullDeps {
                client: client.clone(),
                pool: pool.clone(),
                select_chain: select_chain.clone(),
                deny_unsafe,
                babe: crate::rpc::BabeDeps {
                    babe_config: babe_config.clone(),
                    shared_epoch_changes: shared_epoch_changes.clone(),
                    keystore: keystore.clone(),
                },
                grandpa: crate::rpc::GrandpaDeps {
                    shared_voter_state: shared_voter_state.clone(),
                    shared_authority_set: shared_authority_set.clone(),
                    justification_stream: justification_stream.clone(),
                    subscriptions,
                },
            };

            crate::rpc::create_full(deps)
        };

        (rpc_extensions_builder, rpc_setup)
    };

    Ok(sc_service::PartialComponents {
        client, backend, task_manager, keystore, select_chain, import_queue, transaction_pool,
        inherent_data_providers,
        other: (rpc_extensions_builder, import_setup, rpc_setup)
    })
}

/// Creates a full service from the configuration.
pub fn new_full_base(
    config: Configuration,
    with_startup_data: impl FnOnce(
        &sc_consensus_babe::BabeBlockImport<Block, FullClient, FullGrandpaBlockImport>,
        &sc_consensus_babe::BabeLink<Block>,
    )
) -> Result<(
    TaskManager, InherentDataProviders, Arc<FullClient>,
    Arc<NetworkService<Block, <Block as BlockT>::Hash>>,
    Arc<sc_transaction_pool::FullPool<Block, FullClient>>,
), ServiceError> {
    let sc_service::PartialComponents {
        client, backend, mut task_manager, import_queue, keystore, select_chain, transaction_pool,
        inherent_data_providers,
        other: (rpc_extensions_builder, import_setup, rpc_setup),
    } = new_partial(&config)?;

    let finality_proof_provider =
        GrandpaFinalityProofProvider::new_for_service(backend.clone(), client.clone());

    let (network, network_status_sinks, system_rpc_tx, network_starter) =
        sc_service::build_network(sc_service::BuildNetworkParams {
            config: &config,
            client: client.clone(),
            transaction_pool: transaction_pool.clone(),
            spawn_handle: task_manager.spawn_handle(),
            import_queue,
            on_demand: None,
            block_announce_validator_builder: None,
            finality_proof_request_builder: None,
            finality_proof_provider: Some(finality_proof_provider.clone()),
        })?;

    if config.offchain_worker.enabled {
        sc_service::build_offchain_workers(
            &config, backend.clone(), task_manager.spawn_handle(), client.clone(), network.clone(),
        );
    }

    let role = config.role.clone();
    let force_authoring = config.force_authoring;
    let name = config.network.node_name.clone();
    let enable_grandpa = !config.disable_grandpa;
    let prometheus_registry = config.prometheus_registry().cloned();
    let telemetry_connection_sinks = sc_service::TelemetryConnectionSinks::default();

    sc_service::spawn_tasks(sc_service::SpawnTasksParams {
        config,
        backend: backend.clone(),
        client: client.clone(),
        keystore: keystore.clone(),
        network: network.clone(),
        rpc_extensions_builder: Box::new(rpc_extensions_builder),
        transaction_pool: transaction_pool.clone(),
        task_manager: &mut task_manager,
        on_demand: None,
        remote_blockchain: None,
        telemetry_connection_sinks: telemetry_connection_sinks.clone(),
        network_status_sinks,
        system_rpc_tx,
    })?;

    let (block_import, grandpa_link, babe_link) = import_setup;
    let shared_voter_state = rpc_setup;

    (with_startup_data)(&block_import, &babe_link);

    if let sc_service::config::Role::Authority { .. } = &role {
        let proposer = sc_basic_authorship::ProposerFactory::new(
            client.clone(),
            transaction_pool.clone(),
            prometheus_registry.as_ref(),
        );

        let can_author_with =
            sp_consensus::CanAuthorWithNativeVersion::new(client.executor().clone());

        let babe_config = sc_consensus_babe::BabeParams {
            keystore: keystore.clone(),
            client: client.clone(),
            select_chain,
            env: proposer,
            block_import,
            sync_oracle: network.clone(),
            inherent_data_providers: inherent_data_providers.clone(),
            force_authoring,
            babe_link,
            can_author_with,
        };

        let babe = sc_consensus_babe::start_babe(babe_config)?;
        task_manager.spawn_essential_handle().spawn_blocking("babe-proposer", babe);
    }

    // Spawn authority discovery module.
    if matches!(role, Role::Authority{..} | Role::Sentry {..}) {
        let (sentries, authority_discovery_role) = match role {
            sc_service::config::Role::Authority { ref sentry_nodes } => (
                sentry_nodes.clone(),
                sc_authority_discovery::Role::Authority (
                    keystore.clone(),
                ),
            ),
            sc_service::config::Role::Sentry {..} => (
                vec![],
                sc_authority_discovery::Role::Sentry,
            ),
            _ => unreachable!("Due to outer matches! constraint; qed.")
        };

        let dht_event_stream = network.event_stream("authority-discovery")
            .filter_map(|e| async move { match e {
                Event::Dht(e) => Some(e),
                _ => None,
            }}).boxed();
        let (authority_discovery_worker, _service) = sc_authority_discovery::new_worker_and_service(
            client.clone(),
            network.clone(),
            sentries,
            dht_event_stream,
            authority_discovery_role,
            prometheus_registry.clone(),
        );

        task_manager.spawn_handle().spawn("authority-discovery-worker", authority_discovery_worker);
    }

    // if the node isn't actively participating in consensus then it doesn't
    // need a keystore, regardless of which protocol we use below.
    let keystore = if role.is_authority() {
        Some(keystore as BareCryptoStorePtr)
    } else {
        None
    };

    let config = grandpa::Config {
        // FIXME #1578 make this available through chainspec
        gossip_duration: std::time::Duration::from_millis(333),
        justification_period: 512,
        name: Some(name),
        observer_enabled: false,
        keystore,
        is_authority: role.is_network_authority(),
    };

    if enable_grandpa {
        // start the full GRANDPA voter
        // NOTE: non-authorities could run the GRANDPA observer protocol, but at
        // this point the full voter should provide better guarantees of block
        // and vote data availability than the observer. The observer has not
        // been tested extensively yet and having most nodes in a network run it
        // could lead to finality stalls.
        let grandpa_config = grandpa::GrandpaParams {
            config,
            link: grandpa_link,
            network: network.clone(),
            inherent_data_providers: inherent_data_providers.clone(),
            telemetry_on_connect: Some(telemetry_connection_sinks.on_connect_stream()),
            voting_rule: grandpa::VotingRulesBuilder::default().build(),
            prometheus_registry,
            shared_voter_state,
        };

        // the GRANDPA voter task is considered infallible, i.e.
        // if it fails we take down the service with it.
        task_manager.spawn_essential_handle().spawn_blocking(
            "grandpa-voter",
            grandpa::run_grandpa_voter(grandpa_config)?
        );
    } else {
        grandpa::setup_disabled_grandpa(
            client.clone(),
            &inherent_data_providers,
            network.clone(),
        )?;
    }

    network_starter.start_network();
    Ok((task_manager, inherent_data_providers, client, network, transaction_pool))
}

/// Builds a new service for a full client.
pub fn new_full(config: Configuration)
                -> Result<TaskManager, ServiceError> {
    new_full_base(config, |_, _| ()).map(|(task_manager, _, _, _, _)| {
        task_manager
    })
}

pub fn new_light_base(config: Configuration) -> Result<(
    TaskManager, RpcHandlers, Arc<LightClient>,
    Arc<NetworkService<Block, <Block as BlockT>::Hash>>,
    Arc<sc_transaction_pool::LightPool<Block, LightClient, sc_network::config::OnDemand<Block>>>
), ServiceError> {
    let (client, backend, keystore, mut task_manager, on_demand) =
        sc_service::new_light_parts::<Block, RuntimeApi, Executor>(&config)?;

    let select_chain = sc_consensus::LongestChain::new(backend.clone());

    let transaction_pool = Arc::new(sc_transaction_pool::BasicPool::new_light(
        config.transaction_pool.clone(),
        config.prometheus_registry(),
        task_manager.spawn_handle(),
        client.clone(),
        on_demand.clone(),
    ));

    let grandpa_block_import = grandpa::light_block_import(
        client.clone(), backend.clone(), &(client.clone() as Arc<_>),
        Arc::new(on_demand.checker().clone()),
    )?;

    let finality_proof_import = grandpa_block_import.clone();
    let finality_proof_request_builder =
        finality_proof_import.create_finality_proof_request_builder();

    let (babe_block_import, babe_link) = sc_consensus_babe::block_import(
        sc_consensus_babe::Config::get_or_compute(&*client)?,
        grandpa_block_import,
        client.clone(),
    )?;

    let inherent_data_providers = sp_inherents::InherentDataProviders::new();

    let import_queue = sc_consensus_babe::import_queue(
        babe_link,
        babe_block_import,
        None,
        Some(Box::new(finality_proof_import)),
        client.clone(),
        select_chain.clone(),
        inherent_data_providers.clone(),
        &task_manager.spawn_handle(),
        config.prometheus_registry(),
        sp_consensus::NeverCanAuthor,
    )?;

    let finality_proof_provider =
        GrandpaFinalityProofProvider::new_for_service(backend.clone(), client.clone());

    let (network, network_status_sinks, system_rpc_tx, network_starter) =
        sc_service::build_network(sc_service::BuildNetworkParams {
            config: &config,
            client: client.clone(),
            transaction_pool: transaction_pool.clone(),
            spawn_handle: task_manager.spawn_handle(),
            import_queue,
            on_demand: Some(on_demand.clone()),
            block_announce_validator_builder: None,
            finality_proof_request_builder: Some(finality_proof_request_builder),
            finality_proof_provider: Some(finality_proof_provider),
        })?;
    network_starter.start_network();

    if config.offchain_worker.enabled {
        sc_service::build_offchain_workers(
            &config, backend.clone(), task_manager.spawn_handle(), client.clone(), network.clone(),
        );
    }

    let light_deps = crate::rpc::LightDeps {
        remote_blockchain: backend.remote_blockchain(),
        fetcher: on_demand.clone(),
        client: client.clone(),
        pool: transaction_pool.clone(),
    };

    let rpc_extensions = crate::rpc::create_light(light_deps);

    let rpc_handlers =
        sc_service::spawn_tasks(sc_service::SpawnTasksParams {
            on_demand: Some(on_demand),
            remote_blockchain: Some(backend.remote_blockchain()),
            rpc_extensions_builder: Box::new(sc_service::NoopRpcExtensionBuilder(rpc_extensions)),
            client: client.clone(),
            transaction_pool: transaction_pool.clone(),
            config, keystore, backend, network_status_sinks, system_rpc_tx,
            network: network.clone(),
            telemetry_connection_sinks: sc_service::TelemetryConnectionSinks::default(),
            task_manager: &mut task_manager,
        })?;

    Ok((task_manager, rpc_handlers, client, network, transaction_pool))
}

/// Builds a new service for a light client.
pub fn new_light(config: Configuration) -> Result<TaskManager, ServiceError> {
    new_light_base(config).map(|(task_manager, _, _, _, _)| {
        task_manager
    })
}
