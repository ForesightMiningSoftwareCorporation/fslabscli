//! Storage rules for release publication, in one place.
//!
//! Immutability and lost-update protection come from S3 conditional writes:
//! [`ReleaseStore::put_immutable`] refuses to overwrite via If-None-Match:*,
//! and [`ReleaseStore::cas_update`] is an ETag If-Match compare-and-swap
//! whose transform re-runs against the freshly-read document on every retry,
//! so validation embedded in the transform (for example the backward-move
//! gate) is never evaluated against a stale document. Object lock and
//! write-only credentials guard against deletion, but only a conditional
//! write prevents an overwrite (a re-PUT on a versioned bucket succeeds and
//! becomes the new current version), which is why `release probe-store` must
//! pass against the deployed store before publication may rely on this
//! module.

use anyhow::{Context, bail};
use opendal::{ErrorKind, Operator, services::S3};
use semver::Version;
use sha2::{Digest, Sha256};

pub const CAS_RETRIES: usize = 5;

/// A refused overwrite of an existing object: the caller must not retry the
/// same key. Detect with `err.downcast_ref::<AlreadyExists>()`.
#[derive(Debug, thiserror::Error)]
#[error("refusing to overwrite existing object {0}")]
pub struct AlreadyExists(pub String);

pub struct ReleaseStore {
    op: Operator,
    bucket: String,
}

/// Build an S3-backed operator for `bucket` from the environment:
/// `RELEASE_STORE_ENDPOINT` (optional), `AWS_ACCESS_KEY_ID`,
/// `AWS_SECRET_ACCESS_KEY`, `AWS_REGION` (default us-east-1).
pub fn operator_from_env(bucket: &str) -> anyhow::Result<Operator> {
    let mut builder = S3::default()
        .bucket(bucket)
        .region(&std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".into()));
    if let Ok(endpoint) = std::env::var("RELEASE_STORE_ENDPOINT") {
        builder = builder.endpoint(&endpoint);
    }
    if let Ok(key) = std::env::var("AWS_ACCESS_KEY_ID") {
        builder = builder.access_key_id(&key);
    }
    if let Ok(secret) = std::env::var("AWS_SECRET_ACCESS_KEY") {
        builder = builder.secret_access_key(&secret);
    }
    Ok(Operator::new(builder)?.finish())
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// Strict semver "greater than", with real prerelease precedence: 1.0.0 >
/// 1.0.0-rc.1, and 1.10.0 > 1.9.9. (`sort -V` and string comparison both get
/// these wrong, which is why this lives here and nowhere else.)
pub fn semver_gt(a: &str, b: &str) -> anyhow::Result<bool> {
    let a = Version::parse(a).with_context(|| format!("unparseable version: {a}"))?;
    let b = Version::parse(b).with_context(|| format!("unparseable version: {b}"))?;
    Ok(a > b)
}

impl ReleaseStore {
    pub fn new(op: Operator, bucket: impl Into<String>) -> Self {
        Self {
            op,
            bucket: bucket.into(),
        }
    }

    pub fn from_env(bucket: &str) -> anyhow::Result<Self> {
        Ok(Self::new(operator_from_env(bucket)?, bucket))
    }

    pub async fn read(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        Ok(self
            .op
            .read(key)
            .await
            .with_context(|| format!("cannot read s3://{}/{key}", self.bucket))?
            .to_vec())
    }

    pub async fn exists(&self, key: &str) -> anyhow::Result<bool> {
        Ok(self.op.exists(key).await?)
    }

    /// Write a key that must not already exist. An existing key returns
    /// [`AlreadyExists`]; a shipped artifact is never overwritten.
    pub async fn put_immutable(&self, key: &str, data: Vec<u8>) -> anyhow::Result<()> {
        // opendal expresses S3 If-None-Match:* as if_not_exists.
        match self.op.write_with(key, data).if_not_exists(true).await {
            Ok(_) => Ok(()),
            Err(e)
                if e.kind() == ErrorKind::ConditionNotMatch
                    || e.kind() == ErrorKind::AlreadyExists =>
            {
                Err(AlreadyExists(format!("s3://{}/{key}", self.bucket)).into())
            }
            Err(e) => Err(e).with_context(|| format!("put-immutable s3://{}/{key}", self.bucket)),
        }
    }

    /// Read-modify-write under If-Match, retrying with a fresh read when the
    /// precondition fails. `transform` sees the CURRENT document (or
    /// `initial` when the key does not exist yet) on every attempt; an error
    /// from the transform aborts the whole update and propagates.
    pub async fn cas_update<T, F>(
        &self,
        key: &str,
        initial: Option<T>,
        mut transform: F,
    ) -> anyhow::Result<T>
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
        F: FnMut(T) -> anyhow::Result<T>,
    {
        for attempt in 1..=CAS_RETRIES {
            let (current, etag): (T, Option<String>) = match self.op.stat(key).await {
                Ok(meta) => {
                    let etag = meta.etag().map(str::to_string);
                    let bytes = self.read(key).await?;
                    (
                        serde_json::from_slice(&bytes).with_context(|| {
                            format!("s3://{}/{key} is not valid JSON", self.bucket)
                        })?,
                        etag,
                    )
                }
                Err(e) if e.kind() == ErrorKind::NotFound => match &initial {
                    Some(init) => (serde_json::from_value(serde_json::to_value(init)?)?, None),
                    None => bail!(
                        "cas-update: s3://{}/{key} does not exist and no initial document was given",
                        self.bucket
                    ),
                },
                Err(e) => return Err(e).context("stat failed"),
            };

            let next = transform(current)?;
            let body = serde_json::to_vec_pretty(&next)?;
            let write = match etag {
                Some(ref etag) => self.op.write_with(key, body).if_match(etag).await,
                None => self.op.write_with(key, body).if_not_exists(true).await,
            };
            match write {
                Ok(_) => return Ok(next),
                Err(e)
                    if e.kind() == ErrorKind::ConditionNotMatch
                        || e.kind() == ErrorKind::AlreadyExists =>
                {
                    tracing::warn!(
                        "cas-update: lost the race on s3://{}/{key} (attempt {attempt}/{CAS_RETRIES}), re-reading",
                        self.bucket
                    );
                    continue;
                }
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("cas-update s3://{}/{key}", self.bucket));
                }
            }
        }
        bail!(
            "cas-update: gave up on s3://{}/{key} after {CAS_RETRIES} attempts",
            self.bucket
        )
    }

    /// Fail unless `version` is strictly greater than every version that has
    /// a COMMITTED MANIFEST under `<app>/`. The listed manifests are the
    /// authority, never index.json: an index update can fail after a manifest
    /// landed, and a check that trusted the index would then admit a lower
    /// version forever. A version prefix with artifacts but no manifest is
    /// invisible, because the manifest is the commit point.
    pub async fn assert_monotonic(&self, app: &str, version: &str) -> anyhow::Result<Vec<String>> {
        // Parse the candidate BEFORE the comparison loop. Otherwise the very
        // first publication of an app skips validation entirely (the loop body
        // never runs), so an unparseable version commits once and then refuses
        // every subsequent production publish forever, because `semver_gt`
        // errors on the stored value.
        Version::parse(version).with_context(|| format!("unparseable version: {version}"))?;
        let mut published = Vec::new();
        let entries = self
            .op
            .list_with(&format!("{app}/"))
            .recursive(true)
            .await
            .with_context(|| format!("cannot list s3://{}/{app}/", self.bucket))?;
        for entry in entries {
            let path = entry.path();
            if let Some(v) = path
                .strip_prefix(&format!("{app}/"))
                .and_then(|rest| rest.strip_suffix("/manifest.json"))
                && !v.contains('/')
            {
                published.push(v.to_string());
            }
        }
        for existing in &published {
            if !semver_gt(version, existing)? {
                bail!(
                    "version {version} is not strictly greater than published {existing} for {app}; \
                     production versions increase strictly and a failed publication's number is spent"
                );
            }
        }
        Ok(published)
    }

    /// Attempt a write with a deliberately stale ETag. Returns `Ok(true)` when
    /// the store REFUSED it, which is the behaviour publication depends on.
    ///
    /// Exists solely for `probe-store`: the positive CAS path cannot detect a
    /// store that ignores `If-Match` on PUT, because ignoring the precondition
    /// still produces a successful update. Without this, the probe reports
    /// conditional writes as supported while lost-update protection on
    /// index.json and channels.json is silently absent.
    pub async fn refuses_stale_if_match(
        &self,
        key: &str,
        stale_etag: &str,
        data: Vec<u8>,
    ) -> anyhow::Result<bool> {
        match self.op.write_with(key, data).if_match(stale_etag).await {
            Ok(_) => Ok(false),
            Err(e) if e.kind() == ErrorKind::ConditionNotMatch => Ok(true),
            Err(e) => Err(e)
                .with_context(|| format!("stale-if-match probe on s3://{}/{key}", self.bucket)),
        }
    }

    /// Re-read one object and compare its digest. Used between the artifact
    /// writes and the manifest write, before any manifest exists.
    pub async fn verify_key(&self, key: &str, want_sha256: &str) -> anyhow::Result<()> {
        let data = self.read(key).await?;
        let got = sha256_hex(&data);
        if got != want_sha256 {
            bail!("digest mismatch for {key}: wanted {want_sha256}, object has {got}");
        }
        Ok(())
    }

    /// Turn a manifest artifact URL back into this bucket's object key.
    pub fn key_from_url<'a>(&self, url: &'a str) -> anyhow::Result<&'a str> {
        let marker = format!("/{}/", self.bucket);
        // Last occurrence, not first: a public base URL that itself contains the
        // bucket name yields a doubled marker, and taking the leftmost match
        // returns a key with the bucket prefixed. That surfaces as a read-back
        // failure at publish time, AFTER every artifact is already immutable.
        match url.rfind(&marker) {
            Some(idx) => Ok(&url[idx + marker.len()..]),
            None => bail!(
                "artifact url {url} does not reference bucket {}",
                self.bucket
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(bucket: &str) -> ReleaseStore {
        // No I/O: key_from_url is pure string work over the bucket name.
        ReleaseStore::new(
            Operator::new(opendal::services::Memory::default())
                .unwrap()
                .finish(),
            bucket,
        )
    }

    #[test]
    fn key_from_url_takes_the_last_bucket_segment() {
        let s = store("fsl-releases");
        assert_eq!(
            s.key_from_url("https://api.s3.fsl.dev/fsl-releases/app/1.0.0/a.deb")
                .unwrap(),
            "app/1.0.0/a.deb"
        );
        // A public base URL that itself contains the bucket name. Taking the
        // FIRST match returned "fsl-releases/app/1.0.0/a.deb", which only
        // surfaced as a read-back failure after every artifact was immutable.
        assert_eq!(
            s.key_from_url("https://api.s3.fsl.dev/fsl-releases/fsl-releases/app/1.0.0/a.deb")
                .unwrap(),
            "app/1.0.0/a.deb"
        );
        assert!(
            s.key_from_url("https://api.s3.fsl.dev/other/app/a.deb")
                .is_err()
        );
    }

    #[test]
    fn semver_ordering_is_real_precedence() {
        assert!(semver_gt("1.10.0", "1.9.9").unwrap());
        assert!(semver_gt("1.0.0", "1.0.0-rc.1").unwrap());
        assert!(!semver_gt("1.0.0-rc.1", "1.0.0").unwrap());
        assert!(!semver_gt("1.0.0", "1.0.0").unwrap());
        assert!(semver_gt("1.0.0-rc.2", "1.0.0-rc.1").unwrap());
        assert!(semver_gt("x.y.z", "1.0.0").is_err());
    }
}
