use std::time::Duration;

use reqwest::StatusCode;
use semver::Version;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const CRATES_IO_BASE: &str = "https://crates.io";

const MAX_ATTEMPTS: u32 = 4;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("`{0}` is not a valid crate name")]
    InvalidName(String),
    #[error("crate not found on crates.io")]
    NotFound,
    #[error("crates.io rate limit exceeded")]
    RateLimited,
    #[error("unexpected crates.io response: {0}")]
    Status(StatusCode),
    #[error("crates.io request failed: {0}")]
    Http(#[from] reqwest::Error),
}

/// What the checks need to know about a published crate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrateInfo {
    pub repository: Option<String>,
    /// All releases that have not been yanked.
    pub versions: Vec<Version>,
}

#[derive(Deserialize)]
struct CrateResponse {
    #[serde(rename = "crate")]
    krate: CrateMeta,
    versions: Vec<VersionMeta>,
}

#[derive(Deserialize)]
struct CrateMeta {
    repository: Option<String>,
}

#[derive(Deserialize)]
struct VersionMeta {
    num: String,
    yanked: bool,
}

pub struct Registry {
    http: reqwest::Client,
    base: String,
}

impl Registry {
    pub fn new(http: reqwest::Client, base: impl Into<String>) -> Self {
        Self {
            http,
            base: base.into().trim_end_matches('/').to_owned(),
        }
    }

    pub async fn fetch(&self, name: &str) -> Result<CrateInfo, RegistryError> {
        if !is_valid_crate_name(name) {
            return Err(RegistryError::InvalidName(name.to_owned()));
        }

        let url = format!("{}/api/v1/crates/{name}", self.base);
        for attempt in 1..=MAX_ATTEMPTS {
            let response = self.http.get(&url).send().await?;
            match response.status() {
                StatusCode::OK => return Ok(into_info(response.json().await?)),
                StatusCode::NOT_FOUND => return Err(RegistryError::NotFound),
                StatusCode::TOO_MANY_REQUESTS if attempt < MAX_ATTEMPTS => {
                    tokio::time::sleep(retry_delay(&response, attempt)).await;
                }
                StatusCode::TOO_MANY_REQUESTS => return Err(RegistryError::RateLimited),
                status => return Err(RegistryError::Status(status)),
            }
        }
        Err(RegistryError::RateLimited)
    }
}

fn into_info(response: CrateResponse) -> CrateInfo {
    let versions = response
        .versions
        .into_iter()
        .filter(|v| !v.yanked)
        .filter_map(|v| Version::parse(&v.num).ok())
        .collect();

    CrateInfo {
        repository: response.krate.repository.filter(|r| !r.trim().is_empty()),
        versions,
    }
}

/// Honours `Retry-After`, otherwise backs off exponentially.
fn retry_delay(response: &reqwest::Response, attempt: u32) -> Duration {
    let requested = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs);

    requested
        .unwrap_or_else(|| Duration::from_secs(1 << attempt))
        .min(MAX_RETRY_DELAY)
}

/// Crate names are ASCII alphanumerics, `-` and `_`; anything else must never reach a URL.
fn is_valid_crate_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn registry(server: &MockServer) -> Registry {
        Registry::new(reqwest::Client::new(), server.uri())
    }

    fn crate_body() -> serde_json::Value {
        serde_json::json!({
            "crate": { "repository": "https://github.com/serde-rs/serde" },
            "versions": [
                { "num": "1.0.0", "yanked": false },
                { "num": "1.0.1", "yanked": true },
                { "num": "2.0.0-rc.1", "yanked": false },
                { "num": "not-semver", "yanked": false }
            ]
        })
    }

    #[tokio::test]
    async fn fetches_repository_and_unyanked_versions() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/crates/serde"))
            .respond_with(ResponseTemplate::new(200).set_body_json(crate_body()))
            .mount(&server)
            .await;

        let info = registry(&server).fetch("serde").await.unwrap();

        assert_eq!(
            info.repository.as_deref(),
            Some("https://github.com/serde-rs/serde")
        );
        assert_eq!(
            info.versions,
            vec![
                Version::parse("1.0.0").unwrap(),
                Version::parse("2.0.0-rc.1").unwrap()
            ]
        );
    }

    #[tokio::test]
    async fn empty_repository_is_treated_as_missing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "crate": { "repository": " " },
                "versions": []
            })))
            .mount(&server)
            .await;

        let info = registry(&server).fetch("serde").await.unwrap();

        assert_eq!(info.repository, None);
    }

    #[tokio::test]
    async fn maps_404_to_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let err = registry(&server).fetch("missing").await.unwrap_err();

        assert!(matches!(err, RegistryError::NotFound));
    }

    #[tokio::test]
    async fn retries_after_rate_limit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(crate_body()))
            .mount(&server)
            .await;

        assert!(registry(&server).fetch("serde").await.is_ok());
    }

    #[tokio::test]
    async fn gives_up_when_persistently_rate_limited() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
            .mount(&server)
            .await;

        let err = registry(&server).fetch("serde").await.unwrap_err();

        assert!(matches!(err, RegistryError::RateLimited));
    }

    #[tokio::test]
    async fn reports_unexpected_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let err = registry(&server).fetch("serde").await.unwrap_err();

        assert!(matches!(
            err,
            RegistryError::Status(StatusCode::INTERNAL_SERVER_ERROR)
        ));
    }

    #[tokio::test]
    async fn rejects_names_that_could_alter_the_url() {
        let server = MockServer::start().await;

        for name in ["", "../etc", "a/b", "a?b", "a b"] {
            let err = registry(&server).fetch(name).await.unwrap_err();
            assert!(matches!(err, RegistryError::InvalidName(_)), "{name:?}");
        }
    }
}
