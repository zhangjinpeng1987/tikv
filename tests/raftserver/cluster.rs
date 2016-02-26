#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

use rocksdb::DB;
use tempdir::TempDir;

use tikv::raftserver::Result;
use tikv::raftserver::store::*;
use super::util::*;
use tikv::proto::raft_cmdpb::*;
use tikv::proto::metapb;
use tikv::proto::raftpb::ConfChangeType;

// We simulate 3 or 5 nodes, each has a store, the node id and store id are same.
// E,g, for node 1, the node id and store id are both 1.

pub trait ClusterSimulator {
    fn run_store(&mut self, store_id: u64, engine: Arc<DB>);
    fn stop_store(&mut self, store_id: u64);
    fn get_store_ids(&self) -> Vec<u64>;
    fn call_command(&self,
                    request: RaftCommandRequest,
                    timeout: Duration)
                    -> Option<RaftCommandResponse>;
}

pub struct Cluster<T: ClusterSimulator> {
    id: u64,
    leaders: HashMap<u64, metapb::Peer>,
    paths: HashMap<u64, TempDir>,
    pub engines: HashMap<u64, Arc<DB>>,

    sim: T,
}

impl<T: ClusterSimulator> Cluster<T> {
    // Create the default Store cluster.
    pub fn new(id: u64, count: usize, sim: T) -> Cluster<T> {
        let mut c = Cluster {
            id: id,
            leaders: HashMap::new(),
            paths: HashMap::new(),
            engines: HashMap::new(),
            sim: sim,
        };

        c.create_engines(count);

        c
    }

    fn create_engines(&mut self, count: usize) {
        for i in 0..count {
            self.paths.insert(i as u64 + 1, TempDir::new("test_cluster").unwrap());
        }

        for (i, item) in &self.paths {
            self.engines.insert(*i, new_engine(item));
        }
    }

    pub fn run_store(&mut self, store_id: u64) {
        let engine = self.engines.get(&store_id).unwrap();
        self.sim.run_store(store_id, engine.clone());
    }

    pub fn run_all_stores(&mut self) {
        let count = self.engines.len();
        for i in 0..count {
            self.run_store(i as u64 + 1);
        }
    }

    pub fn stop_store(&mut self, store_id: u64) {
        self.sim.stop_store(store_id);
    }

    pub fn get_engines(&self) -> &HashMap<u64, Arc<DB>> {
        &self.engines
    }

    pub fn get_engine(&self, store_id: u64) -> Arc<DB> {
        self.engines.get(&store_id).unwrap().clone()
    }

    pub fn call_command(&self,
                        request: RaftCommandRequest,
                        timeout: Duration)
                        -> Option<RaftCommandResponse> {
        self.sim.call_command(request, timeout)
    }

    pub fn call_command_on_leader(&mut self,
                                  region_id: u64,
                                  mut request: RaftCommandRequest,
                                  timeout: Duration)
                                  -> Option<RaftCommandResponse> {
        request.mut_header().set_peer(self.leader_of_region(region_id).clone().unwrap());
        self.call_command(request, timeout)
    }

    pub fn leader_of_region(&mut self, region_id: u64) -> Option<metapb::Peer> {
        if let Some(l) = self.leaders.get(&region_id) {
            return Some(l.clone());
        }
        let mut leader = None;
        for id in self.sim.get_store_ids() {
            let peer = new_peer(id, id, id);
            let find_leader = new_status_request(region_id, &peer, new_region_leader_cmd());
            let resp = self.call_command(find_leader, Duration::from_secs(3)).unwrap();
            let region_leader = resp.get_status_response().get_region_leader();
            if region_leader.has_leader() {
                leader = Some(region_leader.get_leader().clone());
                break;
            }
            sleep_ms(100);
        }

        if let Some(l) = leader {
            self.leaders.insert(region_id, l);
        }
        self.leaders.get(&region_id).cloned()
    }

    pub fn bootstrap_single_region(&self) -> Result<()> {
        let mut region = metapb::Region::new();
        region.set_region_id(1);
        region.set_start_key(keys::MIN_KEY.to_vec());
        region.set_end_key(keys::MAX_KEY.to_vec());

        for (&id, engine) in &self.engines {
            let peer = new_peer(id, id, id);
            region.mut_peers().push(peer.clone());
            bootstrap_store(engine.clone(), self.id, id, id).unwrap();
        }

        for engine in self.engines.values() {
            try!(write_first_region(&engine, &region));
        }
        Ok(())
    }

    // 5 store, and store 1 bootstraps first region.
    pub fn bootstrap_conf_change(&self) {
        for (&id, engine) in &self.engines {
            bootstrap_store(engine.clone(), self.id, id, id).unwrap();
        }

        let store_id = 1;
        bootstrap_region(self.engines.get(&store_id).unwrap().clone()).unwrap();
    }

    pub fn reset_leader_of_region(&mut self, region_id: u64) {
        self.leaders.remove(&region_id);
    }

    pub fn check_quorum<F: FnMut(&&Arc<DB>) -> bool>(&self, condition: F) -> bool {
        if self.engines.is_empty() {
            return true;
        }
        self.engines.values().filter(condition).count() > self.engines.len() / 2
    }

    pub fn shutdown(&mut self) {
        let keys: Vec<u64> = self.sim.get_store_ids();
        for id in keys {
            self.stop_store(id);
        }
        self.leaders.clear();
    }

    // If the resp is "not leader error", get the real leader.
    // Sometimes, we may still can't get leader even in "not leader error",
    // returns a INVALID_PEER for this.
    pub fn refresh_leader_if_needed(&mut self, resp: &RaftCommandResponse, region_id: u64) -> bool {
        if !is_error_response(resp) {
            return false;
        }

        let err = resp.get_header().get_error().get_detail();
        if !err.has_not_leader() {
            return false;
        }

        let err = err.get_not_leader();
        if !err.has_leader() {
            return false;
        }
        self.leaders.insert(region_id, err.get_leader().clone());
        true
    }

    pub fn request(&mut self,
                   region_id: u64,
                   request: RaftCommandRequest,
                   timeout: Duration)
                   -> RaftCommandResponse {
        loop {
            let resp = self.call_command_on_leader(region_id, request.clone(), timeout).unwrap();
            if !resp.get_header().has_error() || !self.refresh_leader_if_needed(&resp, region_id) {
                return resp;
            }
            error!("refreshed leader of region {}", region_id);
        }
    }

    pub fn get(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let get = new_request(1, vec![new_get_cmd(&keys::data_key(key))]);
        let mut resp = self.request(1, get, Duration::from_secs(3));
        if resp.get_header().has_error() {
            panic!("response {:?} has error", resp);
        }
        assert_eq!(resp.get_responses().len(), 1);
        assert_eq!(resp.get_responses()[0].get_cmd_type(), CommandType::Get);
        let mut get = resp.mut_responses()[0].take_get();
        if get.has_value() {
            Some(get.take_value())
        } else {
            None
        }
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) {
        let put = new_request(1, vec![new_put_cmd(&keys::data_key(key), value)]);
        let resp = self.request(1, put, Duration::from_secs(3));
        if resp.get_header().has_error() {
            panic!("response {:?} has error", resp);
        }
        assert_eq!(resp.get_responses().len(), 1);
        assert_eq!(resp.get_responses()[0].get_cmd_type(), CommandType::Put);
    }

    pub fn seek(&mut self, key: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
        let seek = new_request(1, vec![new_seek_cmd(&keys::data_key(key))]);
        let resp = self.request(1, seek, Duration::from_secs(3));
        if resp.get_header().has_error() {
            panic!("response {:?} has error", resp);
        }
        assert_eq!(resp.get_responses().len(), 1);
        let resp = &resp.get_responses()[0];
        assert_eq!(resp.get_cmd_type(), CommandType::Seek);
        if !resp.has_seek() {
            None
        } else {
            Some((resp.get_seek().get_key().to_vec(),
                  resp.get_seek().get_value().to_vec()))
        }
    }

    pub fn delete(&mut self, key: &[u8]) {
        let delete = new_request(1, vec![new_delete_cmd(&keys::data_key(key))]);
        let resp = self.request(1, delete, Duration::from_secs(3));
        if resp.get_header().has_error() {
            panic!("response {:?} has error", resp);
        }
        assert_eq!(resp.get_responses().len(), 1);
        assert_eq!(resp.get_responses()[0].get_cmd_type(), CommandType::Delete);
    }

    pub fn change_peer(&mut self,
                       region_id: u64,
                       change_type: ConfChangeType,
                       peer: metapb::Peer) {
        let change_peer = new_admin_request(region_id, new_change_peer_cmd(change_type, peer));
        let resp = self.call_command_on_leader(region_id, change_peer, Duration::from_secs(3))
                       .unwrap();
        assert_eq!(resp.get_admin_response().get_cmd_type(),
                   AdminCommandType::ChangePeer);
    }
}

impl<T: ClusterSimulator> Drop for Cluster<T> {
    fn drop(&mut self) {
        self.shutdown();
    }
}


pub struct StoreCluster {
    senders: HashMap<u64, SendCh>,
    handles: HashMap<u64, thread::JoinHandle<()>>,

    trans: Arc<RwLock<StoreTransport>>,
}

impl StoreCluster {
    pub fn new() -> StoreCluster {
        StoreCluster {
            senders: HashMap::new(),
            handles: HashMap::new(),
            trans: StoreTransport::new(),
        }
    }
}

impl ClusterSimulator for StoreCluster {
    fn run_store(&mut self, store_id: u64, engine: Arc<DB>) {
        assert!(!self.handles.contains_key(&store_id));
        assert!(!self.senders.contains_key(&store_id));

        let cfg = new_store_cfg();

        let mut event_loop = create_event_loop(&cfg).unwrap();

        let mut store = Store::new(&mut event_loop, cfg, engine.clone(), self.trans.clone())
                            .unwrap();

        self.trans.write().unwrap().add_sender(store.get_store_id(), store.get_sendch());

        let sender = store.get_sendch();
        let t = thread::spawn(move || {
            store.run(&mut event_loop).unwrap();
        });

        self.handles.insert(store_id, t);
        self.senders.insert(store_id, sender);
    }

    fn stop_store(&mut self, store_id: u64) {
        let h = self.handles.remove(&store_id).unwrap();
        let sender = self.senders.remove(&store_id).unwrap();

        self.trans.write().unwrap().remove_sender(store_id);

        sender.send_quit().unwrap();
        h.join().unwrap();
    }

    fn get_store_ids(&self) -> Vec<u64> {
        self.senders.keys().cloned().collect()
    }

    fn call_command(&self,
                    request: RaftCommandRequest,
                    timeout: Duration)
                    -> Option<RaftCommandResponse> {
        let store_id = request.get_header().get_peer().get_store_id();
        let sender = self.senders.get(&store_id).unwrap();

        call_command(sender, request, timeout).unwrap()
    }
}

pub fn new_store_cluster(id: u64, count: usize) -> Cluster<StoreCluster> {
    Cluster::new(id, count, StoreCluster::new())
}
