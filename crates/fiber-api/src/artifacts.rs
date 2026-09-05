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
            tokio::fs::create_dir_all(artifacts_dir).await.ok();
            info!(%artifacts_dir, "artifact backend: local filesystem");
            return Ok(Self::Local {
                root: PathBuf::from(artifacts_dir),
            });
        };

        let endpoint =
            std::env::var("FIBER_S3_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:19000".into());
        let public_endpoint = std::env::var("FIBER_S3_PUBLIC_ENDPOINT")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| endpoint.clone());
        let region = std::env::var("FIBER_S3_REGION").unwrap_or_else(|_| "us-east-1".into());
        let access = std::env::var("FIBER_S3_ACCESS_KEY").unwrap_or_else(|_| "fiber".into());
        let secret = std::env::var("FIBER_S3_SECRET_KEY").unwrap_or_else(|_| "fiberfiber".into());

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
    pub async fn presign_put(&self, key: &str, secs: u64) -> Result<Option<String>> {
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
                    .presigned(conf)
                    .await
                    .context("presign put")?;
                Ok(Some(req.uri().to_string()))
            }
        }
    }
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
