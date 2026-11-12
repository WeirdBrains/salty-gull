//! `salty-server`: the single-org Salty Gull update server (G3 MVP).
//!
//! Devices read; publishers write with the admin bearer token. Every write is verified by
//! `salty-core` before it is stored, so the server never needs an org's signing keys.
//!
//! | Method | Path | Auth | What |
//! |---|---|---|---|
//! | GET | `/healthz` | no | `ok` |
//! | GET | `/v1/roots` | no | root chain (`{"roots": [hex CBOR]}`) |
//! | GET | `/v1/apps/{app}/channels/{channel}/bundle` | no | roots plus latest manifest |
//! | GET | `/v1/blobs/{blake3}` | no | blob bytes |
//! | PUT | `/v1/blobs/{blake3}` | yes | upload a blob (hash checked) |
//! | POST | `/v1/roots` | yes | publish a root (CBOR envelope body) |
//! | POST | `/v1/manifests` | yes | publish a manifest (CBOR envelope body) |
#![forbid(unsafe_code)]

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use salty_core::wire::{BundleWire, encode};
use salty_core::{Error, Store};
use serde_json::json;
use std::sync::Arc;
use weirdnet_update::{Hash32, Signed, Timestamp};

/// Largest blob accepted in one upload.
pub const MAX_BLOB_BYTES: usize = 1 << 30;

/// Shared server state.
pub struct App {
    /// The store.
    pub store: Store,
    /// SHA-256 (hex) of the admin token.
    pub admin_token_hash: Option<String>,
}

type Shared = Arc<App>;

/// The HTTP routes.
pub fn router(app: Shared) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/roots", get(roots).post(publish_root))
        .route("/v1/apps/{app}/channels/{channel}/bundle", get(bundle))
        .route("/v1/blobs/{hash}", get(get_blob))
        .route("/v1/blobs/{hash}", put(put_blob).layer(DefaultBodyLimit::max(MAX_BLOB_BYTES)))
        .route("/v1/manifests", post(publish_manifest))
        .with_state(app)
}

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, axum::Json(json!({"error": self.1}))).into_response()
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        match e {
            Error::NotFound(m) => Self(StatusCode::NOT_FOUND, m),
            Error::Conflict(m) => Self(StatusCode::CONFLICT, m),
            Error::Invalid(m) => Self(StatusCode::BAD_REQUEST, m),
            Error::Update(e) => Self(StatusCode::BAD_REQUEST, e.to_string()),
            e @ (Error::Db(_) | Error::Io(_)) => {
                eprintln!("salty-server: internal error: {e}");
                Self(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        }
    }
}

type ApiResult<T> = Result<T, ApiError>;

fn authorize(app: &App, headers: &HeaderMap) -> ApiResult<()> {
    let denied = || ApiError(StatusCode::UNAUTHORIZED, "a valid admin bearer token is required".into());
    let want = app.admin_token_hash.as_deref().ok_or_else(denied)?;
    let got = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).ok_or_else(denied)?;
    if weirdnet_accounts::token::hashes_equal(&weirdnet_accounts::token::token_hash(got), want) { Ok(()) } else { Err(denied()) }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> salty_core::Result<T> + Send + 'static) -> ApiResult<T> {
    tokio::task::spawn_blocking(f).await.map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into()))?.map_err(ApiError::from)
}

fn parse_hash(s: &str) -> ApiResult<Hash32> {
    Hash32::from_hex(s).map_err(|_| ApiError(StatusCode::BAD_REQUEST, "blob name must be a BLAKE3 hex hash".into()))
}

fn parse_env(body: &[u8]) -> ApiResult<Signed> {
    Signed::from_bytes(body).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))
}

async fn roots(State(app): State<Shared>) -> ApiResult<axum::Json<serde_json::Value>> {
    let roots = blocking(move || app.store.roots()).await?;
    let hex: Vec<String> = roots.iter().map(encode).collect::<salty_core::Result<_>>()?;
    Ok(axum::Json(json!({"roots": hex})))
}

async fn bundle(State(app): State<Shared>, Path((a, c)): Path<(String, String)>) -> ApiResult<axum::Json<BundleWire>> {
    let b = blocking(move || app.store.bundle(&a, &c)).await?;
    Ok(axum::Json(BundleWire::from_bundle(&b)?))
}

async fn get_blob(State(app): State<Shared>, Path(h): Path<String>) -> ApiResult<Response> {
    let hash = parse_hash(&h)?;
    let path = app.store.blob_path(&hash).ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("blob {h}")))?;
    let data = tokio::fs::read(path).await.map_err(|e| ApiError::from(Error::Io(e)))?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], data).into_response())
}

async fn put_blob(State(app): State<Shared>, headers: HeaderMap, Path(h): Path<String>, body: Bytes) -> ApiResult<StatusCode> {
    authorize(&app, &headers)?;
    let hash = parse_hash(&h)?;
    blocking(move || app.store.put_blob(&hash, &body)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn publish_root(State(app): State<Shared>, headers: HeaderMap, body: Bytes) -> ApiResult<axum::Json<serde_json::Value>> {
    authorize(&app, &headers)?;
    let env = parse_env(&body)?;
    let v = blocking(move || app.store.publish_root(&env, Timestamp::now())).await?;
    Ok(axum::Json(json!({"version": v})))
}

async fn publish_manifest(State(app): State<Shared>, headers: HeaderMap, body: Bytes) -> ApiResult<axum::Json<salty_core::Published>> {
    authorize(&app, &headers)?;
    let env = parse_env(&body)?;
    Ok(axum::Json(blocking(move || app.store.publish_manifest(&env, Timestamp::now())).await?))
}

/// Path of the admin token hash inside a server home.
pub fn token_file(home: &std::path::Path) -> std::path::PathBuf {
    home.join("admin-token.sha256")
}

/// Issues a new admin token: stores only its SHA-256 and returns the token once.
pub fn issue_admin_token(home: &std::path::Path) -> std::io::Result<String> {
    std::fs::create_dir_all(home)?;
    let token = format!("sg_admin_{}", weirdnet_accounts::token::random_token());
    std::fs::write(token_file(home), weirdnet_accounts::token::token_hash(&token))?;
    Ok(token)
}

/// Reads the stored admin token hash, if one was issued.
pub fn read_admin_token_hash(home: &std::path::Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(token_file(home)) {
        Ok(s) => Ok(Some(s.trim().to_owned())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
