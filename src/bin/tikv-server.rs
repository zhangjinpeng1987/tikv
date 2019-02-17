// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

#![feature(slice_patterns)]
#![feature(proc_macro_hygiene)]

extern crate chrono;
extern crate clap;
extern crate fs2;
extern crate hyper;
extern crate libc;
#[cfg(unix)]
extern crate nix;
extern crate rocksdb;
extern crate serde_json;
#[cfg(unix)]
extern crate signal;
#[macro_use(
    kv,
    slog_kv,
    slog_error,
    slog_warn,
    slog_info,
    slog_log,
    slog_record,
    slog_b,
    slog_record_static
)]
extern crate slog;
extern crate slog_async;
#[macro_use]
extern crate slog_global;
extern crate slog_term;
extern crate tikv;
extern crate tikv_alloc;
extern crate toml;

#[cfg(unix)]
#[macro_use]
mod util;
use crate::util::setup::*;
use crate::util::signal_handler;

use std::fs::File;
use std::path::Path;
use std::process;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use std::usize;

use clap::{App, Arg};
use fs2::FileExt;

use tikv::config::{check_and_persist_critical_config, TiKvConfig};
use tikv::coprocessor;
use tikv::import::{ImportSSTService, SSTImporter};
use tikv::pd::{PdClient, RpcClient};
use tikv::raftstore::coprocessor::{CoprocessorHost, RegionInfoAccessor};
use tikv::raftstore::store::fsm::{self, SendCh};
use tikv::raftstore::store::{new_compaction_listener, Engines, SnapManagerBuilder};
use tikv::server::readpool::ReadPool;
use tikv::server::resolve;
use tikv::server::status_server::StatusServer;
use tikv::server::transport::{MockRaftStoreRouter, ServerRaftStoreRouter};
use tikv::server::{
    create_raft_storage, create_rocksdb_storage, Node, Server, SingleNode, DEFAULT_CLUSTER_ID,
};
use tikv::storage::engine::rocksdb_engine::RocksEngineRegionProvider;
use tikv::storage::{self, AutoGCConfig, DEFAULT_ROCKSDB_SUB_DIR};
use tikv::util::rocksdb_util::metrics_flusher::{MetricsFlusher, DEFAULT_FLUSHER_INTERVAL};
use tikv::util::security::SecurityManager;
use tikv::util::time::Monitor;
use tikv::util::worker::{Builder, FutureWorker};
use tikv::util::{self as tikv_util, check_environment_variables, rocksdb_util};

const RESERVED_OPEN_FDS: u64 = 1000;

fn check_system_config(config: &TiKvConfig) {
    if let Err(e) = tikv_util::config::check_max_open_fds(
        RESERVED_OPEN_FDS + (config.rocksdb.max_open_files + config.raftdb.max_open_files) as u64,
    ) {
        fatal!("{}", e);
    }

    for e in tikv_util::config::check_kernel() {
        warn!(
            "check-kernel";
            "err" => %e
        );
    }

    // check rocksdb data dir
    if let Err(e) = tikv_util::config::check_data_dir(&config.storage.data_dir) {
        warn!(
            "rocksdb check data dir";
            "err" => %e
        );
    }
    // check raft data dir
    if let Err(e) = tikv_util::config::check_data_dir(&config.raft_store.raftdb_path) {
        warn!(
            "raft check data dir";
            "err" => %e
        );
    }
}

fn run_standalone_server(
    pd_client: RpcClient,
    cfg: &TiKvConfig,
    security_mgr: Arc<SecurityManager>,
) {
    let store_path = Path::new(&cfg.storage.data_dir);
    let lock_path = store_path.join(Path::new("LOCK"));
    let db_path = store_path.join(Path::new(DEFAULT_ROCKSDB_SUB_DIR));
    let import_path = store_path.join("import");

    let f = File::create(lock_path.as_path())
        .unwrap_or_else(|e| fatal!("failed to create lock at {}: {}", lock_path.display(), e));
    if f.try_lock_exclusive().is_err() {
        fatal!(
            "lock {} failed, maybe another instance is using this directory.",
            store_path.display()
        );
    }

    // Create router.
    let raft_router = MockRaftStoreRouter::new();

    // Create pd client and pd worker
    let pd_client = Arc::new(pd_client);
    let pd_worker = FutureWorker::new("pd-worker");
    let (mut worker, resolver) = resolve::new_resolver(Arc::clone(&pd_client))
        .unwrap_or_else(|e| fatal!("failed to start address resolver: {}", e));
    let pd_sender = pd_worker.scheduler();

    // Create kv engine, storage.
    let mut kv_db_opts = cfg.rocksdb.build_opt();
    kv_db_opts.add_event_listener(compaction_listener);
    let kv_cfs_opts = cfg.rocksdb.build_cf_opts();
    let kv_engine = Arc::new(
        rocksdb_util::new_engine_opt(db_path.to_str().unwrap(), kv_db_opts, kv_cfs_opts)
            .unwrap_or_else(|s| fatal!("failed to create kv engine: {}", s)),
    );
    let storage_read_pool =
        ReadPool::new("store-read", &cfg.readpool.storage.build_config(), || {
            storage::ReadPoolContext::new(pd_sender.clone())
        });
    let storage = create_rocksdb_storage(&cfg.storage, storage_read_pool, Arc::clone(&kv_engine))
        .unwrap_or_else(|e| fatal!("failed to create rocksdb stroage: {}", e));;

    let server_cfg = Arc::new(cfg.server.clone());
    // Create server
    let cop_read_pool = ReadPool::new("cop", &cfg.readpool.coprocessor.build_config(), || {
        coprocessor::ReadPoolContext::new(pd_sender.clone())
    });
    let cop = coprocessor::Endpoint::new(&server_cfg, storage.get_engine(), cop_read_pool);

    // Create snapshot manager, server.
    let snap_mgr = SnapManagerBuilder::default()
        .max_write_bytes_per_sec(cfg.server.snap_max_write_bytes_per_sec.0)
        .max_total_size(cfg.server.snap_max_total_size.0)
        .build("./temp/snap", None);

    let mut server = Server::new(
        &server_cfg,
        &security_mgr,
        storage.clone(),
        cop,
        raft_router,
        resolver,
        snap_mgr,
        None,
        None,
    )
    .unwrap_or_else(|e| fatal!("failed to create server: {}", e));

    // Create node.
    let mut node = SingleNode::new(&server_cfg, &cfg.raft_store, pd_client.clone());

    node.start(Arc::clone(&kv_engine), pd_worker)
        .unwrap_or_else(|e| fatal!("failed to start node: {}", e));
    initial_metric(&cfg.metric, Some(node.id()));

    let region_info_accessor = RocksEngineRegionProvider::new();

    // Start auto gc
    let auto_gc_cfg = AutoGCConfig::new(pd_client, region_info_accessor, node.id());
    if let Err(e) = storage.start_auto_gc(auto_gc_cfg) {
        fatal!("failed to start auto_gc on storage, error: {}", e);
    }

    // Run server.
    server
        .start(server_cfg, security_mgr)
        .unwrap_or_else(|e| fatal!("failed to start server: {}", e));

    let server_cfg = cfg.server.clone();
    let mut status_enabled = cfg.metric.address.is_empty() && !server_cfg.status_addr.is_empty();

    // Create a status server.
    let mut status_server = StatusServer::new(server_cfg.status_thread_pool_size);
    if status_enabled {
        // Start the status server.
        if let Err(e) = status_server.start(server_cfg.status_addr) {
            error!(
                "failed to bind addr for status service";
            "err" => %e
            );
            status_enabled = false;
        }
    }

    // Stop.
    server
        .stop()
        .unwrap_or_else(|e| fatal!("failed to stop server: {}", e));

    if status_enabled {
        // Stop the status server.
        status_server.stop()
    }

    node.stop()
        .unwrap_or_else(|e| fatal!("failed to stop node: {}", e));

    if let Some(Err(e)) = worker.stop().map(|j| j.join()) {
        info!(
            "ignore failure when stopping resolver";
            "err" => ?e
        );
    }
}

fn run_raft_server(pd_client: RpcClient, cfg: &TiKvConfig, security_mgr: Arc<SecurityManager>) {
    let store_path = Path::new(&cfg.storage.data_dir);
    let lock_path = store_path.join(Path::new("LOCK"));
    let db_path = store_path.join(Path::new(DEFAULT_ROCKSDB_SUB_DIR));
    let snap_path = store_path.join(Path::new("snap"));
    let raft_db_path = Path::new(&cfg.raft_store.raftdb_path);
    let import_path = store_path.join("import");

    let f = File::create(lock_path.as_path())
        .unwrap_or_else(|e| fatal!("failed to create lock at {}: {}", lock_path.display(), e));
    if f.try_lock_exclusive().is_err() {
        fatal!(
            "lock {} failed, maybe another instance is using this directory.",
            store_path.display()
        );
    }

    if tikv_util::panic_mark_file_exists(&cfg.storage.data_dir) {
        fatal!(
            "panic_mark_file {} exists, there must be something wrong with the db.",
            tikv_util::panic_mark_file_path(&cfg.storage.data_dir).display()
        );
    }

    // Initialize raftstore channels.
    let (router, system) = fsm::create_raft_batch_system(&cfg.raft_store);
    let store_sendch = SendCh::new(router.clone(), "raftstore");

    // Create Local Reader.
    let local_reader = Builder::new("local-reader")
        .batch_size(cfg.raft_store.local_read_batch_size as usize)
        .create();
    let local_ch = local_reader.scheduler();

    // Create router.
    let raft_router = ServerRaftStoreRouter::new(store_sendch.clone(), router.clone(), local_ch);
    let compaction_listener = new_compaction_listener(router);

    // Create pd client and pd worker
    let pd_client = Arc::new(pd_client);
    let pd_worker = FutureWorker::new("pd-worker");
    let (mut worker, resolver) = resolve::new_resolver(Arc::clone(&pd_client))
        .unwrap_or_else(|e| fatal!("failed to start address resolver: {}", e));
    let pd_sender = pd_worker.scheduler();

    // Create kv engine, storage.
    let mut kv_db_opts = cfg.rocksdb.build_opt();
    kv_db_opts.add_event_listener(compaction_listener);
    let kv_cfs_opts = cfg.rocksdb.build_cf_opts();
    let kv_engine = Arc::new(
        rocksdb_util::new_engine_opt(db_path.to_str().unwrap(), kv_db_opts, kv_cfs_opts)
            .unwrap_or_else(|s| fatal!("failed to create kv engine: {}", s)),
    );
    let storage_read_pool =
        ReadPool::new("store-read", &cfg.readpool.storage.build_config(), || {
            storage::ReadPoolContext::new(pd_sender.clone())
        });
    let storage = create_raft_storage(
        raft_router.clone(),
        &cfg.storage,
        storage_read_pool,
        Some(Arc::clone(&kv_engine)),
        Some(raft_router.clone()),
    )
    .unwrap_or_else(|e| fatal!("failed to create raft stroage: {}", e));

    // Create raft engine.
    let raft_db_opts = cfg.raftdb.build_opt();
    let raft_db_cf_opts = cfg.raftdb.build_cf_opts();
    let raft_engine = Arc::new(
        rocksdb_util::new_engine_opt(
            raft_db_path.to_str().unwrap(),
            raft_db_opts,
            raft_db_cf_opts,
        )
        .unwrap_or_else(|s| fatal!("failed to create raft engine: {}", s)),
    );
    let engines = Engines::new(Arc::clone(&kv_engine), Arc::clone(&raft_engine));

    // Create snapshot manager, server.
    let snap_mgr = SnapManagerBuilder::default()
        .max_write_bytes_per_sec(cfg.server.snap_max_write_bytes_per_sec.0)
        .max_total_size(cfg.server.snap_max_total_size.0)
        .build(
            snap_path.as_path().to_str().unwrap().to_owned(),
            Some(store_sendch),
        );

    let importer = Arc::new(SSTImporter::new(import_path).unwrap());
    let import_service = ImportSSTService::new(
        cfg.import.clone(),
        raft_router.clone(),
        Arc::clone(&kv_engine),
        Arc::clone(&importer),
    );

    let server_cfg = Arc::new(cfg.server.clone());
    // Create server
    let cop_read_pool = ReadPool::new("cop", &cfg.readpool.coprocessor.build_config(), || {
        coprocessor::ReadPoolContext::new(pd_sender.clone())
    });
    let cop = coprocessor::Endpoint::new(&server_cfg, storage.get_engine(), cop_read_pool);
    let mut server = Server::new(
        &server_cfg,
        &security_mgr,
        storage.clone(),
        cop,
        raft_router,
        resolver,
        snap_mgr.clone(),
        Some(engines.clone()),
        Some(import_service),
    )
    .unwrap_or_else(|e| fatal!("failed to create server: {}", e));
    let trans = server.transport();

    // Create node.
    let mut node = Node::new(system, &server_cfg, &cfg.raft_store, pd_client.clone());

    // Create CoprocessorHost.
    let mut coprocessor_host = CoprocessorHost::new(cfg.coprocessor.clone(), node.get_sendch());

    // Create region collection.
    let region_info_accessor = RegionInfoAccessor::new(&mut coprocessor_host);
    region_info_accessor.start();

    node.start(
        engines.clone(),
        trans,
        snap_mgr,
        pd_worker,
        local_reader,
        coprocessor_host,
        importer,
    )
    .unwrap_or_else(|e| fatal!("failed to start node: {}", e));
    initial_metric(&cfg.metric, Some(node.id()));

    // Start auto gc
    let auto_gc_cfg = AutoGCConfig::new(pd_client, region_info_accessor.clone(), node.id());
    if let Err(e) = storage.start_auto_gc(auto_gc_cfg) {
        fatal!("failed to start auto_gc on storage, error: {}", e);
    }

    let mut metrics_flusher = MetricsFlusher::new(
        engines.clone(),
        Duration::from_millis(DEFAULT_FLUSHER_INTERVAL),
    );

    // Start metrics flusher
    if let Err(e) = metrics_flusher.start() {
        error!(
            "failed to start metrics flusher";
            "err" => %e
        );
    }

    // Run server.
    server
        .start(server_cfg, security_mgr)
        .unwrap_or_else(|e| fatal!("failed to start server: {}", e));

    let server_cfg = cfg.server.clone();
    let mut status_enabled = cfg.metric.address.is_empty() && !server_cfg.status_addr.is_empty();

    // Create a status server.
    let mut status_server = StatusServer::new(server_cfg.status_thread_pool_size);
    if status_enabled {
        // Start the status server.
        if let Err(e) = status_server.start(server_cfg.status_addr) {
            error!(
                "failed to bind addr for status service";
                "err" => %e
            );
            status_enabled = false;
        }
    }

    signal_handler::handle_signal(Some(engines));

    // Stop.
    server
        .stop()
        .unwrap_or_else(|e| fatal!("failed to stop server: {}", e));

    if status_enabled {
        // Stop the status server.
        status_server.stop()
    }

    metrics_flusher.stop();

    node.stop()
        .unwrap_or_else(|e| fatal!("failed to stop node: {}", e));

    region_info_accessor.stop();

    if let Some(Err(e)) = worker.stop().map(|j| j.join()) {
        info!(
            "ignore failure when stopping resolver";
            "err" => ?e
        );
    }
}

fn main() {
    let matches = App::new("TiKV")
        .long_version(util::tikv_version_info().as_ref())
        .author("TiKV Org.")
        .about("A Distributed transactional key-value database powered by Rust and Raft")
        .arg(
            Arg::with_name("config")
                .short("C")
                .long("config")
                .value_name("FILE")
                .help("Sets config file")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("addr")
                .short("A")
                .long("addr")
                .takes_value(true)
                .value_name("IP:PORT")
                .help("Sets listening address"),
        )
        .arg(
            Arg::with_name("advertise-addr")
                .long("advertise-addr")
                .takes_value(true)
                .value_name("IP:PORT")
                .help("Sets advertise listening address for client communication"),
        )
        .arg(
            Arg::with_name("status-addr")
                .long("status-addr")
                .takes_value(true)
                .value_name("IP:PORT")
                .help("Sets HTTP listening address for the status report service"),
        )
        .arg(
            Arg::with_name("log-level")
                .short("L")
                .long("log-level")
                .alias("log")
                .takes_value(true)
                .value_name("LEVEL")
                .possible_values(&[
                    "trace", "debug", "info", "warn", "warning", "error", "critical",
                ])
                .help("Sets log level"),
        )
        .arg(
            Arg::with_name("log-file")
                .short("f")
                .long("log-file")
                .takes_value(true)
                .value_name("FILE")
                .help("Sets log file")
                .long_help("Sets log file. If not set, output log to stderr"),
        )
        .arg(
            Arg::with_name("data-dir")
                .long("data-dir")
                .short("s")
                .alias("store")
                .takes_value(true)
                .value_name("PATH")
                .help("Sets the path to store directory"),
        )
        .arg(
            Arg::with_name("capacity")
                .long("capacity")
                .takes_value(true)
                .value_name("CAPACITY")
                .help("Sets the store capacity")
                .long_help("Sets the store capacity. If not set, use entire partition"),
        )
        .arg(
            Arg::with_name("pd-endpoints")
                .long("pd-endpoints")
                .aliases(&["pd", "pd-endpoint"])
                .takes_value(true)
                .value_name("PD_URL")
                .multiple(true)
                .use_delimiter(true)
                .require_delimiter(true)
                .value_delimiter(",")
                .help("Sets PD endpoints")
                .long_help("Sets PD endpoints. Uses `,` to separate multiple PDs"),
        )
        .arg(
            Arg::with_name("labels")
                .long("labels")
                .alias("label")
                .takes_value(true)
                .value_name("KEY=VALUE")
                .multiple(true)
                .use_delimiter(true)
                .require_delimiter(true)
                .value_delimiter(",")
                .help("Sets server labels")
                .long_help(
                    "Sets server labels. Uses `,` to separate kv pairs, like \
                     `zone=cn,disk=ssd`",
                ),
        )
        .arg(
            Arg::with_name("print-sample-config")
                .long("print-sample-config")
                .help("Print a sample config to stdout"),
        )
        .get_matches();

    if matches.is_present("print-sample-config") {
        let config = TiKvConfig::default();
        println!("{}", toml::to_string_pretty(&config).unwrap());
        process::exit(0);
    }

    let mut config = matches
        .value_of("config")
        .map_or_else(TiKvConfig::default, |path| TiKvConfig::from_file(&path));

    overwrite_config_with_cmd_args(&mut config, &matches);

    if let Err(e) = check_and_persist_critical_config(&config) {
        fatal!("critical config check failed: {}", e);
    }

    // Sets the global logger ASAP.
    // It is okay to use the config w/o `validata()`,
    // because `initial_logger()` handles various conditions.
    initial_logger(&config);
    tikv_util::set_panic_hook(false, &config.storage.data_dir);

    // Print version information.
    util::log_tikv_info();

    config.compatible_adjust();
    if let Err(e) = config.validate() {
        fatal!("invalid configuration: {}", e.description());
    }
    info!(
        "using config";
        "config" => serde_json::to_string(&config).unwrap(),
    );

    // Before any startup, check system configuration.
    check_system_config(&config);

    check_environment_variables();

    let security_mgr = Arc::new(
        SecurityManager::new(&config.security)
            .unwrap_or_else(|e| fatal!("failed to create security manager: {}", e.description())),
    );
    let pd_client = RpcClient::new(&config.pd, Arc::clone(&security_mgr))
        .unwrap_or_else(|e| fatal!("failed to create rpc client: {}", e));
    let cluster_id = pd_client
        .get_cluster_id()
        .unwrap_or_else(|e| fatal!("failed to get cluster id: {}", e));
    if cluster_id == DEFAULT_CLUSTER_ID {
        fatal!("cluster id can't be {}", DEFAULT_CLUSTER_ID);
    }
    config.server.cluster_id = cluster_id;
    info!(
        "connect to PD cluster";
        "cluster_id" => cluster_id
    );

    let _m = Monitor::default();
    run_raft_server(pd_client, &config, security_mgr);
}
