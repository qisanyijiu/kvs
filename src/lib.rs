// #![deny(missing_docs)]

#[macro_use]
extern crate log;

pub use error::{KvsError, Result};
pub use engines::{KvStore,KvsEngine, SledKvsEngine};
pub use client::KvsClient;
pub use server::KvsServer;

mod client;
mod common;
mod engines;
mod error;
mod server;
pub mod thread_pool;