//! JSON wire format: envelopes travel as hex of their exact CBOR bytes.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use weirdnet_update::{Signed, UpdateBundle};

/// An update bundle on the wire.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleWire {
    /// Root chain, oldest first.
    pub roots: Vec<String>,
    /// Latest manifest for the channel, if any.
    #[serde(default)]
    pub manifest: Option<String>,
    /// Freshness record, if the server signs them.
    #[serde(default)]
    pub freshness: Option<String>,
}

/// Hex of an envelope's CBOR.
pub fn encode(env: &Signed) -> Result<String> {
    Ok(hex::encode(env.to_bytes()?))
}

/// Parses hex CBOR into an envelope.
pub fn decode(s: &str) -> Result<Signed> {
    let bytes = hex::decode(s).map_err(|_| Error::Invalid("envelope is not hex".into()))?;
    Ok(Signed::from_bytes(&bytes)?)
}

impl BundleWire {
    /// From a bundle.
    pub fn from_bundle(b: &UpdateBundle) -> Result<Self> {
        Ok(Self {
            roots: b.roots.iter().map(encode).collect::<Result<_>>()?,
            manifest: b.manifest.as_ref().map(encode).transpose()?,
            freshness: b.freshness.as_ref().map(encode).transpose()?,
        })
    }

    /// Into a bundle.
    pub fn to_bundle(&self) -> Result<UpdateBundle> {
        Ok(UpdateBundle {
            roots: self.roots.iter().map(|s| decode(s)).collect::<Result<_>>()?,
            manifest: self.manifest.as_deref().map(decode).transpose()?,
            freshness: self.freshness.as_deref().map(decode).transpose()?,
        })
    }
}
