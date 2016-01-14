#![crate_type = "lib"]
#![feature(test)]
#![feature(vec_push_all)]
#![feature(convert)]

#[macro_use]
extern crate log;
extern crate test;
extern crate protobuf;
extern crate bytes;
extern crate byteorder;

pub use storage::{Storage, Dsn};

pub mod util;
pub mod raft;
mod storage;
