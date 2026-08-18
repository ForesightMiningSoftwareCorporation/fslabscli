//! The one HTTP client for the release commands, over the repository's
//! established hyper-rustls stack (reqwest is a dev-dependency only, and the
//! release surface needs exactly plain requests: credential-less GET/HEAD of
//! the published objects, streamed downloads, and record's small JSON API
//! calls). One module means one TLS, redirect, and error behaviour to
//! review.

use anyhow::{Context, bail};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request};
use hyper_rustls::{ConfigBuilderExt, HttpsConnector};
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

pub(crate) type Client = HyperClient<HttpsConnector<HttpConnector>, Full<Bytes>>;

pub(crate) const MAX_REDIRECTS: usize = 5;

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
            bail!("GET {current} returned {}", res.status());
        }
        return Ok(res);
    }
    bail!("GET {url}: more than {MAX_REDIRECTS} redirects")
}

pub(crate) async fn get_bytes(client: &Client, url: &str) -> anyhow::Result<Vec<u8>> {
    let response = get_response(client, url).await?;
    Ok(response.into_body().collect().await?.to_bytes().to_vec())
}
