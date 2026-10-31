use std::path::PathBuf;

use chrono::{DateTime, Utc};
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const GITHUB_API_BASE: &str = "https://api.github.com";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    pub owner: String,
    pub repo: String,
}

#[derive(Debug, Error)]
pub enum GithubError {
    #[error("repository not found")]
    NotFound,
    #[error("GitHub denied the request (rate limit reached? set GITHUB_TOKEN)")]
    RateLimited,
    #[error("unexpected GitHub response: {0}")]
    Status(StatusCode),
    #[error("GitHub request failed: {0}")]
    Http(#[from] reqwest::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoStatus {
    pub archived: bool,
    /// Committer date of the newest commit on the default branch; `None` for an empty repository.
    pub last_commit: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct RepoResponse {
    archived: bool,
}

#[derive(Deserialize)]
struct CommitItem {
    commit: CommitData,
}

#[derive(Deserialize)]
struct CommitData {
    committer: Option<Signature>,
    author: Option<Signature>,
}

#[derive(Deserialize)]
struct Signature {
    date: DateTime<Utc>,
}

pub struct GithubClient {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl GithubClient {
    pub fn new(http: reqwest::Client, base: impl Into<String>, token: Option<String>) -> Self {
        Self {
            http,
            base: base.into().trim_end_matches('/').to_owned(),
            token,
        }
    }

    /// Fetches the repository and its newest commit concurrently. A repository
    /// error takes precedence, so a missing repository is always `NotFound`.
    pub async fn repo_status(&self, repo: &RepoRef) -> Result<RepoStatus, GithubError> {
        let repo_url = format!("{}/repos/{}/{}", self.base, repo.owner, repo.repo);

        let (archived, last_commit) =
            tokio::join!(self.archived(&repo_url), self.last_commit(&repo_url));

        Ok(RepoStatus {
            archived: archived?,
            last_commit: last_commit?,
        })
    }

    async fn archived(&self, repo_url: &str) -> Result<bool, GithubError> {
        let response = check(self.request(repo_url).send().await?)?;
        Ok(response.json::<RepoResponse>().await?.archived)
    }

    async fn last_commit(&self, repo_url: &str) -> Result<Option<DateTime<Utc>>, GithubError> {
        let commits = self
            .request(&format!("{repo_url}/commits?per_page=1"))
            .send()
            .await?;
        // GitHub answers 409 for a repository without any commits.
        if commits.status() == StatusCode::CONFLICT {
            return Ok(None);
        }

        let items = check(commits)?.json::<Vec<CommitItem>>().await?;
        Ok(items
            .into_iter()
            .next()
            .and_then(|item| item.commit.committer.or(item.commit.author))
            .map(|signature| signature.date))
    }

    fn request(&self, url: &str) -> RequestBuilder {
        let builder = self
            .http
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");

        match &self.token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }
}

fn check(response: Response) -> Result<Response, GithubError> {
    match response.status() {
        status if status.is_success() => Ok(response),
        StatusCode::NOT_FOUND => Err(GithubError::NotFound),
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => Err(GithubError::RateLimited),
        status => Err(GithubError::Status(status)),
    }
}

/// Extracts the owner and repository name from the common spellings of a GitHub URL.
pub fn parse_repo_url(url: &str) -> Option<RepoRef> {
    let url = url.trim();

    let path = if let Some(scp_path) = url.strip_prefix("git@github.com:") {
        scp_path
    } else {
        let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
        let (authority, path) = without_scheme.split_once('/')?;
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        if !matches!(
            host.to_ascii_lowercase().as_str(),
            "github.com" | "www.github.com"
        ) {
            return None;
        }
        path
    };

    let mut segments = path.split('/').filter(|segment| !segment.is_empty());
    let owner = segments.next()?;
    let repo = segments.next()?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);

    (is_valid_segment(owner) && is_valid_segment(repo)).then(|| RepoRef {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
    })
}

/// Owner and repo names are restricted so they cannot alter the API URL.
fn is_valid_segment(segment: &str) -> bool {
    !matches!(segment, "" | "." | "..")
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Looks for a token in `GITHUB_TOKEN`, then in the `github.com` entry of `~/.netrc`.
pub fn find_token() -> Option<String> {
    std::env::var("GITHUB_TOKEN")
        .ok()
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty())
        .or_else(|| {
            let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
            let contents = std::fs::read_to_string(PathBuf::from(home).join(".netrc")).ok()?;
            token_from_netrc(&contents)
        })
}

pub fn token_from_netrc(contents: &str) -> Option<String> {
    let mut tokens = contents.split_whitespace();
    let mut in_github_entry = false;

    while let Some(token) = tokens.next() {
        match token {
            "machine" => in_github_entry = tokens.next() == Some("github.com"),
            "default" => in_github_entry = false,
            "password" if in_github_entry => return tokens.next().map(str::to_owned),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn repo(owner: &str, name: &str) -> RepoRef {
        RepoRef {
            owner: owner.to_owned(),
            repo: name.to_owned(),
        }
    }

    #[test]
    fn parses_common_github_url_forms() {
        let expected = Some(repo("serde-rs", "serde"));
        for url in [
            "https://github.com/serde-rs/serde",
            "https://github.com/serde-rs/serde/",
            "https://github.com/serde-rs/serde.git",
            "http://www.github.com/serde-rs/serde",
            "https://github.com/serde-rs/serde/tree/master/serde",
            "git://github.com/serde-rs/serde.git",
            "ssh://git@github.com/serde-rs/serde.git",
            "git@github.com:serde-rs/serde.git",
            "github.com/serde-rs/serde",
            "  https://GitHub.com/serde-rs/serde  ",
        ] {
            assert_eq!(parse_repo_url(url), expected, "{url}");
        }
    }

    #[test]
    fn rejects_non_github_and_malformed_urls() {
        for url in [
            "https://gitlab.com/a/b",
            "https://github.com.evil.com/a/b",
            "https://example.com/github.com/a/b",
            "https://github.com/onlyowner",
            "https://github.com/",
            "https://github.com/a/b%2Fc",
            "https://github.com/../etc",
            "",
        ] {
            assert_eq!(parse_repo_url(url), None, "{url}");
        }
    }

    #[test]
    fn reads_password_for_github_machine_from_netrc() {
        let netrc = "machine example.com login a password wrong\n\
                     machine github.com\n  login me\n  password ghp_secret\n";

        assert_eq!(token_from_netrc(netrc).as_deref(), Some("ghp_secret"));
    }

    #[test]
    fn netrc_without_github_entry_has_no_token() {
        assert_eq!(
            token_from_netrc("machine example.com login a password b"),
            None
        );
        assert_eq!(token_from_netrc("default login a password b"), None);
        assert_eq!(token_from_netrc(""), None);
    }

    fn client(server: &MockServer, token: Option<&str>) -> GithubClient {
        GithubClient::new(
            reqwest::Client::new(),
            server.uri(),
            token.map(str::to_owned),
        )
    }

    async fn mock_repo(server: &MockServer, archived: bool) {
        Mock::given(method("GET"))
            .and(path("/repos/o/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "archived": archived })),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn reports_archived_flag_and_last_commit_date() {
        let server = MockServer::start().await;
        mock_repo(&server, true).await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits"))
            .and(query_param("per_page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "commit": { "committer": { "date": "2024-03-01T10:00:00Z" } } }
            ])))
            .mount(&server)
            .await;

        let status = client(&server, None)
            .repo_status(&repo("o", "r"))
            .await
            .unwrap();

        assert!(status.archived);
        assert_eq!(
            status.last_commit,
            Some("2024-03-01T10:00:00Z".parse().unwrap())
        );
    }

    #[tokio::test]
    async fn falls_back_to_author_date() {
        let server = MockServer::start().await;
        mock_repo(&server, false).await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "commit": { "committer": null, "author": { "date": "2023-01-02T03:04:05Z" } } }
            ])))
            .mount(&server)
            .await;

        let status = client(&server, None)
            .repo_status(&repo("o", "r"))
            .await
            .unwrap();

        assert_eq!(
            status.last_commit,
            Some("2023-01-02T03:04:05Z".parse().unwrap())
        );
    }

    #[tokio::test]
    async fn empty_repository_has_no_last_commit() {
        let server = MockServer::start().await;
        mock_repo(&server, false).await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits"))
            .respond_with(ResponseTemplate::new(409))
            .mount(&server)
            .await;

        let status = client(&server, None)
            .repo_status(&repo("o", "r"))
            .await
            .unwrap();

        assert_eq!(
            status,
            RepoStatus {
                archived: false,
                last_commit: None
            }
        );
    }

    #[tokio::test]
    async fn maps_missing_repository_and_rate_limit_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/limited"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let gone = client(&server, None).repo_status(&repo("o", "gone")).await;
        let limited = client(&server, None)
            .repo_status(&repo("o", "limited"))
            .await;

        assert!(matches!(gone, Err(GithubError::NotFound)));
        assert!(matches!(limited, Err(GithubError::RateLimited)));
    }

    #[tokio::test]
    async fn repository_and_commit_requests_run_concurrently() {
        const DELAY: Duration = Duration::from_millis(400);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "archived": false }))
                    .set_delay(DELAY),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([]))
                    .set_delay(DELAY),
            )
            .mount(&server)
            .await;

        let started = Instant::now();
        let status = client(&server, None).repo_status(&repo("o", "r")).await;

        assert!(status.is_ok());
        assert!(
            started.elapsed() < DELAY * 2,
            "requests ran sequentially: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn repository_error_wins_over_commit_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let result = client(&server, None).repo_status(&repo("o", "r")).await;

        assert!(matches!(result, Err(GithubError::NotFound)));
    }

    #[tokio::test]
    async fn sends_bearer_token_when_configured() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r"))
            .and(header("authorization", "Bearer secret"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "archived": false })),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/commits"))
            .and(header("authorization", "Bearer secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;

        let status = client(&server, Some("secret"))
            .repo_status(&repo("o", "r"))
            .await;

        assert!(status.is_ok());
    }
}
