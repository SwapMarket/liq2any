//! Liquid and Polygon plumbing shared by the maker.

#![forbid(unsafe_code)]

#[cfg(feature = "native")]
pub mod esplora;
pub mod evm;
pub mod liquid;
/// Its encoding and decoding build everywhere, the browser included; only its RPC client is native.
pub mod polygon;
pub mod swap;
