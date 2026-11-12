//! Salty Gull service core.
//!
//! The service stores what orgs sign and hands it to devices; it never holds org root keys.
//! Everything it accepts is checked with `weirdnet-update` first:
//!
//! - [`Store::publish_root`]: the first root must be self-consistent and signed by its own
//!   threshold; later roots must be signed by both the old and the new threshold.
//! - [`Store::put_blob`]: content-addressed by BLAKE3; the hash is checked on upload.
//! - [`Store::publish_manifest`]: signed by the root's delegated release keys, valid, unexpired,
//!   with a strictly higher seq per (app, channel), and every listed file already uploaded.
//! - [`Store::bundle`]: what a device fetches (root chain plus latest manifest).
//!
//! The single-org MVP (G3) keeps one org per server home. Hosted multi-org storage builds on
//! the same records.
#![forbid(unsafe_code)]

mod error;
#[cfg(feature = "store")]
mod store;
pub mod wire;

#[cfg(feature = "testkit")]
pub mod testkit;

pub use error::{Error, Result};
#[cfg(feature = "store")]
pub use store::{Published, Store};

#[cfg(all(test, feature = "store"))]
mod tests;
