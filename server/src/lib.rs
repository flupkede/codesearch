//! PostgreSQL-backed, unauthenticated loopback HTTP foundation (upstream #384 stage 2).
mod document;
mod embedding;
mod http;
mod store;
mod types;

pub use embedding::Embedding;
pub use http::router;
pub use store::Store;
pub use types::*;

#[cfg(test)]
#[path = "integration_tests.rs"]
mod integration_tests;
