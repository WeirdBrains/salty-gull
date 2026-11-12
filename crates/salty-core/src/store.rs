use crate::{Error, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use weirdnet_update::envelope::Domain;
use weirdnet_update::{Hash32, Root, Signed, Timestamp, UpdateBundle, UpdateManifest};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS roots (
  version INTEGER PRIMARY KEY,
  envelope BLOB NOT NULL,
  created INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS manifests (
  app TEXT NOT NULL,
  channel TEXT NOT NULL,
  seq INTEGER NOT NULL,
  update_id TEXT NOT NULL UNIQUE,
  envelope BLOB NOT NULL,
  created INTEGER NOT NULL,
  PRIMARY KEY (app, channel, seq)
);
";

/// What a publish stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Published {
    /// App id.
    pub app: String,
    /// Channel.
    pub channel: String,
    /// Sequence number.
    pub seq: u64,
    /// Update id.
    pub update_id: String,
}

/// The single-org store: SQLite for records, a directory of BLAKE3-named blobs.
pub struct Store {
    conn: Mutex<Connection>,
    blobs: PathBuf,
    org: String,
}

impl Store {
    /// Opens (or creates) the store in `home` for `org`. A home belongs to one org.
    pub fn open(home: &Path, org: &str) -> Result<Self> {
        fs::create_dir_all(home.join("blobs"))?;
        let conn = Connection::open(home.join("salty.db"))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        let have: Option<String> = conn.query_row("SELECT value FROM meta WHERE key = 'org'", [], |r| r.get(0)).optional()?;
        match have {
            Some(o) if o != org => return Err(Error::Conflict(format!("this home belongs to org {o}, not {org}"))),
            Some(_) => {}
            None => {
                conn.execute("INSERT INTO meta (key, value) VALUES ('org', ?1)", params![org])?;
            }
        }
        Ok(Self { conn: Mutex::new(conn), blobs: home.join("blobs"), org: org.to_owned() })
    }

    /// The org this store serves.
    pub fn org(&self) -> &str {
        &self.org
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Root envelopes, oldest first.
    pub fn roots(&self) -> Result<Vec<Signed>> {
        let db = self.db();
        let mut st = db.prepare("SELECT envelope FROM roots ORDER BY version")?;
        let rows = st.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
        let mut out = Vec::new();
        for b in rows {
            out.push(Signed::from_bytes(&b?)?);
        }
        Ok(out)
    }

    /// The current (latest) root, verified when it was stored.
    pub fn current_root(&self) -> Result<Option<Root>> {
        let db = self.db();
        let b: Option<Vec<u8>> = db.query_row("SELECT envelope FROM roots ORDER BY version DESC LIMIT 1", [], |r| r.get(0)).optional()?;
        b.map(|b| Ok(Signed::from_bytes(&b)?.decode()?)).transpose()
    }

    /// Stores a root after checking it against the current one (or, for the first, against
    /// its own keys). Returns the root version.
    pub fn publish_root(&self, env: &Signed, now: Timestamp) -> Result<u32> {
        if env.domain != Domain::Root {
            return Err(Error::Invalid("not a root record".into()));
        }
        let root = match self.current_root()? {
            None => {
                let claimed: Root = env.decode()?;
                Root::verify_initial(&claimed.pin(), env, now)?
            }
            Some(cur) => cur.verify_next(env, now)?,
        };
        if root.org != self.org {
            return Err(Error::Invalid(format!("root is for org {}, this server serves {}", root.org, self.org)));
        }
        let db = self.db();
        db.execute("INSERT INTO roots (version, envelope, created) VALUES (?1, ?2, ?3)", params![root.version, env.to_bytes()?, now.0 as i64])
            .map_err(|e| match e {
                rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ConstraintViolation => {
                    Error::Conflict(format!("root version {} exists", root.version))
                }
                e => Error::Db(e),
            })?;
        Ok(root.version)
    }

    fn blob_file(&self, hash: &Hash32) -> PathBuf {
        self.blobs.join(hash.to_hex())
    }

    /// Stores `data` under its BLAKE3 hash, which must equal `hash`.
    pub fn put_blob(&self, hash: &Hash32, data: &[u8]) -> Result<()> {
        if Hash32::of(data) != *hash {
            return Err(Error::Invalid("blob does not match its hash".into()));
        }
        let path = self.blob_file(hash);
        if path.is_file() {
            return Ok(());
        }
        let tmp = self.blobs.join(format!(".{}.part", hash.to_hex()));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(data)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Path of a stored blob.
    pub fn blob_path(&self, hash: &Hash32) -> Option<PathBuf> {
        let p = self.blob_file(hash);
        p.is_file().then_some(p)
    }

    /// Size of a stored blob.
    fn blob_size(&self, hash: &Hash32) -> Option<u64> {
        fs::metadata(self.blob_file(hash)).ok().map(|m| m.len())
    }

    /// Stores a manifest signed by the current root's release keys.
    pub fn publish_manifest(&self, env: &Signed, now: Timestamp) -> Result<Published> {
        let root = self.current_root()?.ok_or_else(|| Error::Invalid("publish a root first".into()))?;
        if env.domain != Domain::Manifest {
            return Err(Error::Invalid("not a manifest record".into()));
        }
        // The app and channel pick the delegation; nothing else is trusted before the check.
        let claimed: UpdateManifest = env.decode()?;
        let keys = root.release_keys_for(&claimed.app, &claimed.channel, now);
        if keys.is_empty() {
            return Err(Error::Invalid(format!("no release key is delegated for {}", claimed.target())));
        }
        env.verify_threshold(Domain::Manifest, &keys, root.release_threshold.into())?;
        let m: UpdateManifest = env.decode()?;
        m.validate()?;
        if now >= m.expires {
            return Err(Error::Invalid("manifest has expired".into()));
        }
        for f in &m.files {
            match self.blob_size(&f.blake3) {
                Some(s) if s == f.size => {}
                Some(_) => return Err(Error::Invalid(format!("{}: uploaded blob has a different size", f.path))),
                None => return Err(Error::Invalid(format!("{}: blob {} not uploaded", f.path, f.blake3.to_hex()))),
            }
        }
        let db = self.db();
        let last: Option<i64> =
            db.query_row("SELECT MAX(seq) FROM manifests WHERE app = ?1 AND channel = ?2", params![m.app, m.channel], |r| r.get(0))?;
        if let Some(last) = last
            && m.seq as i64 <= last
        {
            return Err(Error::Conflict(format!("seq {} is not above the latest {last} for {}", m.seq, m.target())));
        }
        db.execute(
            "INSERT INTO manifests (app, channel, seq, update_id, envelope, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![m.app, m.channel, m.seq as i64, m.update_id, env.to_bytes()?, now.0 as i64],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ConstraintViolation => {
                Error::Conflict(format!("update id {} exists", m.update_id))
            }
            e => Error::Db(e),
        })?;
        Ok(Published { app: m.app, channel: m.channel, seq: m.seq, update_id: m.update_id })
    }

    /// The latest manifest for a channel.
    pub fn latest(&self, app: &str, channel: &str) -> Result<Option<Signed>> {
        let db = self.db();
        let b: Option<Vec<u8>> = db
            .query_row("SELECT envelope FROM manifests WHERE app = ?1 AND channel = ?2 ORDER BY seq DESC LIMIT 1", params![app, channel], |r| {
                r.get(0)
            })
            .optional()?;
        b.map(|b| Ok(Signed::from_bytes(&b)?)).transpose()
    }

    /// What a device fetches: the root chain and the latest manifest.
    pub fn bundle(&self, app: &str, channel: &str) -> Result<UpdateBundle> {
        Ok(UpdateBundle { roots: self.roots()?, manifest: self.latest(app, channel)?, freshness: None })
    }
}
