//! [KCP](https://github.com/skywind3000/kcp) implementation in Rust.
//!
//! A Fast and Reliable ARQ Protocol

#![feature(stmt_expr_attributes)]

extern crate bytes;
#[macro_use]
extern crate log;

mod error;
mod kcp;

/// The `KCP` prelude
pub mod prelude {
    pub use super::Kcp;
    pub use super::get_conv;
}

pub use error::Error;
pub use kcp::{DEFAULT_KCP_OVERHEAD, MAX_KCP_OVERHEAD, Kcp, get_conv, get_token, compute_hash};

/// KCP result
pub type KcpResult<T> = Result<T, Error>;
