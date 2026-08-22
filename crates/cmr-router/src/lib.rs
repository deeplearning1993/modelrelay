//! Local-only Codex/Responses compatibility router.
//!
//! The router merges the live official Codex model catalog with user-selected
//! provider models. Official traffic keeps the user's `ChatGPT` authentication and
//! is sent to the official backend; external traffic receives only the credential
//! explicitly referenced by that provider.

mod catalog;
mod error;
mod server;
mod transport;

pub use error::RouterError;
pub use server::{AppState, build_router, serve};

/// Router result alias.
pub type Result<T> = std::result::Result<T, RouterError>;
