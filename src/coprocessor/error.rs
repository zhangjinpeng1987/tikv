// Copyright 2018 TiKV Project Authors. Licensed under Apache-2.0.

use std::error;
use std::result;
use std::time::Duration;

use kvproto::{errorpb, kvrpcpb};
use tipb;

use crate::coprocessor;
use crate::storage;

quick_error! {
    #[derive(Debug)]
    pub enum Error {
        Region(err: errorpb::Error) {
            description("region related failure")
            display("region {:?}", err)
        }
        Locked(l: kvrpcpb::LockInfo) {
            description("key is locked")
            display("locked {:?}", l)
        }
        Outdated(elapsed: Duration, tag: &'static str) {
            description("request is outdated")
        }
        Full {
            description("Coprocessor end-point thread pool is full")
        }
        Eval(err: tipb::select::Error) {
            from()
            description("eval failed")
            display("Eval error: {}", err.get_msg())
        }
        Other(err: Box<dyn error::Error + Send + Sync>) {
            from()
            cause(err.as_ref())
            description(err.description())
            display("{}", err)
        }
    }
}

pub type Result<T> = result::Result<T, Error>;

impl From<storage::kv::Error> for Error {
    fn from(e: storage::kv::Error) -> Error {
        match e {
            storage::kv::Error::Request(e) => Error::Region(e),
            _ => Error::Other(Box::new(e)),
        }
    }
}

impl From<coprocessor::dag::expr::Error> for Error {
    fn from(e: coprocessor::dag::expr::Error) -> Error {
        Error::Eval(e.into())
    }
}

impl From<storage::txn::Error> for Error {
    fn from(e: storage::txn::Error) -> Error {
        match e {
            storage::txn::Error::Mvcc(storage::mvcc::Error::KeyIsLocked {
                primary,
                ts,
                key,
                ttl,
            }) => {
                let mut info = kvrpcpb::LockInfo::new();
                info.set_primary_lock(primary);
                info.set_lock_version(ts);
                info.set_key(key);
                info.set_lock_ttl(ttl);
                Error::Locked(info)
            }
            _ => Error::Other(Box::new(e)),
        }
    }
}
