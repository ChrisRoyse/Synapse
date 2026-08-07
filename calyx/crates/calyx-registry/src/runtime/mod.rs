//! Lens runtime implementations.

pub mod adapters;
pub mod algorithmic;
pub(crate) mod batch_scope;
#[cfg(feature = "embedding-runtimes")]
pub mod candle;
pub(crate) mod common;
pub mod external_cmd;
#[cfg(feature = "embedding-runtimes")]
pub mod onnx;
#[cfg(feature = "embedding-runtimes")]
pub mod qwen3;
#[cfg(feature = "embedding-runtimes")]
pub mod static_lookup;
pub mod tei_http;
