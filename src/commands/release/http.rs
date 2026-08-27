//! The one HTTP client for the release commands, over the repository's
//! established hyper-rustls stack (reqwest is a dev-dependency only, and the
//! release surface needs exactly plain requests: credential-less GET/HEAD of
//! the published objects, streamed downloads, and record's small JSON API
//! calls). One module means one TLS, redirect, and error behaviour to
//! review.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, bail};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request, StatusCode};
use hyper_rustls::{ConfigBuilderExt, HttpsConnector};
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use sha2::{Digest, Sha256};

pub(crate) type Client = HyperClient<HttpsConnector<HttpConnector>, Full<Bytes>>;

pub(crate) const MAX_REDIRECTS: usize = 5;

/// A non-2xx response, carrying the status so callers can branch on it.
///
/// Detect with `err.downcast_ref::<HttpStatus>()`. Callers that must tell "the
/// object is not there" from "we could not find out" need the code itself: a
/// substring match on the message silently changes meaning the next time the
/// wording moves, and the two answers are not equally safe to guess at.
#[derive(Debug, thiserror::Error)]
#[error("GET {url} returned {status}")]
pub(crate) struct HttpStatus {
    pub(crate) url: String,
    pub(crate) status: StatusCode,
}

pub(crate) fn client() -> anyhow::Result<Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let tls_config = rustls::ClientConfig::builder()
        .with_native_roots()
        .context("no native TLS roots available")?
        .with_no_client_auth();
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls_config)
        .https_or_http()
        .enable_http1()
        .build();
    Ok(HyperClient::builder(TokioExecutor::new()).build(https))
}

/// One request, no redirect handling: record's API calls, where a redirect
/// would be a misconfiguration, and callers that inspect the status
/// themselves.
pub(crate) async fn request(
    client: &Client,
    method: Method,
    url: &str,
    headers: &[(&str, String)],
    body: Vec<u8>,
) -> anyhow::Result<(StatusCode, Bytes)> {
    let mut builder = Request::builder().method(method).uri(url);
    for (name, value) in headers {
        builder = builder.header(*name, value);
    }
    let req = builder.body(Full::new(Bytes::from(body)))?;
    let res = client
        .request(req)
        .await
        .with_context(|| format!("request to {url} failed"))?;
    let status = res.status();
    let bytes = res
        .into_body()
        .collect()
        .await
        .with_context(|| format!("could not read the response body from {url}"))?
        .to_bytes();
    Ok((status, bytes))
}

/// GET following up to [`MAX_REDIRECTS`] redirects (GitHub release downloads
/// 302 to object storage), succeeding only on a 2xx.
async fn get_response(
    client: &Client,
    url: &str,
) -> anyhow::Result<hyper::Response<hyper::body::Incoming>> {
    let mut current = url::Url::parse(url).with_context(|| format!("invalid url {url}"))?;
    for _ in 0..=MAX_REDIRECTS {
        let req = Request::builder()
            .method(Method::GET)
            .uri(current.as_str())
            .body(Full::new(Bytes::new()))?;
        let res = client
            .request(req)
            .await
            .with_context(|| format!("GET {current} failed"))?;
        if res.status().is_redirection() {
            let location = res
                .headers()
                .get(hyper::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .with_context(|| format!("GET {current}: redirect without a Location header"))?;
            current = current
                .join(location)
                .with_context(|| format!("GET {current}: unusable redirect to {location}"))?;
            continue;
        }
        if !res.status().is_success() {
            return Err(HttpStatus {
                url: current.to_string(),
                status: res.status(),
            }
            .into());
        }
        return Ok(res);
    }
    bail!("GET {url}: more than {MAX_REDIRECTS} redirects")
}

pub(crate) async fn get_bytes(client: &Client, url: &str) -> anyhow::Result<Vec<u8>> {
    let response = get_response(client, url).await?;
    Ok(response.into_body().collect().await?.to_bytes().to_vec())
}

/// Stream a GET straight to `path`, returning the sha256 of the bytes as
/// written so verification covers the file on disk, not an in-memory copy.
pub(crate) async fn get_to_file(client: &Client, url: &str, path: &Path) -> anyhow::Result<String> {
    let response = get_response(client, url).await?;
    let mut body = response.into_body();
    let mut file =
        std::fs::File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
    let mut hasher = Sha256::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.with_context(|| format!("GET {url}: body stream failed"))?;
        if let Ok(data) = frame.into_data() {
            file.write_all(&data)?;
            hasher.update(&data);
        }
    }
    file.flush()?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// HEAD, where a redirect counts as present: the semantics of
/// `curl -fsSI` without `-L` (only >= 400 fails).
pub(crate) async fn head_present(client: &Client, url: &str) -> bool {
    let Ok(req) = Request::builder()
        .method(Method::HEAD)
        .uri(url)
        .body(Full::new(Bytes::new()))
    else {
        return false;
    };
    match client.request(req).await {
        Ok(res) => res.status().is_success() || res.status().is_redirection(),
        Err(_) => false,
    }
}
