#![allow(dead_code)]
use std::cmp;
use raft::storage::Storage;
use util::DefaultRng;
use rand::Rng;
use proto::raftpb::{HardState, Entry, EntryType, Message, Snapshot, MessageType};
use protobuf::repeated::RepeatedField;
use raft::progress::{Progress, Inflights, ProgressState};
use raft::errors::{Result, Error, StorageError};
use std::collections::HashMap;
use raft::raft_log::{self, RaftLog};
use std::sync::Arc;


#[derive(Debug, PartialEq, Clone, Copy)]
pub enum StateRole {
    Follower,
    Candidate,
    Leader,
}

impl Default for StateRole {
    fn default() -> StateRole {
        StateRole::Follower
    }
}

pub const INVALID_ID: u64 = 0;

/// Config contains the parameters to start a raft.
#[derive(Default)]
pub struct Config<T: Storage + Default> {
    /// id is the identity of the local raft. ID cannot be 0.
    pub id: u64,

    /// peers contains the IDs of all nodes (including self) in
    /// the raft cluster. It should only be set when starting a new
    /// raft cluster.
    /// Restarting raft from previous configuration will panic if
    /// peers is set.
    /// peer is private and only used for testing right now.
    pub peers: Vec<u64>,

    /// ElectionTick is the election timeout. If a follower does not
    /// receive any message from the leader of current term during
    /// ElectionTick, it will become candidate and start an election.
    /// ElectionTick must be greater than HeartbeatTick. We suggest
    /// to use ElectionTick = 10 * HeartbeatTick to avoid unnecessary
    /// leader switching.
    pub election_tick: usize,
    /// HeartbeatTick is the heartbeat usizeerval. A leader sends heartbeat
    /// message to mausizeain the leadership every heartbeat usizeerval.
    pub heartbeat_tick: usize,

    /// Storage is the storage for raft. raft generates entires and
    /// states to be stored in storage. raft reads the persisted entires
    /// and states out of Storage when it needs. raft reads out the previous
    /// state and configuration out of storage when restarting.
    pub storage: Arc<T>,
    /// Applied is the last applied index. It should only be set when restarting
    /// raft. raft will not return entries to the application smaller or equal to Applied.
    /// If Applied is unset when restarting, raft might return previous applied entries.
    /// This is a very application dependent configuration.
    pub applied: u64,

    /// MaxSizePerMsg limits the max size of each append message. Smaller value lowers
    /// the raft recovery cost(initial probing and message lost during normal operation).
    /// On the other side, it might affect the throughput during normal replication.
    /// Note: math.MaxUusize64 for unlimited, 0 for at most one entry per message.
    pub max_size_per_msg: u64,
    /// max_inflight_msgs limits the max number of in-flight append messages during optimistic
    /// replication phase. The application transportation layer usually has its own sending
    /// buffer over TCP/UDP. Setting MaxInflightMsgs to avoid overflowing that sending buffer.
    /// TODO (xiangli): feedback to application to limit the proposal rate?
    pub max_inflight_msgs: usize,

    /// check_quorum specifies if the leader should check quorum activity. Leader steps down when
    /// quorum is not active for an electionTimeout.
    pub check_quorum: bool,
}

impl<T: Storage + Default> Config<T> {
    pub fn validate(&self) -> Result<()> {
        if self.id == INVALID_ID {
            return Err(Error::ConfigInvalid("invalid node id".to_string()));
        }

        if self.heartbeat_tick <= 0 {
            return Err(Error::ConfigInvalid("heartbeat tick must greater than 0".to_string()));
        }

        if self.election_tick <= self.heartbeat_tick {
            return Err(Error::ConfigInvalid("election tick must be greater than heartbeat tick"
                                                .to_string()));
        }

        if self.max_inflight_msgs <= 0 {
            return Err(Error::ConfigInvalid("max inflight messages must be greater than 0"
                                                .to_string()));
        }

        Ok(())
    }
}

// SoftState provides state that is useful for logging and debugging.
// The state is volatile and does not need to be persisted to the WAL.
#[derive(Default, PartialEq)]
pub struct SoftState {
    pub lead: u64,
    pub raft_state: StateRole,
}

#[derive(Default)]
pub struct Raft<T: Default + Storage> {
    pub hs: HardState,

    pub id: u64,

    /// the log
    pub raft_log: RaftLog<T>,

    pub max_inflight: usize,
    pub max_msg_size: u64,
    pub prs: HashMap<u64, Progress>,

    pub state: StateRole,

    pub votes: HashMap<u64, bool>,

    pub msgs: Vec<Message>,

    /// the leader id
    pub lead: u64,

    /// New configuration is ignored if there exists unapplied configuration.
    pending_conf: bool,

    /// number of ticks since it reached last electionTimeout when it is leader
    /// or candidate.
    /// number of ticks since it reached last electionTimeout or received a
    /// valid message from current leader when it is a follower.
    election_elapsed: usize,

    /// number of ticks since it reached last heartbeatTimeout.
    /// only leader keeps heartbeatElapsed.
    heartbeat_elapsed: usize,

    check_quorum: bool,

    heartbeat_timeout: usize,
    election_timeout: usize,
    /// Will be called when step** is about to be called.
    /// return false will skip step**.
    skip_step: Option<Box<FnMut() -> bool>>,
    rng: DefaultRng,
}

fn new_progress(next_idx: u64, ins_size: usize) -> Progress {
    Progress {
        next_idx: next_idx,
        ins: Inflights::new(ins_size),
        ..Default::default()
    }
}

fn new_message(to: u64, field_type: MessageType, from: Option<u64>) -> Message {
    let mut m = Message::new();
    m.set_to(to);
    if let Some(id) = from {
        m.set_from(id);
    }
    m.set_msg_type(field_type);
    m
}

impl<T: Storage + Default> Raft<T> {
    pub fn new(c: &Config<T>) -> Raft<T> {
        c.validate().expect("configuration is invalid");
        let store = c.storage.clone();
        let rs = store.initial_state().expect("");
        let raft_log = RaftLog::new(store);
        let mut peers: &[u64] = &c.peers;
        if rs.conf_state.get_nodes().len() > 0 {
            if peers.len() > 0 {
                // TODO(bdarnell): the peers argument is always nil except in
                // tests; the argument should be removed and these tests should be
                // updated to specify their nodes through a snap
                panic!("cannot specify both new(peers) and ConfState.Nodes")
            }
            peers = rs.conf_state.get_nodes();
        }
        let mut r = Raft {
            id: c.id,
            raft_log: raft_log,
            max_inflight: c.max_inflight_msgs,
            max_msg_size: c.max_size_per_msg,
            prs: HashMap::with_capacity(peers.len()),
            state: StateRole::Follower,
            check_quorum: c.check_quorum,
            heartbeat_timeout: c.heartbeat_tick,
            election_timeout: c.election_tick,
            ..Default::default()
        };
        for p in peers {
            r.prs.insert(*p, new_progress(1, r.max_inflight));
        }
        if rs.hard_state != HardState::new() {
            r.load_state(rs.hard_state);
        }
        if c.applied > 0 {
            r.raft_log.applied_to(c.applied);
        }
        let term = r.get_term();
        r.become_follower(term, INVALID_ID);
        let nodes_str = r.nodes().iter().fold(String::new(), |b, n| b + &format!("{}", n));
        info!("newRaft {:x} [peers: [{}], term: {:?}, commit: {}, applied: {}, last_index: {}, \
               last_term: {}]",
              r.id,
              nodes_str,
              r.get_term(),
              r.raft_log.committed,
              r.raft_log.get_applied(),
              r.raft_log.last_index(),
              r.raft_log.last_term());
        r
    }

    pub fn get_store(&self) -> Arc<T> {
        self.raft_log.get_store()
    }

    fn has_leader(&self) -> bool {
        self.lead != INVALID_ID
    }

    pub fn soft_state(&self) -> SoftState {
        SoftState {
            lead: self.lead,
            raft_state: self.state,
        }
    }

    pub fn hard_state(&self) -> HardState {
        self.hs.clone()
    }

    fn quorum(&self) -> usize {
        self.prs.len() / 2 + 1
    }

    pub fn nodes(&self) -> Vec<u64> {
        let mut nodes = Vec::with_capacity(self.prs.len());
        nodes.extend(self.prs.keys());
        nodes.sort();
        nodes
    }

    // send persists state to stable storage and then sends to its mailbox.
    fn send(&mut self, m: Message) {
        let mut m = m;
        m.set_from(self.id);
        // do not attach term to MsgPropose
        // proposals are a way to forward to the leader and
        // should be treated as local message.
        if m.get_msg_type() != MessageType::MsgPropose {
            m.set_term(self.get_term());
        }
        self.msgs.push(m);
    }

    fn prepare_send_snapshot(&mut self, m: &mut Message, to: u64) {
        let pr = self.prs.get_mut(&to).unwrap();
        if !pr.recent_active {
            debug!("ignore sending snapshot to {:x} since it is not recently active",
                   to);
            return;
        }

        m.set_msg_type(MessageType::MsgSnapshot);
        let snapshot_r = self.raft_log.snapshot();
        if let Err(e) = snapshot_r {
            if e == Error::Store(StorageError::SnapshotTemporarilyUnavailable) {
                debug!("{:x} failed to send snapshot to {:x} because snapshot is termporarily \
                        unavailable",
                       self.id,
                       to);
                return;
            }
            panic!(e);
        }
        let snapshot = snapshot_r.unwrap();
        if snapshot.get_metadata().get_index() == 0 {
            panic!("need non-empty snapshot");
        }
        let (sindex, sterm) = (snapshot.get_metadata().get_index(),
                               snapshot.get_metadata().get_term());
        m.set_snapshot(snapshot);
        debug!("{:x} [firstindex: {}, commit: {}] sent snapshot[index: {}, term: {}] to {:x} \
                [{:?}]",
               self.id,
               self.raft_log.first_index(),
               self.hs.get_commit(),
               sindex,
               sterm,
               to,
               pr);
        pr.become_snapshot(sindex);
        debug!("{:x} paused sending replication messages to {:x} [{:?}]",
               self.id,
               to,
               pr);
    }

    fn prepare_send_entries(&mut self, m: &mut Message, to: u64, term: u64, ents: Vec<Entry>) {
        let pr = self.prs.get_mut(&to).unwrap();
        m.set_msg_type(MessageType::MsgAppend);
        m.set_index(pr.next_idx - 1);
        m.set_log_term(term);
        m.set_entries(RepeatedField::from_vec(ents));
        m.set_commit(self.raft_log.committed);
        if m.get_entries().len() != 0 {
            match pr.state {
                ProgressState::Replicate => {
                    let last = m.get_entries().last().unwrap().get_index();
                    pr.optimistic_update(last);
                    pr.ins.add(last);
                }
                ProgressState::Probe => pr.pause(),
                _ => {
                    panic!("{:x} is sending append in unhandled state {:?}",
                           self.id,
                           pr.state)
                }
            }
        }
    }

    // send_append sends RPC, with entries to the given peer.
    fn send_append(&mut self, to: u64) {
        let (term, ents) = {
            let pr = self.prs.get(&to).unwrap();
            if pr.is_paused() {
                return;
            }
            (self.raft_log.term(pr.next_idx - 1),
             self.raft_log.entries(pr.next_idx, self.max_msg_size))
        };
        let mut m = Message::new();
        m.set_to(to);
        if term.is_err() || ents.is_err() {
            // send snapshot if we failed to get term or entries
            self.prepare_send_snapshot(&mut m, to);
        } else {
            self.prepare_send_entries(&mut m, to, term.unwrap(), ents.unwrap());
        }
        self.send(m);
    }

    // send_heartbeat sends an empty MsgAppend
    fn send_heartbeat(&mut self, to: u64) {
        // Attach the commit as min(to.matched, self.raft_log.committed).
        // When the leader sends out heartbeat message,
        // the receiver(follower) might not be matched with the leader
        // or it might not have all the committed entries.
        // The leader MUST NOT forward the follower's commit to
        // an unmatched index.
        let mut m = Message::new();
        m.set_to(to);
        m.set_msg_type(MessageType::MsgHeartbeat);
        let commit = cmp::min(self.prs.get(&to).unwrap().matched, self.raft_log.committed);
        m.set_commit(commit);
        self.send(m);
    }

    // bcastAppend sends RPC, with entries to all peers that are not up-to-date
    // according to the progress recorded in r.prs.
    fn bcast_append(&mut self) {
        // TODO: avoid copy
        let keys: Vec<u64> = self.prs.keys().map(|x| *x).collect();
        for id in keys {
            if id == self.id {
                continue;
            }
            self.send_append(id);
        }
    }

    // bcastHeartbeat sends RPC, without entries to all the peers.
    fn bcast_heartbeat(&mut self) {
        // TODO: avoid copy
        let keys: Vec<u64> = self.prs.keys().map(|x| *x).collect();
        for id in keys {
            if id == self.id {
                continue;
            }
            self.send_heartbeat(id);
            self.prs.get_mut(&id).unwrap().resume()
        }
    }

    fn maybe_commit(&mut self) -> bool {
        // TODO: optimize
        let mut mis = Vec::with_capacity(self.prs.len());
        for p in self.prs.values() {
            mis.push(p.matched);
        }
        // reverse sort
        mis.sort_by(|a, b| b.cmp(a));
        let mci = mis[self.quorum() - 1];
        let term = self.get_term();
        self.raft_log.maybe_commit(mci, term)
    }

    fn reset(&mut self, term: u64) {
        if self.get_term() != term {
            self.hs.set_term(term);
            self.hs.set_vote(INVALID_ID);
        }
        self.lead = INVALID_ID;
        self.election_elapsed = 0;
        self.heartbeat_elapsed = 0;

        self.votes = HashMap::new();
        let (last_index, max_inflight) = (self.raft_log.last_index(), self.max_inflight);
        let self_id = self.id;
        for (id, p) in self.prs.iter_mut() {
            *p = new_progress(last_index + 1, max_inflight);
            if id == &self_id {
                p.matched = last_index;
            }
        }
        self.pending_conf = false;
    }

    fn append_entry(&mut self, es: &mut [Entry]) {
        let li = self.raft_log.last_index();
        for i in 0..es.len() {
            let e = es.get_mut(i).unwrap();
            e.set_term(self.get_term());
            e.set_index(li + 1 + i as u64);
        }
        self.raft_log.append(es);
        let id = self.id;
        let last_index = self.raft_log.last_index();
        self.prs.get_mut(&id).unwrap().maybe_update(last_index);
        self.maybe_commit();
    }

    pub fn tick(&mut self) {
        match self.state {
            StateRole::Candidate | StateRole::Follower => self.tick_election(),
            StateRole::Leader => self.tick_heartbeat(),
        }
    }

    // tick_election is run by followers and candidates after self.election_timeout.
    fn tick_election(&mut self) {
        if !self.promotable() {
            self.election_elapsed = 0;
            return;
        }
        self.election_elapsed += 1;
        if self.is_election_timeout() {
            self.election_elapsed = 0;
            let m = new_message(INVALID_ID, MessageType::MsgHup, Some(self.id));
            self.step(m).is_ok();
        }
    }

    // tick_heartbeat is run by leaders to send a MsgBeat after self.heartbeat_timeout.
    fn tick_heartbeat(&mut self) {
        self.heartbeat_elapsed += 1;
        self.election_elapsed += 1;

        if self.election_elapsed >= self.election_timeout {
            self.election_elapsed = 0;
            if self.check_quorum {
                let m = new_message(INVALID_ID, MessageType::MsgCheckQuorum, Some(self.id));
                self.step(m).is_ok();
            }
        }

        if self.state != StateRole::Leader {
            return;
        }

        if self.heartbeat_elapsed >= self.heartbeat_timeout {
            self.heartbeat_elapsed = 0;
            let m = new_message(INVALID_ID, MessageType::MsgBeat, Some(self.id));
            self.step(m).is_ok();
        }
    }

    pub fn become_follower(&mut self, term: u64, lead: u64) {
        self.reset(term);
        self.lead = lead;
        self.state = StateRole::Follower;
        info!("{:x} became follower at term {}", self.id, self.get_term());
    }

    pub fn become_candidate(&mut self) {
        assert!(self.state != StateRole::Leader,
                "invalid transition [leader -> candidate]");
        let term = self.get_term() + 1;
        self.reset(term);
        let id = self.id;
        self.hs.set_vote(id);
        self.state = StateRole::Candidate;
        info!("{:x} became candidate at term {}", self.id, self.get_term());
    }

    fn become_leader(&mut self) {
        assert!(self.state != StateRole::Follower,
                "invalid transition [follower -> leader]");
        let term = self.get_term();
        self.reset(term);
        self.lead = self.id;
        self.state = StateRole::Leader;
        let begin = self.raft_log.committed + 1;
        let ents = self.raft_log
                       .entries(begin, raft_log::NO_LIMIT)
                       .expect("unexpected error getting uncommitted entries");
        for e in ents {
            if e.get_entry_type() != EntryType::EntryConfChange {
                continue;
            }
            assert!(!self.pending_conf,
                    "unexpected double uncommitted config entry");
            self.pending_conf = true;
        }
        self.append_entry(&mut [Entry::new()]);
        info!("{:x} became leader at term {}", self.id, self.get_term());
    }

    fn campaign(&mut self) {
        self.become_candidate();
        let id = self.id;
        let poll_res = self.poll(id, true);
        if self.quorum() == poll_res {
            self.become_leader();
            return;
        }
        let keys: Vec<u64> = self.prs.keys().map(|x| *x).collect();
        for id in keys {
            if id == self.id {
                continue;
            }
            info!("{:x} [logterm: {}, index: {}] sent vote request to {:x} at term {}",
                  self.id,
                  self.raft_log.last_term(),
                  self.raft_log.last_index(),
                  id,
                  self.get_term());
            let mut m = new_message(id, MessageType::MsgRequestVote, None);
            m.set_index(self.raft_log.last_index());
            m.set_log_term(self.raft_log.last_term());
            self.send(m);
        }
    }

    fn get_term(&self) -> u64 {
        self.hs.get_term()
    }

    fn poll(&mut self, id: u64, v: bool) -> usize {
        if v {
            info!("{:x} received vote from {:x} at term {}",
                  self.id,
                  id,
                  self.get_term())
        } else {
            info!("{:x} received vote rejection from {:x} at term {}",
                  self.id,
                  id,
                  self.get_term())
        }
        if !self.votes[&id] {
            self.votes.insert(id, v);
        }
        self.votes.values().filter(|x| **x).count()
    }

    pub fn step(&mut self, m: Message) -> Result<()> {
        if m.get_msg_type() == MessageType::MsgHup {
            if self.state != StateRole::Leader {
                info!("{:x} is starting a new election at term {}",
                      self.id,
                      self.get_term());
                self.campaign();
                let committed = self.raft_log.committed;
                self.hs.set_commit(committed);
            } else {
                debug!("{:x} ignoring MsgHup because already leader", self.id);
            }
            return Ok(());
        }

        if m.get_term() == 0 {
            // local message
        } else if m.get_term() > self.get_term() {
            let mut lead = m.get_from();
            if m.get_msg_type() == MessageType::MsgRequestVote {
                lead = INVALID_ID;
            }
            info!("{:x} [term: {}] received a {:?} message with higher term from {:x} [term: {}]",
                  self.id,
                  self.get_term(),
                  m.get_msg_type(),
                  m.get_from(),
                  m.get_term());
            self.become_follower(m.get_term(), lead);
        } else {
            // ignore
            info!("{:x} [term: {}] ignored a {:?} message with lower term from {} [term: {}]",
                  self.id,
                  self.get_term(),
                  m.get_msg_type(),
                  m.get_from(),
                  m.get_term());
            return Ok(());
        }

        if self.skip_step.is_none() || self.skip_step.as_mut().unwrap()() {
            match self.state {
                StateRole::Candidate => self.step_candidate(m),
                StateRole::Follower => self.step_follower(m),
                StateRole::Leader => self.step_leader(m),
            }
        }
        let committed = self.raft_log.committed;
        self.hs.set_commit(committed);
        Ok(())
    }

    fn handle_append_response(&mut self,
                              m: &Message,
                              old_paused: &mut bool,
                              send_append: &mut bool,
                              maybe_commit: &mut bool) {
        let pr = self.prs.get_mut(&m.get_from()).unwrap();
        pr.recent_active = true;
        if m.get_reject() {
            debug!("{:x} received msgAppend rejection(lastindex: {}) from {:x} for index {}",
                   self.id,
                   m.get_reject_hint(),
                   m.get_from(),
                   m.get_index());
            if pr.maybe_decr_to(m.get_index(), m.get_reject_hint()) {
                debug!("{:x} decreased progress of {:x} to [{:?}]",
                       self.id,
                       m.get_from(),
                       pr);
                if pr.state == ProgressState::Replicate {
                    pr.become_probe();
                }
                *send_append = true;
            }
            return;
        }
        *old_paused = pr.is_paused();
        if !pr.maybe_update(m.get_index()) {
            return;
        }
        match pr.state {
            ProgressState::Probe => pr.become_replicate(),
            ProgressState::Snapshot if pr.maybe_snapshot_abort() => {
                debug!("{:x} snapshot aborted, resumed sending replication messages to {:x} \
                        [{:?}]",
                       self.id,
                       m.get_from(),
                       pr);
                pr.become_probe();
            }
            ProgressState::Replicate => pr.ins.free_to(m.get_index()),
            // TODO: remove this later.
            _ => {}
        }
        *maybe_commit = true;
    }

    fn handle_snapshot_status(&mut self, m: &Message) {
        let pr = self.prs.get_mut(&m.get_from()).unwrap();
        if !m.get_reject() {
            pr.become_probe();
            debug!("{:x} snapshot succeeded, resumed sending replication messages to {:x} [{:?}]",
                   self.id,
                   m.get_from(),
                   pr);
        } else {
            pr.snapshot_failure();
            pr.become_probe();
            debug!("{:x} snapshot failed, resumed sending replication messages to {:x} [{:?}]",
                   self.id,
                   m.get_from(),
                   pr);
        }
        // If snapshot finish, wait for the msgAppResp from the remote node before sending
        // out the next msgAppend.
        // If snapshot failure, wait for a heartbeat interval before next try
        pr.pause();
    }

    /// check message's progress to decide which action should be taken.
    fn check_message_with_progress(&mut self,
                                   m: &Message,
                                   send_append: &mut bool,
                                   old_paused: &mut bool,
                                   maybe_commit: &mut bool) {
        if !self.prs.contains_key(&m.get_from()) {
            debug!("no progress available for {:x}", m.get_from());
            return;
        }
        match m.get_msg_type() {
            MessageType::MsgAppendResponse => {
                self.handle_append_response(m, old_paused, send_append, maybe_commit)
            }
            MessageType::MsgHeartbeatResponse => {
                let pr = self.prs.get_mut(&m.get_from()).unwrap();
                pr.recent_active = true;

                // free one slot for the full inflights window to allow progress.
                if pr.state == ProgressState::Replicate && pr.ins.full() {
                    pr.ins.free_first_one();
                }
                if pr.matched < self.raft_log.last_index() {
                    *send_append = true;
                }
            }
            MessageType::MsgSnapStatus => {
                if self.prs[&m.get_from()].state != ProgressState::Snapshot {
                    return;
                }
                self.handle_snapshot_status(&m);
            }
            MessageType::MsgUnreachable => {
                let pr = self.prs.get_mut(&m.get_from()).unwrap();
                // During optimistic replication, if the remote becomes unreachable,
                // there is huge probability that a MsgAppend is lost.
                if pr.state == ProgressState::Replicate {
                    pr.become_probe();
                }
                debug!("{:x} failed to send message to {:x} because it is unreachable [{:?}]",
                       self.id,
                       m.get_from(),
                       pr);
            }
            _ => {}
        }
    }

    fn log_vote_reject(&self, m: &Message) {
        info!("{:x} [logterm: {}, index: {}, vote: {:x}] rejected vote from {:x} [logterm: {}, \
               index: {}] at term {}",
              self.id,
              self.raft_log.last_term(),
              self.raft_log.last_index(),
              self.hs.get_vote(),
              m.get_from(),
              m.get_log_term(),
              m.get_index(),
              self.get_term());
    }

    fn log_vote_approve(&self, m: &Message) {
        info!("{:x} [logterm: {}, index: {}, vote: {:x}] voted for {:x} [logterm: {}, index: {}] \
               at term {}",
              self.id,
              self.raft_log.last_term(),
              self.raft_log.last_index(),
              self.hs.get_vote(),
              m.get_from(),
              m.get_log_term(),
              m.get_index(),
              self.get_term());
    }

    fn step_leader(&mut self, m: Message) {
        // These message types do not require any progress for m.From.
        match m.get_msg_type() {
            MessageType::MsgBeat => {
                self.bcast_heartbeat();
                return;
            }
            MessageType::MsgCheckQuorum => {
                if !self.check_quorum_active() {
                    warn!("{:x} stepped down to follower since quorum is not active",
                          self.id);
                    let term = self.get_term();
                    self.become_follower(term, INVALID_ID);
                }
                return;
            }
            MessageType::MsgPropose => {
                if m.get_entries().len() == 0 {
                    panic!("{:x} stepped empty MsgProp", self.id);
                }
                if !self.prs.contains_key(&self.id) {
                    // If we are not currently a member of the range (i.e. this node
                    // was removed from the configuration while serving as leader),
                    // drop any new proposals.
                    return;
                }
                let mut m = m;
                if self.pending_conf {
                    for e in m.mut_entries().iter_mut() {
                        if e.get_entry_type() == EntryType::EntryConfChange {
                            *e = Entry::new();
                            e.set_entry_type(EntryType::EntryNormal);
                        }
                    }
                }
                self.append_entry(&mut m.mut_entries());
                self.bcast_append();
                return;
            }
            MessageType::MsgRequestVote => {
                self.log_vote_reject(&m);
                let mut to_sent = Message::new();
                to_sent.set_to(m.get_to());
                to_sent.set_msg_type(MessageType::MsgRequestVoteResponse);
                to_sent.set_reject(true);
                self.send(to_sent);
                return;
            }
            _ => {}
        }

        let mut send_append = false;
        let mut maybe_commit = false;
        let mut old_paused = false;
        self.check_message_with_progress(&m, &mut send_append, &mut old_paused, &mut maybe_commit);
        if maybe_commit {
            if self.maybe_commit() {
                self.bcast_append();
            } else if old_paused {
                // update() reset the wait state on this node. If we had delayed sending
                // an update before, send it now.
                send_append = true;
            }
        }
        if send_append {
            self.send_append(m.get_from());
        }
    }

    fn step_candidate(&mut self, m: Message) {
        let term = self.get_term();
        match m.get_msg_type() {
            MessageType::MsgPropose => {
                info!("{:x} no leader at term {}; dropping proposal",
                      self.id,
                      term);
                return;
            }
            MessageType::MsgAppend => {
                self.become_follower(term, m.get_from());
                self.handle_append_entries(m);
            }
            MessageType::MsgHeartbeat => {
                self.become_follower(term, m.get_from());
                self.handle_heartbeat(m);
            }
            MessageType::MsgSnapshot => {
                self.become_follower(term, m.get_from());
                self.handle_snapshot(m);
            }
            MessageType::MsgRequestVote => {
                self.log_vote_reject(&m);
                let t = MessageType::MsgRequestVoteResponse;
                let mut to_send = new_message(m.get_from(), t, None);
                to_send.set_reject(true);
                self.send(to_send);
            }
            MessageType::MsgRequestVoteResponse => {
                let gr = self.poll(m.get_from(), !m.get_reject());
                let quorum = self.quorum();
                info!("{:x} [quorum:{}] has received {} votes and {} vote rejections",
                      self.id,
                      quorum,
                      gr,
                      self.votes.len() - gr);
                if quorum == gr {
                    self.become_leader();
                    self.bcast_append();
                } else if quorum == self.votes.len() - gr {
                    self.become_follower(term, INVALID_ID);
                }
            }
            _ => {}
        }
    }

    fn step_follower(&mut self, m: Message) {
        let term = self.get_term();
        match m.get_msg_type() {
            MessageType::MsgPropose => {
                if self.lead == INVALID_ID {
                    info!("{:x} no leader at term {}; dropping proposal",
                          self.id,
                          term);
                    return;
                }
                let mut m = m;
                m.set_to(self.lead);
                self.send(m);
            }
            MessageType::MsgAppend => {
                self.election_elapsed = 0;
                self.lead = m.get_from();
                self.handle_append_entries(m);
            }
            MessageType::MsgHeartbeat => {
                self.election_elapsed = 0;
                self.lead = m.get_from();
                self.handle_heartbeat(m);
            }
            MessageType::MsgSnapshot => {
                self.election_elapsed = 0;
                self.handle_snapshot(m);
            }
            MessageType::MsgRequestVote => {
                let t = MessageType::MsgRequestVoteResponse;
                if (self.hs.get_vote() == INVALID_ID || self.hs.get_vote() == m.get_from()) &&
                   self.raft_log.is_up_to_date(m.get_index(), m.get_log_term()) {
                    self.log_vote_approve(&m);
                    self.election_elapsed = 0;
                    self.hs.set_vote(m.get_from());
                    self.send(new_message(m.get_from(), t, None));
                } else {
                    self.log_vote_reject(&m);
                    let mut to_send = new_message(m.get_from(), t, None);
                    to_send.set_reject(true);
                    self.send(to_send);
                }
            }
            _ => {}
        }
    }

    fn handle_append_entries(&mut self, m: Message) {
        if m.get_index() < self.hs.get_commit() {
            let mut to_send = Message::new();
            to_send.set_to(m.get_from());
            to_send.set_msg_type(MessageType::MsgAppendResponse);
            to_send.set_index(self.hs.get_commit());
            self.send(to_send);
            return;
        }
        let mut to_send = Message::new();
        to_send.set_to(m.get_from());
        to_send.set_msg_type(MessageType::MsgAppendResponse);
        match self.raft_log.maybe_append(m.get_index(),
                                         m.get_log_term(),
                                         m.get_commit(),
                                         m.get_entries()) {
            Some(mlast_index) => {
                to_send.set_index(mlast_index);
                self.send(to_send);
            }
            None => {
                debug!("{:x} [logterm: {}, index: {}] rejected msgApp [logterm: {}, index: {}] \
                        from {:x}",
                       self.id,
                       self.raft_log.zero_term_on_err_compacted(self.raft_log.term(m.get_index())),
                       m.get_index(),
                       m.get_log_term(),
                       m.get_index(),
                       m.get_from());
                to_send.set_index(m.get_index());
                to_send.set_reject(true);
                to_send.set_reject_hint(self.raft_log.last_index());
                self.send(to_send);
            }
        }
    }

    fn handle_heartbeat(&mut self, m: Message) {
        self.raft_log.commit_to(m.get_commit());
        let mut to_send = Message::new();
        to_send.set_to(m.get_from());
        to_send.set_msg_type(MessageType::MsgHeartbeatResponse);
        self.send(to_send);
    }

    fn handle_snapshot(&mut self, m: Message) {
        let mut m = m;
        let (sindex, sterm) = (m.get_snapshot().get_metadata().get_index(),
                               m.get_snapshot().get_metadata().get_term());
        if self.restore(m.take_snapshot()) {
            info!("{:x} [commit: {}] restored snapshot [index: {}, term: {}]",
                  self.id,
                  self.hs.get_commit(),
                  sindex,
                  sterm);
            let mut to_send = Message::new();
            to_send.set_to(m.get_from());
            to_send.set_msg_type(MessageType::MsgAppendResponse);
            to_send.set_index(self.raft_log.last_index());
            self.send(to_send);
        } else {
            info!("{:x} [commit: {}] ignored snapshot [index: {}, term: {}]",
                  self.id,
                  self.hs.get_commit(),
                  sindex,
                  sterm);
            let mut to_send = Message::new();
            to_send.set_to(m.get_from());
            to_send.set_msg_type(MessageType::MsgAppendResponse);
            to_send.set_index(self.raft_log.committed);
            self.send(to_send);
        }
    }

    fn get_commit(&self) -> u64 {
        self.hs.get_commit()
    }

    fn restore_raft(&mut self, snap: &Snapshot) -> Option<bool> {
        let meta = snap.get_metadata();
        if self.raft_log.match_term(meta.get_index(), meta.get_term()) {
            info!("{:x} [commit: {}, lastindex: {}, lastterm: {}] fast-forwarded commit to \
                   snapshot [index: {}, term: {}]",
                  self.id,
                  self.get_commit(),
                  self.raft_log.last_index(),
                  self.raft_log.last_term(),
                  meta.get_index(),
                  meta.get_term());
            self.raft_log.commit_to(meta.get_index());
            return Some(false);
        }

        info!("{:x} [commit: {}, lastindex: {}, lastterm: {}] starts to restore snapshot [index: \
               {}, term: {}]",
              self.id,
              self.get_commit(),
              self.raft_log.last_index(),
              self.raft_log.last_term(),
              meta.get_index(),
              meta.get_term());
        self.prs = HashMap::with_capacity(meta.get_conf_state().get_nodes().len());
        for n in meta.get_conf_state().get_nodes() {
            let n = *n;
            let next_idx = self.raft_log.last_index() + 1;
            let matched = if n == self.id {
                next_idx - 1
            } else {
                0
            };
            self.set_progress(n, matched, next_idx);
            info!("{:x} restored progress of {:x} [{:?}]",
                  self.id,
                  n,
                  self.prs[&n]);
        }
        None
    }

    // restore recovers the state machine from a snapshot. It restores the log and the
    // configuration of state machine.
    fn restore(&mut self, snap: Snapshot) -> bool {
        if snap.get_metadata().get_index() < self.raft_log.committed {
            return false;
        }
        if let Some(b) = self.restore_raft(&snap) {
            return b;
        }
        self.raft_log.restore(snap);
        true
    }

    // promotable indicates whether state machine can be promoted to leader,
    // which is true when its own id is in progress list.
    fn promotable(&self) -> bool {
        self.prs.contains_key(&self.id)
    }

    pub fn add_node(&mut self, id: u64) {
        if self.prs.contains_key(&id) {
            // Ignore any redundant addNode calls (which can happen because the
            // initial bootstrapping entries are applied twice).
            return;
        }
        let last_index = self.raft_log.last_index();
        self.set_progress(id, 0, last_index + 1);
        self.pending_conf = false;
    }

    pub fn remove_node(&mut self, id: u64) {
        self.del_progress(id);
        self.pending_conf = false;
    }

    pub fn reset_pending_conf(&mut self) {
        self.pending_conf = false;
    }

    fn set_progress(&mut self, id: u64, matched: u64, next_idx: u64) {
        let mut p = new_progress(next_idx, self.max_inflight);
        p.matched = matched;
        self.prs.insert(id, p);
    }

    fn del_progress(&mut self, id: u64) {
        self.prs.remove(&id);
    }

    fn load_state(&mut self, hs: HardState) {
        if hs.get_commit() < self.raft_log.committed ||
           hs.get_commit() > self.raft_log.last_index() {
            panic!("{:x} hs.commit {} is out of range [{}, {}]",
                   self.id,
                   hs.get_commit(),
                   self.raft_log.committed,
                   self.raft_log.last_index())
        }
        self.raft_log.committed = hs.get_commit();
        self.hs.set_term(hs.get_term());
        self.hs.set_vote(hs.get_vote());
        self.hs.set_commit(hs.get_commit());
    }

    // is_election_timeout returns true if self.election_elapsed is greater than the
    // randomized election timeout in (electiontimeout, 2 * electiontimeout - 1).
    // Otherwise, it returns false.
    fn is_election_timeout(&mut self) -> bool {
        if self.election_elapsed < self.election_timeout {
            return false;
        }
        let d = self.election_elapsed - self.election_timeout;
        d > self.rng.gen_range(0, self.election_timeout)
    }

    // check_quorum_active returns true if the quorum is active from
    // the view of the local raft state machine. Otherwise, it returns
    // false.
    // check_quorum_active also resets all recent_active to false.
    fn check_quorum_active(&mut self) -> bool {
        let mut act = 0;
        let self_id = self.id;
        for (id, p) in self.prs.iter_mut() {
            if id == &self_id {
                // self is always active
                act += 1;
                continue;
            }

            if p.recent_active {
                act += 1;
            }

            p.recent_active = false;
        }
        act >= self.quorum()
    }
}
