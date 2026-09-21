//! Artifact blob storage: local filesystem (default) or S3-compatible (MinIO).

use anyhow::{Context, Result, anyhow};
use aws_credential_types::Credentials;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::{BehaviorVersion, Region};
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::info;

#[derive(Clone)]
pub enum ArtifactBackend {
    Local {
        root: PathBuf,
    },
    S3 {
        client: Client,
        /// Client whose endpoint matches what agents/browsers reach (SigV4 Host).
        presign_client: Client,
        bucket: String,
    },
}

impl ArtifactBackend {
    /// Build from env. If `FIBER_S3_BUCKET` is set, use S3/MinIO; else local dir.
    pub async fn from_env(artifacts_dir: &str) -> Result<Self> {
        let bucket = std::env::var("FIBER_S3_BUCKET")
            .ok()
            .filter(|s| !s.is_empty());
        let Some(bucket) = bucket else {
            let root = PathBuf::from(artifacts_dir);
            ensure_writable(&root).await?;
            info!(%artifacts_dir, "artifact backend: local filesystem");
            return Ok(Self::Local { root });
        };

        let endpoint =
            std::env::var("FIBER_S3_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:19000".into());
        let public_endpoint = std::env::var("FIBER_S3_PUBLIC_ENDPOINT")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| endpoint.clone());
        let region = std::env::var("FIBER_S3_REGION").unwrap_or_else(|_| "us-east-1".into());
        // No silent fallback to the MinIO dev credentials: a bucket without keys is a misconfiguration.
        let access = std::env::var("FIBER_S3_ACCESS_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .context("FIBER_S3_ACCESS_KEY is required when FIBER_S3_BUCKET is set")?;
        let secret = std::env::var("FIBER_S3_SECRET_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .context("FIBER_S3_SECRET_KEY is required when FIBER_S3_BUCKET is set")?;

        let client = s3_client(&endpoint, &region, &access, &secret);
        let presign_client = if public_endpoint == endpoint {
            client.clone()
        } else {
            info!(%public_endpoint, "artifact presign endpoint (public)");
            s3_client(&public_endpoint, &region, &access, &secret)
        };

        // Ensure bucket exists (idempotent).
        match client.head_bucket().bucket(&bucket).send().await {
            Ok(_) => {}
            Err(_) => {
                client
                    .create_bucket()
                    .bucket(&bucket)
                    .send()
                    .await
                    .context("create artifact bucket")?;
                info!(%bucket, "created S3 artifact bucket");
            }
        }

        info!(%bucket, "artifact backend: s3");
        Ok(Self::S3 {
            client,
            presign_client,
            bucket,
        })
    }
}

/// Prove the local artifact root is writable before the API reports itself ready.
///
/// `create_dir_all` succeeds on a directory that already exists whatever it is owned by,
/// so an install whose artifact volume was created by the old root image boots green
/// under uid 10001 and only finds out on the first upload — and the WebSocket upload
/// path used to swallow that. One create-and-unlink at boot turns a silent, permanent
/// loss of every artifact into a refusal to start, with the command that fixes it.
async fn ensure_writable(root: &Path) -> Result<()> {
    tokio::fs::create_dir_all(root)
        .await
        .map_err(|e| unwritable(root, "create", &e))?;
    let probe = root.join(format!(".fiber-write-probe-{}", uuid::Uuid::new_v4()));
    tokio::fs::write(&probe, b"")
        .await
        .map_err(|e| unwritable(root, "write to", &e))?;
    // Best effort: a probe file left by a crash between these two calls is harmless, and
    // the next boot writes its own.
    let _ = tokio::fs::remove_file(&probe).await;
    Ok(())
}

/// The one line an operator gets when the artifact directory is not theirs to write.
/// It has to carry the fix, because the reason is invisible from inside the container:
/// the directory is there and looks fine, it just belongs to uid 0.
fn unwritable(root: &Path, verb: &str, err: &std::io::Error) -> anyhow::Error {
    anyhow!(
        "cannot {verb} the artifact directory {} ({err}). fiber-api runs as uid 10001, \
         and a directory created by an older root image has to be handed over once: \
         `chown -R 10001:10001 <dir>` on the host, or for Compose `docker run --rm -v \
         fiber_fiber_artifacts:/data/artifacts alpine chown -R 10001:10001 \
         /data/artifacts`. See docs/operations.md#upgrades. Alternatively point \
         FIBER_ARTIFACTS_DIR somewhere writable, or set FIBER_S3_BUCKET to store \
         artifacts in object storage instead.",
        root.display()
    )
}

impl ArtifactBackend {
    pub fn object_key(run_id: &str, step_run_id: &str, name: &str) -> String {
        format!("artifacts/{run_id}/{step_run_id}/{name}")
    }

    pub fn stored_path_for_key(&self, key: &str) -> String {
        match self {
            Self::Local { root } => root.join(key).to_string_lossy().to_string(),
            Self::S3 { bucket, .. } => format!("s3://{bucket}/{key}"),
        }
    }

    pub async fn put(&self, key: &str, bytes: &[u8]) -> Result<String> {
        match self {
            Self::Local { root } => {
                let path = root.join(key);
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(&path, bytes).await?;
                Ok(path.to_string_lossy().to_string())
            }
            Self::S3 { client, bucket, .. } => {
                client
                    .put_object()
                    .bucket(bucket)
                    .key(key)
                    .body(ByteStream::from(bytes.to_vec()))
                    .send()
                    .await
                    .context("s3 put_object")?;
                Ok(format!("s3://{bucket}/{key}"))
            }
        }
    }

    pub async fn get_bytes(&self, stored_path: &str) -> Result<Vec<u8>> {
        match self {
            Self::Local { .. } => tokio::fs::read(stored_path)
                .await
                .with_context(|| format!("read artifact {stored_path}")),
            Self::S3 { client, bucket, .. } => {
                let key = s3_key_from_stored(stored_path, bucket)?;
                let out = client
                    .get_object()
                    .bucket(bucket)
                    .key(&key)
                    .send()
                    .await
                    .context("s3 get_object")?;
                let data = out.body.collect().await?.into_bytes().to_vec();
                Ok(data)
            }
        }
    }

    /// Object size when present (S3 HEAD); local uses filesystem metadata.
    pub async fn object_size(&self, stored_path: &str) -> Result<Option<u64>> {
        match self {
            Self::Local { .. } => {
                let meta = tokio::fs::metadata(stored_path).await.ok();
                Ok(meta.map(|m| m.len()))
            }
            Self::S3 { client, bucket, .. } => {
                let key = s3_key_from_stored(stored_path, bucket)?;
                match client.head_object().bucket(bucket).key(key).send().await {
                    Ok(out) => Ok(Some(out.content_length().unwrap_or(0) as u64)),
                    Err(_) => Ok(None),
                }
            }
        }
    }

    /// Best-effort delete of a stored blob (missing objects are ok).
    pub async fn delete(&self, stored_path: &str) -> Result<()> {
        match self {
            Self::Local { .. } => match tokio::fs::remove_file(stored_path).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            },
            Self::S3 { client, bucket, .. } => {
                let key = s3_key_from_stored(stored_path, bucket)?;
                client
                    .delete_object()
                    .bucket(bucket)
                    .key(key)
                    .send()
                    .await
                    .context("s3 delete_object")?;
                Ok(())
            }
        }
    }

    /// Presigned GET when using S3; `None` for local (caller streams bytes).
    pub async fn presign_get(&self, stored_path: &str, secs: u64) -> Result<Option<String>> {
        match self {
            Self::Local { .. } => Ok(None),
            Self::S3 {
                presign_client,
                bucket,
                ..
            } => {
                let key = s3_key_from_stored(stored_path, bucket)?;
                let conf = PresigningConfig::expires_in(Duration::from_secs(secs))
                    .map_err(|e| anyhow!("presign config: {e}"))?;
                let req = presign_client
                    .get_object()
                    .bucket(bucket)
                    .key(key)
                    .presigned(conf)
                    .await
                    .context("presign get")?;
                Ok(Some(req.uri().to_string()))
            }
        }
    }

    /// Presigned PUT for direct agent upload; `None` for local (use API proxy).
    ///
    /// `content_length` is signed, so the URL is only usable for an object of exactly
    /// that size: without it the URL was a licence to write any number of bytes into the
    /// bucket, and the size check on `complete` came far too late to stop them arriving.
    pub async fn presign_put(
        &self,
        key: &str,
        secs: u64,
        content_length: i64,
    ) -> Result<Option<String>> {
        match self {
            Self::Local { .. } => Ok(None),
            Self::S3 {
                presign_client,
                bucket,
                ..
            } => {
                let conf = PresigningConfig::expires_in(Duration::from_secs(secs))
                    .map_err(|e| anyhow!("presign config: {e}"))?;
                let req = presign_client
                    .put_object()
                    .bucket(bucket)
                    .key(key)
                    .content_length(content_length)
                    .presigned(conf)
                    .await
                    .context("presign put")?;
                Ok(Some(req.uri().to_string()))
            }
        }
    }

    /// Stored paths under the artifact prefix last modified before `cutoff`, up to `limit`.
    ///
    /// Retention follows rows; an object whose row was never written — a presigned upload
    /// the agent never completed, or one `complete` refused — is invisible to it and stays
    /// in the bucket for ever. This is the other half: the objects, so the caller can ask
    /// which of them nothing points at. Only the `artifacts/{run}/{step}/{name}` layout
    /// this process writes is listed.
    pub async fn list_stale_objects(
        &self,
        cutoff: std::time::SystemTime,
        limit: usize,
    ) -> Result<Vec<String>> {
        match self {
            Self::Local { root } => {
                let mut out = Vec::new();
                let base = root.join("artifacts");
                let mut dirs = vec![base.clone()];
                while let Some(dir) = dirs.pop() {
                    let Ok(mut rd) = tokio::fs::read_dir(&dir).await else {
                        continue;
                    };
                    while let Ok(Some(entry)) = rd.next_entry().await {
                        if out.len() >= limit {
                            return Ok(out);
                        }
                        let Ok(meta) = entry.metadata().await else {
                            continue;
                        };
                        if meta.is_dir() {
                            dirs.push(entry.path());
                            continue;
                        }
                        if !meta.modified().is_ok_and(|m| m < cutoff) {
                            continue;
                        }
                        let path = entry.path();
                        let Ok(rel) = path.strip_prefix(root) else {
                            continue;
                        };
                        if !is_artifact_object_key(&rel.to_string_lossy()) {
                            continue;
                        }
                        out.push(path.to_string_lossy().to_string());
                    }
                }
                Ok(out)
            }
            Self::S3 { client, bucket, .. } => {
                let mut out = Vec::new();
                let mut token: Option<String> = None;
                loop {
                    let mut req = client
                        .list_objects_v2()
                        .bucket(bucket)
                        .prefix("artifacts/")
                        .max_keys(1000);
                    if let Some(t) = token {
                        req = req.continuation_token(t);
                    }
                    let page = req.send().await.context("s3 list_objects_v2")?;
                    for obj in page.contents() {
                        let Some(key) = obj.key() else { continue };
                        if !is_artifact_object_key(key) {
                            continue;
                        }
                        let old = obj
                            .last_modified()
                            .and_then(|t| std::time::SystemTime::try_from(*t).ok())
                            .is_some_and(|t| t < cutoff);
                        if !old {
                            continue;
                        }
                        out.push(format!("s3://{bucket}/{key}"));
                        if out.len() >= limit {
                            return Ok(out);
                        }
                    }
                    token = page.next_continuation_token().map(str::to_string);
                    if token.is_none() {
                        break;
                    }
                }
                Ok(out)
            }
        }
    }
}

/// Whether a key is one this process wrote: `artifacts/{run uuid}/{step uuid}/{name}`.
///
/// The sweep deletes what it lists, so it only ever considers the exact layout
/// [`ArtifactBackend::object_key`] produces. Anything else in the bucket — another tool's
/// data, an operator's own upload — is none of its business.
pub(crate) fn is_artifact_object_key(key: &str) -> bool {
    let mut parts = key.split('/');
    if parts.next() != Some("artifacts") {
        return false;
    }
    let (Some(run), Some(step), Some(name)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    parts.next().is_none()
        && !name.is_empty()
        && uuid::Uuid::parse_str(run).is_ok()
        && uuid::Uuid::parse_str(step).is_ok()
}

fn s3_client(endpoint: &str, region: &str, access: &str, secret: &str) -> Client {
    let creds = Credentials::new(access, secret, None, None, "fiber");
    let conf = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(region.to_string()))
        .endpoint_url(endpoint)
        .credentials_provider(creds)
        .force_path_style(true)
        .build();
    Client::from_conf(conf)
}

fn s3_key_from_stored(stored: &str, bucket: &str) -> Result<String> {
    let prefix = format!("s3://{bucket}/");
    if let Some(key) = stored.strip_prefix(&prefix) {
        return Ok(key.to_string());
    }
    // Also accept raw keys.
    if stored.starts_with("artifacts/") {
        return Ok(stored.to_string());
    }
    // Legacy local path under a known layout: .../run/step/name
    let p = Path::new(stored);
    let name = p
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("bad artifact path"))?;
    let step = p
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("bad artifact path"))?;
    let run = p
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("bad artifact path"))?;
    Ok(format!("artifacts/{run}/{step}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "fiber-artifacts-test-{name}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    #[tokio::test]
    async fn a_writable_root_is_created_and_left_clean() {
        let dir = scratch("writable");
        ensure_writable(&dir).await.expect("fresh dir is writable");
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "probe file left behind: {left:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_unwritable_root_refuses_with_the_command_that_fixes_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("unwritable");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Mode bits do not stop uid 0, which is how this suite runs in some containers.
        let enforced = std::fs::write(dir.join("root-check"), b"").is_err();
        let err = ensure_writable(&dir).await.err();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        if !enforced {
            return;
        }
        let msg = err
            .expect("a root-owned directory must fail the probe")
            .to_string();
        assert!(msg.contains("chown -R 10001:10001"), "{msg}");
        assert!(msg.contains(&dir.display().to_string()), "{msg}");
    }

    fn s3_backend(bucket: &str) -> ArtifactBackend {
        // Building a client performs no I/O, so the key helpers are testable offline.
        let client = s3_client("http://127.0.0.1:19000", "us-east-1", "ak", "sk");
        ArtifactBackend::S3 {
            presign_client: client.clone(),
            client,
            bucket: bucket.to_string(),
        }
    }

    #[tokio::test]
    async fn a_presigned_put_is_bound_to_the_size_it_was_issued_for() {
        // Without a signed content-length the URL is a licence to write any number of
        // bytes into the bucket for ten minutes, and the size check on `complete`
        // happens after they have all arrived.
        let url = s3_backend("fiber")
            .presign_put("artifacts/r/s/a.txt", 600, 1234)
            .await
            .unwrap()
            .expect("s3 backend presigns");
        let lower = url.to_ascii_lowercase();
        assert!(
            lower.contains("content-length"),
            "content-length is not a signed header: {url}"
        );
    }

    #[tokio::test]
    async fn the_local_sweep_lists_only_old_objects_of_our_own_layout() {
        let root = scratch("sweep");
        let key = ArtifactBackend::object_key(
            &uuid::Uuid::new_v4().to_string(),
            &uuid::Uuid::new_v4().to_string(),
            "dist.tgz",
        );
        let backend = ArtifactBackend::Local { root: root.clone() };
        let stored = backend.put(&key, b"bytes").await.unwrap();
        // Something else living in the same directory must be left alone.
        std::fs::create_dir_all(root.join("artifacts/not-a-uuid")).unwrap();
        std::fs::write(root.join("artifacts/not-a-uuid/keepme"), b"x").unwrap();

        let fresh = backend
            .list_stale_objects(
                std::time::SystemTime::now() - Duration::from_secs(3600),
                100,
            )
            .await
            .unwrap();
        assert!(
            fresh.is_empty(),
            "an object written seconds ago is still in flight: {fresh:?}"
        );

        let stale = backend
            .list_stale_objects(std::time::SystemTime::now() + Duration::from_secs(600), 100)
            .await
            .unwrap();
        assert_eq!(
            stale,
            vec![stored],
            "only this process's own layout is swept"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn object_key_is_run_then_step_then_leaf() {
        assert_eq!(
            ArtifactBackend::object_key("run1", "step1", "dist.tgz"),
            "artifacts/run1/step1/dist.tgz"
        );
    }

    #[test]
    fn stored_path_reflects_the_backend() {
        let key = ArtifactBackend::object_key("r", "s", "a.txt");
        let local = ArtifactBackend::Local {
            root: PathBuf::from("/var/fiber/data"),
        };
        assert_eq!(
            local.stored_path_for_key(&key),
            "/var/fiber/data/artifacts/r/s/a.txt"
        );
        assert_eq!(
            s3_backend("fiber").stored_path_for_key(&key),
            "s3://fiber/artifacts/r/s/a.txt"
        );
    }

    #[test]
    fn an_s3_url_round_trips_back_to_its_key() {
        let key = ArtifactBackend::object_key("r", "s", "a.txt");
        let stored = s3_backend("fiber").stored_path_for_key(&key);
        assert_eq!(s3_key_from_stored(&stored, "fiber").unwrap(), key);
    }

    #[test]
    fn a_raw_key_is_accepted_unchanged() {
        assert_eq!(
            s3_key_from_stored("artifacts/r/s/a.txt", "fiber").unwrap(),
            "artifacts/r/s/a.txt"
        );
    }

    #[test]
    fn a_legacy_local_path_is_rebuilt_from_its_last_three_segments() {
        // Rows written before the S3 backend store an absolute filesystem path.
        assert_eq!(
            s3_key_from_stored("/var/fiber/data/artifacts/r/s/a.txt", "fiber").unwrap(),
            "artifacts/r/s/a.txt"
        );
    }

    #[test]
    fn a_url_for_another_bucket_is_not_silently_reused() {
        // Falling through to the legacy branch would hand back a key that resolves
        // inside *our* bucket; only the trailing three segments are kept, so the
        // mismatch must not read as a valid s3:// URL for this bucket.
        let other = "s3://someone-elses/artifacts/r/s/a.txt";
        assert_eq!(
            s3_key_from_stored(other, "fiber").unwrap(),
            "artifacts/r/s/a.txt",
            "legacy fallback keeps run/step/name and drops the foreign bucket"
        );
    }

    #[test]
    fn a_path_too_shallow_to_carry_run_and_step_is_refused() {
        assert!(s3_key_from_stored("a.txt", "fiber").is_err());
        assert!(s3_key_from_stored("s/a.txt", "fiber").is_err());
        assert!(s3_key_from_stored("", "fiber").is_err());
    }
}
