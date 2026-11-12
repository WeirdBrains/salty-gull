//! Where bundles and blobs come from: HTTPS now, Weirdnet relays in G4.

use crate::{Error, Result};
use salty_core::wire::BundleWire;
use std::io::Read;
use weirdnet_update::Hash32;

/// A source of bundles and blobs. Nothing from it is trusted before verification.
pub trait Transport {
    /// The bundle for a channel.
    fn bundle(&self, app: &str, channel: &str) -> Result<BundleWire>;
    /// A blob by hash.
    fn blob(&self, hash: &Hash32) -> Result<Box<dyn Read + Send>>;
}

/// HTTP(S) to a Salty Gull server.
pub struct HttpTransport {
    base: String,
    agent: ureq::Agent,
}

impl HttpTransport {
    /// `base` like `https://salty-gull.example`.
    pub fn new(base: &str) -> Self {
        Self { base: base.trim_end_matches('/').to_owned(), agent: ureq::AgentBuilder::new().build() }
    }
}

fn segment(s: &str) -> Result<&str> {
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
        return Err(Error::Invalid(format!("bad id {s:?}")));
    }
    Ok(s)
}

impl Transport for HttpTransport {
    fn bundle(&self, app: &str, channel: &str) -> Result<BundleWire> {
        let url = format!("{}/v1/apps/{}/channels/{}/bundle", self.base, segment(app)?, segment(channel)?);
        let r = self.agent.get(&url).call().map_err(|e| Error::Transport(e.to_string()))?;
        let text = r.into_string().map_err(|e| Error::Transport(e.to_string()))?;
        serde_json::from_str(&text).map_err(|e| Error::Transport(e.to_string()))
    }

    fn blob(&self, hash: &Hash32) -> Result<Box<dyn Read + Send>> {
        let url = format!("{}/v1/blobs/{}", self.base, hash.to_hex());
        let r = self.agent.get(&url).call().map_err(|e| Error::Transport(e.to_string()))?;
        Ok(Box::new(r.into_reader()))
    }
}
