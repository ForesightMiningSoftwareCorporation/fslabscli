use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Lines};
use std::{env, fs};

use anyhow::Context;
use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper::{Method, Request, Uri};
use hyper_rustls::{ConfigBuilderExt, HttpsConnector};
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde::{Deserialize, Serialize};

const NPM_DEFAULT_API_URL: &str = "https://registry.npmjs.org/";

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct PackageMetadataFslabsCiPublishNpmNapi {
    pub publish: bool,
    pub scope: Option<String>,
    #[serde(skip)]
    pub error: Option<String>,
}

impl PackageMetadataFslabsCiPublishNpmNapi {
    pub async fn check(
        &mut self,
        package: String,
        version: String,
        npm: &Npm,
    ) -> anyhow::Result<()> {
        if !self.publish {
            return Ok(());
        }
        let npm_package_prefix = match self.scope.clone() {
            Some(s) => format!("@{s}/"),
            None => "".to_string(),
        };
        let package_name = format!("{}{}", npm_package_prefix, package.clone());
        tracing::debug!(
            "NPM: checking if version {} of {} already exists",
            version,
            package_name
        );
        self.publish = !npm.check_npm_package_exists(package_name, version).await?;
        Ok(())
    }
}

#[derive(Debug)]
struct NpmRegistry {
    auth_token: Option<String>,
    url: String,
}

#[derive(Debug)]
struct NpmRCConfig {
    registries: HashMap<String, NpmRegistry>,
    scopes: HashMap<String, String>,
}

fn read_lines(filename: String) -> anyhow::Result<Lines<BufReader<File>>> {
    let file = File::open(filename)?;
    Ok(BufReader::new(file).lines())
}

impl NpmRCConfig {
    pub fn new(
        url: Option<String>,
        token: Option<String>,
        npmrc_path: Option<String>,
        tls: bool,
    ) -> Self {
        let mut registries = HashMap::new();
        let registry_url = url.unwrap_or_else(|| NPM_DEFAULT_API_URL.to_string());
        registries.insert(
            "default".to_string(),
            NpmRegistry {
                url: registry_url,
                auth_token: token,
            },
        );
        let mut config = Self {
            registries,
            scopes: HashMap::new(),
        };

        let npmrc_path = if let Some(p) = npmrc_path {
            p
        } else {
            let home_dir = env::var("HOME").unwrap_or_else(|_| "~".to_string());
            format!("{home_dir}/.npmrc")
        };

        if fs::metadata(npmrc_path.clone()).is_err() {
            return config;
        }

        if let Ok(lines) = read_lines(npmrc_path.clone()) {
            for line in lines.map_while(Result::ok) {
                // Registry
                let token_value: Vec<&str> = line.split(":_authToken=").collect();
                if token_value.len() == 2 {
                    let registry_name = token_value[0].to_string().split_off(2);
                    let protocol = match tls {
                        true => "https",
                        false => "http",
                    };
                    config.registries.insert(
                        registry_name,
                        NpmRegistry {
                            auth_token: Some(token_value[1].to_string()),
                            url: format!("{}:{}", protocol, token_value[0]),
                        },
                    );
                    continue;
                }

                let registry_value: Vec<&str> = line.split(":registry=https://").collect();
                if registry_value.len() == 2 {
                    config
                        .scopes
                        .insert(registry_value[0].to_string(), registry_value[1].to_string());
                }
            }
        }
        config
    }
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct NpmPackageVersion {
    pub name: String,
    pub version: String,
    pub deprecated: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct NpmPackage {
    versions: HashMap<String, NpmPackageVersion>,
}

pub struct Npm {
    rc_config: NpmRCConfig,
    client: HyperClient<HttpsConnector<HttpConnector>, Empty<Bytes>>,
}

impl Npm {
    pub fn new(
        registry_url: Option<String>,
        registry_token: Option<String>,
        npmrc_path: Option<String>,
        tls: bool,
    ) -> anyhow::Result<Self> {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(
                rustls::ClientConfig::builder()
                    .with_native_roots()?
                    .with_no_client_auth(),
            )
            .https_or_http()
            .enable_http1()
            .build();

        Ok(Self {
            rc_config: NpmRCConfig::new(registry_url, registry_token, npmrc_path, tls),
            client: HyperClient::builder(TokioExecutor::new()).build(https),
        })
    }

    pub async fn check_npm_package_exists(
        &self,
        package: String,
        version: String,
    ) -> anyhow::Result<bool> {
        // Infer registry if scoped
        let registry: Option<&NpmRegistry> = if package.starts_with('@') {
            if let Some((scope, _)) = package.clone().split_once('/') {
                if let Some(scoped_registry) = self.rc_config.scopes.get(scope) {
                    self.rc_config.registries.get(scoped_registry)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        // If no registry, should use default
        let registry = match registry {
            Some(s) => s,
            None => {
                if let Some(default_reg) = self.rc_config.registries.get("default") {
                    default_reg
                } else {
                    anyhow::bail!("Could not infer npm default registry")
                }
            }
        };

        let url: Uri = format!("{}{}", registry.url, package).parse()?;

        let mut req_builder = Request::builder().method(Method::GET).uri(url);

        if let Some(token) = &registry.auth_token {
            req_builder = req_builder.header("Authorization", format!("Bearer {token}"));
        }

        let req = req_builder.body(Empty::default())?;
        let res = self
            .client
            .request(req)
            .await
            .with_context(|| "Could not fetch from the npm registry")?;

        if res.status().as_u16() >= 400 {
            anyhow::bail!("Something went wrong while getting npm api data");
        }

        let body = res
            .into_body()
            .collect()
            .await
            .with_context(|| "Could not get body from the npm registry")?
            .to_bytes();

        let package: NpmPackage = serde_json::from_str(String::from_utf8_lossy(&body).as_ref())?;
        for (_, package_version) in package.versions {
            if package_version.version == version {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

// #[cfg(test)]
// mod tests {
//     use std::fs::File;
//     use std::io::Write;
//
//     use assert_fs::TempDir;
//     use wiremock::matchers::bearer_token;
//     use wiremock::{
//         matchers::{method, path},
//         Mock, MockServer, ResponseTemplate,
//     };
//
//     use super::*;
//
//     const NPM_EXISTING_PACKAGE_DATA: &str = "{\"_id\":\"axios\",\"_rev\":\"779-b37ceeb27a03858a89a0226f7c554aaf\",\"name\":\"axios\",\"description\":\"Promise based HTTP client for the browser and node.js\",\"dist-tags\":{\"latest\":\"0.1.0\",\"next\":\"0.2.0\"},\"versions\":{\"0.1.0\":{\"name\":\"axios\",\"version\":\"0.1.0\",\"description\":\"Promise based XHR library\",\"main\":\"index.js\",\"scripts\":{\"test\":\"grunt test\",\"start\":\"node ./sandbox/index.js\"},\"repository\":{\"type\":\"git\",\"url\":\"https://github.com/mzabriskie/axios.git\"},\"keywords\":[\"xhr\",\"http\",\"ajax\",\"promise\"],\"author\":{\"name\":\"Matt Zabriskie\"},\"license\":\"MIT\",\"bugs\":{\"url\":\"https://github.com/mzabriskie/axios/issues\"},\"homepage\":\"https://github.com/mzabriskie/axios\",\"dependencies\":{\"es6-promise\":\"^1.0.0\"},\"devDependencies\":{\"grunt\":\"^0.4.5\",\"grunt-contrib-clean\":\"^0.6.0\",\"grunt-contrib-watch\":\"^0.6.1\",\"webpack\":\"^1.3.3-beta2\",\"webpack-dev-server\":\"^1.4.10\",\"grunt-webpack\":\"^1.0.8\",\"load-grunt-tasks\":\"^0.6.0\",\"karma\":\"^0.12.21\",\"karma-jasmine\":\"^0.1.5\",\"grunt-karma\":\"^0.8.3\",\"karma-phantomjs-launcher\":\"^0.1.4\",\"karma-jasmine-ajax\":\"^0.1.4\",\"grunt-update-json\":\"^0.1.3\",\"grunt-contrib-nodeunit\":\"^0.4.1\",\"grunt-banner\":\"^0.2.3\"},\"_id\":\"axios@0.1.0\",\"dist\":{\"shasum\":\"854e14f2999c2ef7fab058654fd995dd183688f2\",\"tarball\":\"https://registry.npmjs.org/axios/-/axios-0.1.0.tgz\",\"integrity\":\"sha512-hRPotWTy88LEsJ31RWEs2fmU7mV2YJs3Cw7Tk5XkKGtnT5NKOyIvPU+6qTWfwQFusxzChe8ozjay8r56wfpX8w==\",\"signatures\":[{\"keyid\":\"SHA256:jl3bwswu80PjjokCgh0o2w5c2U4LhQAE57gj9cz1kzA\",\"sig\":\"MEYCIQC/cOvHsV7UqLAet6WE89O4Ga3AUHgkqqoP0riLs6sgTAIhAIrePavu3Uw0T3vLyYMlfEI9bqENYjPzH5jGK8vYQVJK\"}]},\"_from\":\"./\",\"_npmVersion\":\"1.4.3\",\"_npmUser\":{\"name\":\"mzabriskie\",\"email\":\"mzabriskie@gmail.com\"},\"maintainers\":[{\"name\":\"mzabriskie\",\"email\":\"mzabriskie@gmail.com\"}],\"directories\":{},\"deprecated\":\"Critical security vulnerability fixed in v0.21.1. For more information, see https://github.com/axios/axios/pull/3410\"},\"0.2.0\":{\"name\":\"axios\",\"version\":\"0.2.0\",\"description\":\"Promise based HTTP client for the browser and node.js\",\"main\":\"index.js\",\"scripts\":{\"test\":\"grunt test\",\"start\":\"node ./sandbox/server.js\"},\"repository\":{\"type\":\"git\",\"url\":\"https://github.com/mzabriskie/axios.git\"},\"keywords\":[\"xhr\",\"http\",\"ajax\",\"promise\",\"node\"],\"author\":{\"name\":\"Matt Zabriskie\"},\"license\":\"MIT\",\"bugs\":{\"url\":\"https://github.com/mzabriskie/axios/issues\"},\"homepage\":\"https://github.com/mzabriskie/axios\",\"dependencies\":{\"es6-promise\":\"^1.0.0\"},\"devDependencies\":{\"grunt\":\"^0.4.5\",\"grunt-contrib-clean\":\"^0.6.0\",\"grunt-contrib-watch\":\"^0.6.1\",\"webpack\":\"^1.3.3-beta2\",\"webpack-dev-server\":\"^1.4.10\",\"grunt-webpack\":\"^1.0.8\",\"load-grunt-tasks\":\"^0.6.0\",\"karma\":\"^0.12.21\",\"karma-jasmine\":\"^0.1.5\",\"grunt-karma\":\"^0.8.3\",\"karma-phantomjs-launcher\":\"^0.1.4\",\"karma-jasmine-ajax\":\"^0.1.4\",\"grunt-update-json\":\"^0.1.3\",\"grunt-contrib-nodeunit\":\"^0.4.1\",\"grunt-banner\":\"^0.2.3\"},\"_id\":\"axios@0.2.0\",\"dist\":{\"shasum\":\"315cd618142078fd22f2cea35380caad19e32069\",\"tarball\":\"https://registry.npmjs.org/axios/-/axios-0.2.0.tgz\",\"integrity\":\"sha512-ZQb2IDQfop5Asx8PlKvccsSVPD8yFCwYZpXrJCyU+MqL4XgJVjMHkCTNQV/pmB0Wv7l74LUJizSM/SiPz6r9uw==\",\"signatures\":[{\"keyid\":\"SHA256:jl3bwswu80PjjokCgh0o2w5c2U4LhQAE57gj9cz1kzA\",\"sig\":\"MEQCIAkrijLTtL7uiw0fQf5GL/y7bJ+3J8Z0zrrzNLC5fTXlAiBd4Nr/EJ2nWfBGWv/9OkrAONoboG5C8t8plIt5LVeGQA==\"}]},\"_from\":\"./\",\"_npmVersion\":\"1.4.3\",\"_npmUser\":{\"name\":\"mzabriskie\",\"email\":\"mzabriskie@gmail.com\"},\"maintainers\":[{\"name\":\"mzabriskie\",\"email\":\"mzabriskie@gmail.com\"}],\"directories\":{},\"deprecated\":\"Critical security vulnerability fixed in v0.21.1. For more information, see https://github.com/axios/axios/pull/3410\"}},\"readme\":\"axios\",\"maintainers\":[],\"time\":{\"modified\":\"2022-12-29T06:38:42.456Z\",\"created\":\"2014-08-29T23:08:36.810Z\",\"0.1.0\":\"2014-08-29T23:08:36.810Z\",\"0.2.0\":\"2014-09-12T20:06:33.167Z\"},\"homepage\":\"https://axios-http.com\",\"keywords\":[],\"repository\":{\"type\":\"git\",\"url\":\"git+https://github.com/axios/axios.git\"},\"author\":{\"name\":\"Matt Zabriskie\"},\"bugs\":{\"url\":\"https://github.com/axios/axios/issues\"},\"license\":\"MIT\",\"readmeFilename\":\"README.md\",\"users\":{},\"contributors\":[]}\n";
//     const NPM_EXISTING_SCOPE_PACKAGE_DATA: &str = "{\"_id\":\"@TestScope/test\",\"_rev\":\"779-b37ceeb27a03858a89a0226f7c554aaf\",\"name\":\"@TestScope/test\",\"description\":\"Promise based HTTP client for the browser and node.js\",\"dist-tags\":{\"latest\":\"0.1.0\",\"next\":\"0.2.0\"},\"versions\":{\"0.1.0\":{\"name\":\"@TestScope/test\",\"version\":\"0.1.0\",\"description\":\"Promise based XHR library\",\"main\":\"index.js\",\"scripts\":{\"test\":\"grunt test\",\"start\":\"node ./sandbox/index.js\"},\"repository\":{\"type\":\"git\",\"url\":\"https://github.com/mzabriskie/@TestScope/test.git\"},\"keywords\":[\"xhr\",\"http\",\"ajax\",\"promise\"],\"author\":{\"name\":\"Matt Zabriskie\"},\"license\":\"MIT\",\"bugs\":{\"url\":\"https://github.com/mzabriskie/@TestScope/test/issues\"},\"homepage\":\"https://github.com/mzabriskie/@TestScope/test\",\"dependencies\":{\"es6-promise\":\"^1.0.0\"},\"devDependencies\":{\"grunt\":\"^0.4.5\",\"grunt-contrib-clean\":\"^0.6.0\",\"grunt-contrib-watch\":\"^0.6.1\",\"webpack\":\"^1.3.3-beta2\",\"webpack-dev-server\":\"^1.4.10\",\"grunt-webpack\":\"^1.0.8\",\"load-grunt-tasks\":\"^0.6.0\",\"karma\":\"^0.12.21\",\"karma-jasmine\":\"^0.1.5\",\"grunt-karma\":\"^0.8.3\",\"karma-phantomjs-launcher\":\"^0.1.4\",\"karma-jasmine-ajax\":\"^0.1.4\",\"grunt-update-json\":\"^0.1.3\",\"grunt-contrib-nodeunit\":\"^0.4.1\",\"grunt-banner\":\"^0.2.3\"},\"_id\":\"@TestScope/test@0.1.0\",\"dist\":{\"shasum\":\"854e14f2999c2ef7fab058654fd995dd183688f2\",\"tarball\":\"https://registry.npmjs.org/@TestScope/test/-/@TestScope/test-0.1.0.tgz\",\"integrity\":\"sha512-hRPotWTy88LEsJ31RWEs2fmU7mV2YJs3Cw7Tk5XkKGtnT5NKOyIvPU+6qTWfwQFusxzChe8ozjay8r56wfpX8w==\",\"signatures\":[{\"keyid\":\"SHA256:jl3bwswu80PjjokCgh0o2w5c2U4LhQAE57gj9cz1kzA\",\"sig\":\"MEYCIQC/cOvHsV7UqLAet6WE89O4Ga3AUHgkqqoP0riLs6sgTAIhAIrePavu3Uw0T3vLyYMlfEI9bqENYjPzH5jGK8vYQVJK\"}]},\"_from\":\"./\",\"_npmVersion\":\"1.4.3\",\"_npmUser\":{\"name\":\"mzabriskie\",\"email\":\"mzabriskie@gmail.com\"},\"maintainers\":[{\"name\":\"mzabriskie\",\"email\":\"mzabriskie@gmail.com\"}],\"directories\":{},\"deprecated\":\"Critical security vulnerability fixed in v0.21.1. For more information, see https://github.com/@TestScope/test/@TestScope/test/pull/3410\"},\"0.2.0\":{\"name\":\"@TestScope/test\",\"version\":\"0.2.0\",\"description\":\"Promise based HTTP client for the browser and node.js\",\"main\":\"index.js\",\"scripts\":{\"test\":\"grunt test\",\"start\":\"node ./sandbox/server.js\"},\"repository\":{\"type\":\"git\",\"url\":\"https://github.com/mzabriskie/@TestScope/test.git\"},\"keywords\":[\"xhr\",\"http\",\"ajax\",\"promise\",\"node\"],\"author\":{\"name\":\"Matt Zabriskie\"},\"license\":\"MIT\",\"bugs\":{\"url\":\"https://github.com/mzabriskie/@TestScope/test/issues\"},\"homepage\":\"https://github.com/mzabriskie/@TestScope/test\",\"dependencies\":{\"es6-promise\":\"^1.0.0\"},\"devDependencies\":{\"grunt\":\"^0.4.5\",\"grunt-contrib-clean\":\"^0.6.0\",\"grunt-contrib-watch\":\"^0.6.1\",\"webpack\":\"^1.3.3-beta2\",\"webpack-dev-server\":\"^1.4.10\",\"grunt-webpack\":\"^1.0.8\",\"load-grunt-tasks\":\"^0.6.0\",\"karma\":\"^0.12.21\",\"karma-jasmine\":\"^0.1.5\",\"grunt-karma\":\"^0.8.3\",\"karma-phantomjs-launcher\":\"^0.1.4\",\"karma-jasmine-ajax\":\"^0.1.4\",\"grunt-update-json\":\"^0.1.3\",\"grunt-contrib-nodeunit\":\"^0.4.1\",\"grunt-banner\":\"^0.2.3\"},\"_id\":\"@TestScope/test@0.2.0\",\"dist\":{\"shasum\":\"315cd618142078fd22f2cea35380caad19e32069\",\"tarball\":\"https://registry.npmjs.org/@TestScope/test/-/@TestScope/test-0.2.0.tgz\",\"integrity\":\"sha512-ZQb2IDQfop5Asx8PlKvccsSVPD8yFCwYZpXrJCyU+MqL4XgJVjMHkCTNQV/pmB0Wv7l74LUJizSM/SiPz6r9uw==\",\"signatures\":[{\"keyid\":\"SHA256:jl3bwswu80PjjokCgh0o2w5c2U4LhQAE57gj9cz1kzA\",\"sig\":\"MEQCIAkrijLTtL7uiw0fQf5GL/y7bJ+3J8Z0zrrzNLC5fTXlAiBd4Nr/EJ2nWfBGWv/9OkrAONoboG5C8t8plIt5LVeGQA==\"}]},\"_from\":\"./\",\"_npmVersion\":\"1.4.3\",\"_npmUser\":{\"name\":\"mzabriskie\",\"email\":\"mzabriskie@gmail.com\"},\"maintainers\":[{\"name\":\"mzabriskie\",\"email\":\"mzabriskie@gmail.com\"}],\"directories\":{},\"deprecated\":\"Critical security vulnerability fixed in v0.21.1. For more information, see https://github.com/@TestScope/test/@TestScope/test/pull/3410\"}},\"readme\":\"@TestScope/test\",\"maintainers\":[],\"time\":{\"modified\":\"2022-12-29T06:38:42.456Z\",\"created\":\"2014-08-29T23:08:36.810Z\",\"0.1.0\":\"2014-08-29T23:08:36.810Z\",\"0.2.0\":\"2014-09-12T20:06:33.167Z\"},\"homepage\":\"https://@TestScope/test-http.com\",\"keywords\":[],\"repository\":{\"type\":\"git\",\"url\":\"git+https://github.com/@TestScope/test/@TestScope/test.git\"},\"author\":{\"name\":\"Matt Zabriskie\"},\"bugs\":{\"url\":\"https://github.com/@TestScope/test/@TestScope/test/issues\"},\"license\":\"MIT\",\"readmeFilename\":\"README.md\",\"users\":{},\"contributors\":[]}\n";
//
//     #[allow(clippy::too_many_arguments)]
//     async fn npm_test(
//         package_name: String,
//         package_version: String,
//         registry_token: Option<String>,
//         npmrc: bool,
//         expected_result: bool,
//         expected_error: bool,
//         mocked_body: Option<String>,
//         mocked_token: Option<String>,
//         mocked_status: Option<u16>,
//     ) {
//         let tmp_dir = TempDir::new().expect("cannot create tmp directory");
//         let mut registry_url: Option<String> = None;
//
//         let mut npmrc_path: Option<String> = None;
//         if let (Some(body), Some(token), Some(status)) = (mocked_body, mocked_token, mocked_status)
//         {
//             let mock_server = MockServer::start().await;
//             Mock::given(method("GET"))
//                 .and(path(format!("/{}", package_name)))
//                 .and(bearer_token(token.clone()))
//                 .respond_with(ResponseTemplate::new(status).set_body_raw(body, "application/json"))
//                 .mount(&mock_server)
//                 .await;
//             let mock_server_uri = mock_server.uri();
//
//             if npmrc {
//                 let path = tmp_dir.path().join(".npmrc");
//                 let mut f = File::create(path.clone()).expect("Could not create npmrc file");
//                 let npm_mock_server_uri = mock_server_uri.replace("http://", "");
//                 let npmrc = format!(
//                     "//{}/:_authToken={}\n@TestScope:registry=https://{}/",
//                     npm_mock_server_uri, token, npm_mock_server_uri
//                 );
//                 f.write_all(npmrc.as_bytes())
//                     .expect("Could not write htpasswd file");
//                 npmrc_path = Some(path.to_string_lossy().to_string());
//             } else {
//                 registry_url = Some(format!("{}/", mock_server_uri));
//             }
//         }
//
//         let npm = Npm::new(registry_url, registry_token, npmrc_path, false)
//             .expect("Could not get npm client");
//         let result = npm
//             .check_npm_package_exists(package_name, package_version)
//             .await;
//         match result {
//             Ok(exists) => {
//                 assert!(!expected_error);
//                 assert_eq!(expected_result, exists);
//             }
//             Err(e) => {
//                 println!("Got error: {}", e);
//                 assert!(expected_error);
//             }
//         }
//     }
//
//     #[tokio::test]
//     async fn npm_package_existing_package() {
//         npm_test(
//             "axios".to_string(),
//             "1.0.0".to_string(),
//             None,
//             false,
//             true,
//             false,
//             None::<String>,
//             None::<String>,
//             None::<u16>,
//         )
//             .await;
//     }
//
//     #[tokio::test]
//     async fn npm_package_on_existing_package() {
//         npm_test(
//             "axios".to_string(),
//             "99.99.99".to_string(),
//             None,
//             false,
//             false,
//             false,
//             None::<String>,
//             None::<String>,
//             None::<u16>,
//         )
//             .await;
//     }
//
//     #[tokio::test]
//     async fn npm_package_existing_package_custom_registry_token() {
//         npm_test(
//             "axios".to_string(),
//             "0.2.0".to_string(),
//             Some("my_token".to_string()),
//             false,
//             true,
//             false,
//             Some(NPM_EXISTING_PACKAGE_DATA.to_string()),
//             Some("my_token".to_string()),
//             Some(200),
//         )
//             .await;
//     }
//
//     #[tokio::test]
//     async fn npm_package_non_existing_package_version_custom_registry_token() {
//         npm_test(
//             "axios".to_string(),
//             "99.99.99".to_string(),
//             Some("my_token".to_string()),
//             false,
//             false,
//             false,
//             Some(NPM_EXISTING_PACKAGE_DATA.to_string()),
//             Some("my_token".to_string()),
//             Some(200),
//         )
//             .await;
//     }
//
//     #[tokio::test]
//     async fn npm_package_existing_package_custom_registry_npmrc() {
//         npm_test(
//             "@TestScope/test".to_string(),
//             "0.2.0".to_string(),
//             None,
//             true,
//             true,
//             false,
//             Some(NPM_EXISTING_SCOPE_PACKAGE_DATA.to_string()),
//             Some("my_token".to_string()),
//             Some(200),
//         )
//             .await;
//     }
//
//     #[tokio::test]
//     async fn npm_package_non_existing_package_custom_registry_npmrc() {
//         npm_test(
//             "@TestScope/test".to_string(),
//             "99.99.99".to_string(),
//             None,
//             true,
//             false,
//             false,
//             Some(NPM_EXISTING_SCOPE_PACKAGE_DATA.to_string()),
//             Some("my_token".to_string()),
//             Some(200),
//         )
//             .await;
//     }
// }
