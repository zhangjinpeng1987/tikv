// Copyright 2017 TiKV Project Authors. Licensed under Apache-2.0.

use std::collections::Bound::Excluded;
use std::mem;
use std::sync::Mutex;

use engine::rocks;
use engine::rocks::DB;
use engine::LARGE_CFS;
use engine::{util, Range};
use engine::{CF_DEFAULT, CF_WRITE};
use kvproto::metapb::Region;
use kvproto::pdpb::CheckPolicy;
use tikv_util::escape;

use crate::raftstore::store::{keys, CasualMessage, CasualRouter};

use super::super::error::Result;
use super::super::metrics::*;
use super::super::properties::RangeProperties;
use super::super::{Coprocessor, KeyEntry, ObserverContext, SplitCheckObserver, SplitChecker};
use super::Host;

pub struct Checker {
    max_size: u64,
    split_size: u64,
    current_size: u64,
    split_keys: Vec<Vec<u8>>,
    batch_split_limit: u64,
    policy: CheckPolicy,
}

impl Checker {
    pub fn new(
        max_size: u64,
        split_size: u64,
        batch_split_limit: u64,
        policy: CheckPolicy,
    ) -> Checker {
        Checker {
            max_size,
            split_size,
            current_size: 0,
            split_keys: Vec::with_capacity(1),
            batch_split_limit,
            policy,
        }
    }
}

impl SplitChecker for Checker {
    fn on_kv(&mut self, _: &mut ObserverContext<'_>, entry: &KeyEntry) -> bool {
        let size = entry.entry_size() as u64;
        self.current_size += size;

        let mut over_limit = self.split_keys.len() as u64 >= self.batch_split_limit;
        if self.current_size > self.split_size && !over_limit {
            self.split_keys.push(keys::origin_key(entry.key()).to_vec());
            // if for previous on_kv() self.current_size == self.split_size,
            // the split key would be pushed this time, but the entry size for this time should not be ignored.
            self.current_size = if self.current_size - size == self.split_size {
                size
            } else {
                0
            };
            over_limit = self.split_keys.len() as u64 >= self.batch_split_limit;
        }

        // For a large region, scan over the range maybe cost too much time,
        // so limit the number of produced split_key for one batch.
        // Also need to scan over self.max_size for last part.
        over_limit && self.current_size + self.split_size >= self.max_size
    }

    fn split_keys(&mut self) -> Vec<Vec<u8>> {
        // make sure not to split when less than max_size for last part
        if self.current_size + self.split_size < self.max_size {
            self.split_keys.pop();
        }
        if !self.split_keys.is_empty() {
            mem::replace(&mut self.split_keys, vec![])
        } else {
            vec![]
        }
    }

    fn policy(&self) -> CheckPolicy {
        self.policy
    }

    fn approximate_split_keys(&mut self, region: &Region, engine: &DB) -> Result<Vec<Vec<u8>>> {
        Ok(box_try!(get_approximate_split_keys(
            engine,
            region,
            self.split_size,
            self.max_size,
            self.batch_split_limit,
        )))
    }
}

pub struct SizeCheckObserver<C> {
    region_max_size: u64,
    split_size: u64,
    split_limit: u64,
    router: Mutex<C>,
}

impl<C: CasualRouter> SizeCheckObserver<C> {
    pub fn new(
        region_max_size: u64,
        split_size: u64,
        split_limit: u64,
        router: C,
    ) -> SizeCheckObserver<C> {
        SizeCheckObserver {
            region_max_size,
            split_size,
            split_limit,
            router: Mutex::new(router),
        }
    }
}

impl<C> Coprocessor for SizeCheckObserver<C> {}

impl<C: CasualRouter + Send> SplitCheckObserver for SizeCheckObserver<C> {
    fn add_checker(
        &self,
        ctx: &mut ObserverContext<'_>,
        host: &mut Host,
        engine: &DB,
        mut policy: CheckPolicy,
    ) {
        let region = ctx.region();
        let region_id = region.get_id();
        let region_size = match get_region_approximate_size(engine, &region) {
            Ok(size) => size,
            Err(e) => {
                warn!(
                    "failed to get approximate stat";
                    "region_id" => region_id,
                    "err" => %e,
                );
                // Need to check size.
                host.add_checker(Box::new(Checker::new(
                    self.region_max_size,
                    self.split_size,
                    self.split_limit,
                    policy,
                )));
                return;
            }
        };

        // send it to raftstore to update region approximate size
        let res = CasualMessage::RegionApproximateSize { size: region_size };
        if let Err(e) = self.router.lock().unwrap().send(region_id, res) {
            warn!(
                "failed to send approximate region size";
                "region_id" => region_id,
                "err" => %e,
            );
        }

        REGION_SIZE_HISTOGRAM.observe(region_size as f64);
        if region_size >= self.region_max_size {
            info!(
                "approximate size over threshold, need to do split check";
                "region_id" => region.get_id(),
                "size" => region_size,
                "threshold" => self.region_max_size,
            );
            // when meet large region use approximate way to produce split keys
            if region_size >= self.region_max_size * self.split_limit * 2 {
                policy = CheckPolicy::APPROXIMATE
            }
            // Need to check size.
            host.add_checker(Box::new(Checker::new(
                self.region_max_size,
                self.split_size,
                self.split_limit,
                policy,
            )));
        } else {
            // Does not need to check size.
            debug!(
                "approximate size less than threshold, does not need to do split check";
                "region_id" => region.get_id(),
                "size" => region_size,
                "threshold" => self.region_max_size,
            );
        }
    }
}

/// Get the approximate size of the range.
pub fn get_region_approximate_size(db: &DB, region: &Region) -> Result<u64> {
    let mut size = 0;
    for cfname in LARGE_CFS {
        size += get_region_approximate_size_cf(db, cfname, &region)?
    }
    Ok(size)
}

pub fn get_region_approximate_size_cf(db: &DB, cfname: &str, region: &Region) -> Result<u64> {
    let cf = box_try!(rocks::util::get_cf_handle(db, cfname));
    let start_key = keys::enc_start_key(region);
    let end_key = keys::enc_end_key(region);
    let range = Range::new(&start_key, &end_key);
    let (_, mut size) = db.get_approximate_memtable_stats_cf(cf, &range);

    let collection = box_try!(util::get_range_properties_cf(
        db, cfname, &start_key, &end_key
    ));
    for (_, v) in &*collection {
        let props = box_try!(RangeProperties::decode(v.user_collected_properties()));
        size += props.get_approximate_size_in_range(&start_key, &end_key);
    }
    Ok(size)
}

/// Get region approximate split keys based on default and write cf.
fn get_approximate_split_keys(
    db: &DB,
    region: &Region,
    split_size: u64,
    max_size: u64,
    batch_split_limit: u64,
) -> Result<Vec<Vec<u8>>> {
    let get_cf_size = |cf: &str| get_region_approximate_size_cf(db, cf, &region);

    let default_cf_size = box_try!(get_cf_size(CF_DEFAULT));
    let write_cf_size = box_try!(get_cf_size(CF_WRITE));
    if default_cf_size + write_cf_size == 0 {
        return Err(box_err!("default cf and write cf is empty"));
    }

    // assume the size of keys is uniform distribution in both cfs.
    let (cf, cf_split_size) = if default_cf_size >= write_cf_size {
        (
            CF_DEFAULT,
            split_size * default_cf_size / (default_cf_size + write_cf_size),
        )
    } else {
        (
            CF_WRITE,
            split_size * write_cf_size / (default_cf_size + write_cf_size),
        )
    };

    get_approximate_split_keys_cf(db, cf, &region, cf_split_size, max_size, batch_split_limit)
}

fn get_approximate_split_keys_cf(
    db: &DB,
    cfname: &str,
    region: &Region,
    split_size: u64,
    max_size: u64,
    batch_split_limit: u64,
) -> Result<Vec<Vec<u8>>> {
    let start = keys::enc_start_key(region);
    let end = keys::enc_end_key(region);
    let collection = box_try!(util::get_range_properties_cf(db, cfname, &start, &end));

    let mut keys = vec![];
    let mut total_size = 0;
    for (_, v) in &*collection {
        let props = box_try!(RangeProperties::decode(v.user_collected_properties()));
        total_size += props.get_approximate_size_in_range(&start, &end);
        keys.extend(
            props
                .offsets
                .range::<[u8], _>((Excluded(start.as_slice()), Excluded(end.as_slice())))
                .map(|(k, _)| k.to_owned()),
        );
    }
    if keys.len() == 1 {
        return Ok(vec![]);
    }
    if keys.is_empty() || total_size == 0 || split_size == 0 {
        return Err(box_err!(
            "unexpected key len {} or total_size {} or split size {}, len of collection {}, cf {}, start {}, end {}",
            keys.len(),
            total_size,
            split_size,
            collection.len(),
            cfname,
            escape(&start),
            escape(&end)
        ));
    }
    keys.sort();

    // use total size of this range and the number of keys in this range to
    // calculate the average distance between two keys, and we produce a
    // split_key every `split_size / distance` keys.
    let len = keys.len();
    let distance = total_size as f64 / len as f64;
    let n = (split_size as f64 / distance).ceil() as usize;
    if n == 0 {
        return Err(box_err!(
            "unexpected n == 0, total_size: {}, split_size: {}, len: {}, distance: {}",
            total_size,
            split_size,
            keys.len(),
            distance
        ));
    }

    // cause first element of the iterator will always be returned by step_by(),
    // so the first key returned may not the desired split key. Note that, the
    // start key of region is not included, so we we drop first n - 1 keys.
    //
    // For example, the split size is `3 * distance`. And the numbers stand for the
    // key in `RangeProperties`, `^` stands for produced split key.
    //
    // skip:
    // start___1___2___3___4___5___6___7....
    //                 ^           ^
    //
    // not skip:
    // start___1___2___3___4___5___6___7....
    //         ^           ^           ^
    let mut split_keys = keys
        .into_iter()
        .skip(n - 1)
        .step_by(n)
        .collect::<Vec<Vec<u8>>>();

    if split_keys.len() as u64 > batch_split_limit {
        split_keys.truncate(batch_split_limit as usize);
    } else {
        // make sure not to split when less than max_size for last part
        let rest = (len % n) as u64;
        if rest * distance as u64 + split_size < max_size {
            split_keys.pop();
        }
    }
    Ok(split_keys)
}

#[cfg(test)]
pub mod tests {
    use super::Checker;
    use crate::raftstore::coprocessor::properties::RangePropertiesCollectorFactory;
    use crate::raftstore::coprocessor::{Config, CoprocessorHost, ObserverContext, SplitChecker};
    use crate::raftstore::store::{
        keys, CasualMessage, KeyEntry, SplitCheckRunner, SplitCheckTask,
    };
    use crate::storage::Key;
    use engine::rocks::util::{new_engine_opt, CFOptions};
    use engine::rocks::{ColumnFamilyOptions, DBOptions, Writable};
    use engine::{ALL_CFS, CF_DEFAULT, CF_WRITE, LARGE_CFS};
    use kvproto::metapb::Peer;
    use kvproto::metapb::Region;
    use kvproto::pdpb::CheckPolicy;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::{iter, u64};
    use tempdir::TempDir;
    use tikv_util::config::ReadableSize;
    use tikv_util::worker::Runnable;

    use super::*;

    pub fn must_split_at(
        rx: &mpsc::Receiver<(u64, CasualMessage)>,
        exp_region: &Region,
        exp_split_keys: Vec<Vec<u8>>,
    ) {
        loop {
            match rx.try_recv() {
                Ok((region_id, CasualMessage::RegionApproximateSize { .. }))
                | Ok((region_id, CasualMessage::RegionApproximateKeys { .. })) => {
                    assert_eq!(region_id, exp_region.get_id());
                }
                Ok((
                    region_id,
                    CasualMessage::SplitRegion {
                        region_epoch,
                        split_keys,
                        ..
                    },
                )) => {
                    assert_eq!(region_id, exp_region.get_id());
                    assert_eq!(&region_epoch, exp_region.get_region_epoch());
                    assert_eq!(split_keys, exp_split_keys);
                    break;
                }
                others => panic!("expect split check result, but got {:?}", others),
            }
        }
    }

    #[test]
    fn test_split_check() {
        let path = TempDir::new("test-raftstore").unwrap();
        let path_str = path.path().to_str().unwrap();
        let db_opts = DBOptions::new();
        let mut cf_opts = ColumnFamilyOptions::new();
        let f = Box::new(RangePropertiesCollectorFactory::default());
        cf_opts.add_table_properties_collector_factory("tikv.range-collector", f);

        let cfs_opts = ALL_CFS
            .iter()
            .map(|cf| CFOptions::new(cf, cf_opts.clone()))
            .collect();
        let engine = Arc::new(new_engine_opt(path_str, db_opts, cfs_opts).unwrap());

        let mut region = Region::new();
        region.set_id(1);
        region.set_start_key(vec![]);
        region.set_end_key(vec![]);
        region.mut_peers().push(Peer::new());
        region.mut_region_epoch().set_version(2);
        region.mut_region_epoch().set_conf_ver(5);

        let (tx, rx) = mpsc::sync_channel(100);
        let mut cfg = Config::default();
        cfg.region_max_size = ReadableSize(100);
        cfg.region_split_size = ReadableSize(60);
        cfg.batch_split_limit = 5;

        let mut runnable = SplitCheckRunner::new(
            Arc::clone(&engine),
            tx.clone(),
            Arc::new(CoprocessorHost::new(cfg, tx.clone())),
        );

        // so split key will be [z0006]
        for i in 0..7 {
            let s = keys::data_key(format!("{:04}", i).as_bytes());
            engine.put(&s, &s).unwrap();
        }

        runnable.run(SplitCheckTask::new(region.clone(), true, CheckPolicy::SCAN));
        // size has not reached the max_size 100 yet.
        match rx.try_recv() {
            Ok((region_id, CasualMessage::RegionApproximateSize { .. })) => {
                assert_eq!(region_id, region.get_id());
            }
            others => panic!("expect recv empty, but got {:?}", others),
        }

        for i in 7..11 {
            let s = keys::data_key(format!("{:04}", i).as_bytes());
            engine.put(&s, &s).unwrap();
        }

        // Approximate size of memtable is inaccurate for small data,
        // we flush it to SST so we can use the size properties instead.
        engine.flush(true).unwrap();

        runnable.run(SplitCheckTask::new(region.clone(), true, CheckPolicy::SCAN));
        must_split_at(&rx, &region, vec![b"0006".to_vec()]);

        // so split keys will be [z0006, z0012]
        for i in 11..19 {
            let s = keys::data_key(format!("{:04}", i).as_bytes());
            engine.put(&s, &s).unwrap();
        }
        engine.flush(true).unwrap();
        runnable.run(SplitCheckTask::new(region.clone(), true, CheckPolicy::SCAN));
        must_split_at(&rx, &region, vec![b"0006".to_vec(), b"0012".to_vec()]);

        // for test batch_split_limit
        // so split kets will be [z0006, z0012, z0018, z0024, z0030]
        for i in 19..51 {
            let s = keys::data_key(format!("{:04}", i).as_bytes());
            engine.put(&s, &s).unwrap();
        }
        engine.flush(true).unwrap();
        runnable.run(SplitCheckTask::new(region.clone(), true, CheckPolicy::SCAN));
        must_split_at(
            &rx,
            &region,
            vec![
                b"0006".to_vec(),
                b"0012".to_vec(),
                b"0018".to_vec(),
                b"0024".to_vec(),
                b"0030".to_vec(),
            ],
        );

        drop(rx);
        // It should be safe even the result can't be sent back.
        runnable.run(SplitCheckTask::new(region, true, CheckPolicy::SCAN));
    }

    #[test]
    fn test_checker_with_same_max_and_split_size() {
        let mut checker = Checker::new(24, 24, 1, CheckPolicy::SCAN);
        let region = Region::default();
        let mut ctx = ObserverContext::new(&region);
        loop {
            let data = KeyEntry::new(b"zxxxx".to_vec(), 0, 4, CF_WRITE);
            if checker.on_kv(&mut ctx, &data) {
                break;
            }
        }

        assert!(!checker.split_keys().is_empty());
    }

    #[test]
    fn test_checker_with_max_twice_bigger_than_split_size() {
        let mut checker = Checker::new(20, 10, 1, CheckPolicy::SCAN);
        let region = Region::default();
        let mut ctx = ObserverContext::new(&region);
        for _ in 0..2 {
            let data = KeyEntry::new(b"zxxxx".to_vec(), 0, 5, CF_WRITE);
            if checker.on_kv(&mut ctx, &data) {
                break;
            }
        }

        assert!(!checker.split_keys().is_empty());
    }

    fn make_region(id: u64, start_key: Vec<u8>, end_key: Vec<u8>) -> Region {
        let mut peer = Peer::new();
        peer.set_id(id);
        peer.set_store_id(id);
        let mut region = Region::new();
        region.set_id(id);
        region.set_start_key(start_key);
        region.set_end_key(end_key);
        region.mut_peers().push(peer);
        region
    }

    #[test]
    fn test_get_approximate_split_keys_error() {
        let tmp = TempDir::new("test_raftstore_util").unwrap();
        let path = tmp.path().to_str().unwrap();

        let db_opts = DBOptions::new();
        let mut cf_opts = ColumnFamilyOptions::new();
        cf_opts.set_level_zero_file_num_compaction_trigger(10);

        let cfs_opts = LARGE_CFS
            .iter()
            .map(|cf| CFOptions::new(cf, cf_opts.clone()))
            .collect();
        let engine = rocks::util::new_engine_opt(path, db_opts, cfs_opts).unwrap();

        let region = make_region(1, vec![], vec![]);
        assert_eq!(
            get_approximate_split_keys(&engine, &region, 3, 5, 1).is_err(),
            true
        );

        let cf_handle = engine.cf_handle(CF_DEFAULT).unwrap();
        let mut big_value = Vec::with_capacity(256);
        big_value.extend(iter::repeat(b'v').take(256));
        for i in 0..100 {
            let k = format!("key_{:03}", i).into_bytes();
            let k = keys::data_key(Key::from_raw(&k).as_encoded());
            engine.put_cf(cf_handle, &k, &big_value).unwrap();
            engine.flush_cf(cf_handle, true).unwrap();
        }
        assert_eq!(
            get_approximate_split_keys(&engine, &region, 3, 5, 1).is_err(),
            true
        );
    }

    #[test]
    fn test_get_approximate_split_keys() {
        let tmp = TempDir::new("test_raftstore_util").unwrap();
        let path = tmp.path().to_str().unwrap();

        let db_opts = DBOptions::new();
        let mut cf_opts = ColumnFamilyOptions::new();
        cf_opts.set_level_zero_file_num_compaction_trigger(10);
        let f = Box::new(RangePropertiesCollectorFactory::default());
        cf_opts.add_table_properties_collector_factory("tikv.size-collector", f);
        let cfs_opts = LARGE_CFS
            .iter()
            .map(|cf| CFOptions::new(cf, cf_opts.clone()))
            .collect();
        let engine = rocks::util::new_engine_opt(path, db_opts, cfs_opts).unwrap();

        let cf_handle = engine.cf_handle(CF_DEFAULT).unwrap();
        let mut big_value = Vec::with_capacity(256);
        big_value.extend(iter::repeat(b'v').take(256));

        // total size for one key and value
        const ENTRY_SIZE: u64 = 256 + 9;

        for i in 0..4 {
            let k = format!("key_{:03}", i).into_bytes();
            let k = keys::data_key(Key::from_raw(&k).as_encoded());
            engine.put_cf(cf_handle, &k, &big_value).unwrap();
            // Flush for every key so that we can know the exact middle key.
            engine.flush_cf(cf_handle, true).unwrap();
        }
        let region = make_region(1, vec![], vec![]);
        let split_keys =
            get_approximate_split_keys(&engine, &region, 3 * ENTRY_SIZE, 5 * ENTRY_SIZE, 1)
                .unwrap()
                .into_iter()
                .map(|k| {
                    Key::from_encoded_slice(keys::origin_key(&k))
                        .into_raw()
                        .unwrap()
                })
                .collect::<Vec<Vec<u8>>>();

        assert_eq!(split_keys.is_empty(), true);

        for i in 4..5 {
            let k = format!("key_{:03}", i).into_bytes();
            let k = keys::data_key(Key::from_raw(&k).as_encoded());
            engine.put_cf(cf_handle, &k, &big_value).unwrap();
            // Flush for every key so that we can know the exact middle key.
            engine.flush_cf(cf_handle, true).unwrap();
        }
        let split_keys =
            get_approximate_split_keys(&engine, &region, 3 * ENTRY_SIZE, 5 * ENTRY_SIZE, 5)
                .unwrap()
                .into_iter()
                .map(|k| {
                    Key::from_encoded_slice(keys::origin_key(&k))
                        .into_raw()
                        .unwrap()
                })
                .collect::<Vec<Vec<u8>>>();

        assert_eq!(split_keys, vec![b"key_002".to_vec()]);

        for i in 5..10 {
            let k = format!("key_{:03}", i).into_bytes();
            let k = keys::data_key(Key::from_raw(&k).as_encoded());
            engine.put_cf(cf_handle, &k, &big_value).unwrap();
            // Flush for every key so that we can know the exact middle key.
            engine.flush_cf(cf_handle, true).unwrap();
        }
        let split_keys =
            get_approximate_split_keys(&engine, &region, 3 * ENTRY_SIZE, 5 * ENTRY_SIZE, 5)
                .unwrap()
                .into_iter()
                .map(|k| {
                    Key::from_encoded_slice(keys::origin_key(&k))
                        .into_raw()
                        .unwrap()
                })
                .collect::<Vec<Vec<u8>>>();

        assert_eq!(split_keys, vec![b"key_002".to_vec(), b"key_005".to_vec()]);

        for i in 10..20 {
            let k = format!("key_{:03}", i).into_bytes();
            let k = keys::data_key(Key::from_raw(&k).as_encoded());
            engine.put_cf(cf_handle, &k, &big_value).unwrap();
            // Flush for every key so that we can know the exact middle key.
            engine.flush_cf(cf_handle, true).unwrap();
        }
        let split_keys =
            get_approximate_split_keys(&engine, &region, 3 * ENTRY_SIZE, 5 * ENTRY_SIZE, 5)
                .unwrap()
                .into_iter()
                .map(|k| {
                    Key::from_encoded_slice(keys::origin_key(&k))
                        .into_raw()
                        .unwrap()
                })
                .collect::<Vec<Vec<u8>>>();

        assert_eq!(
            split_keys,
            vec![
                b"key_002".to_vec(),
                b"key_005".to_vec(),
                b"key_008".to_vec(),
                b"key_011".to_vec(),
                b"key_014".to_vec(),
            ]
        );
    }

    #[test]
    fn test_region_approximate_size() {
        let path = TempDir::new("_test_raftstore_region_approximate_size").expect("");
        let path_str = path.path().to_str().unwrap();
        let db_opts = DBOptions::new();
        let mut cf_opts = ColumnFamilyOptions::new();
        cf_opts.set_level_zero_file_num_compaction_trigger(10);
        let f = Box::new(RangePropertiesCollectorFactory::default());
        cf_opts.add_table_properties_collector_factory("tikv.range-collector", f);
        let cfs_opts = LARGE_CFS
            .iter()
            .map(|cf| CFOptions::new(cf, cf_opts.clone()))
            .collect();
        let db = rocks::util::new_engine_opt(path_str, db_opts, cfs_opts).unwrap();

        let cases = [("a", 1024), ("b", 2048), ("c", 4096)];
        let cf_size = 2 + 1024 + 2 + 2048 + 2 + 4096;
        for &(key, vlen) in &cases {
            for cfname in LARGE_CFS {
                let k1 = keys::data_key(key.as_bytes());
                let v1 = vec![0; vlen as usize];
                assert_eq!(k1.len(), 2);
                let cf = db.cf_handle(cfname).unwrap();
                db.put_cf(cf, &k1, &v1).unwrap();
                db.flush_cf(cf, true).unwrap();
            }
        }

        let region = make_region(1, vec![], vec![]);
        let size = get_region_approximate_size(&db, &region).unwrap();
        assert_eq!(size, cf_size * LARGE_CFS.len() as u64);
        for cfname in LARGE_CFS {
            let size = get_region_approximate_size_cf(&db, cfname, &region).unwrap();
            assert_eq!(size, cf_size);
        }
    }

    #[test]
    fn test_region_maybe_inaccurate_approximate_size() {
        let path =
            TempDir::new("_test_raftstore_region_maybe_inaccurate_approximate_size").expect("");
        let path_str = path.path().to_str().unwrap();
        let db_opts = DBOptions::new();
        let mut cf_opts = ColumnFamilyOptions::new();
        cf_opts.set_disable_auto_compactions(true);
        let f = Box::new(RangePropertiesCollectorFactory::default());
        cf_opts.add_table_properties_collector_factory("tikv.range-collector", f);
        let cfs_opts = LARGE_CFS
            .iter()
            .map(|cf| CFOptions::new(cf, cf_opts.clone()))
            .collect();
        let db = rocks::util::new_engine_opt(path_str, db_opts, cfs_opts).unwrap();

        let mut cf_size = 0;
        for i in 0..100 {
            let k1 = keys::data_key(format!("k1{}", i).as_bytes());
            let k2 = keys::data_key(format!("k9{}", i).as_bytes());
            let v = vec![0; 4096];
            cf_size += k1.len() + k2.len() + v.len() * 2;
            let cf = db.cf_handle("default").unwrap();
            db.put_cf(cf, &k1, &v).unwrap();
            db.put_cf(cf, &k2, &v).unwrap();
            db.flush_cf(cf, true).unwrap();
        }

        let region = make_region(1, vec![], vec![]);
        let size = get_region_approximate_size(&db, &region).unwrap();
        assert_eq!(size, cf_size as u64);

        let region = make_region(1, b"k2".to_vec(), b"k8".to_vec());
        let size = get_region_approximate_size(&db, &region).unwrap();
        assert_eq!(size, 0);
    }
}
