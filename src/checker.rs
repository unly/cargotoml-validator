use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use futures::stream::{self, StreamExt};
use tokio::sync::mpsc::UnboundedSender;

use crate::cache::Cache;
use crate::github::{self, GithubClient, GithubError, RepoRef, RepoStatus};
use crate::manifest::DependencySpec;
use crate::model::{Maintenance, Report, Stage, Update};
use crate::registry::{CrateInfo, Registry, RegistryError};
use crate::version;

/// A repository without commits for longer than this counts as unmaintained.
const STALE_AFTER_DAYS: i64 = 365;

/// New releases show up quickly, so crate metadata is kept for a short time only.
const CRATE_TTL: Duration = Duration::hours(6);
/// Archival and commit activity are judged on a scale of months.
const REPO_TTL: Duration = Duration::hours(12);

pub struct Checker {
    registry: Registry,
    github: GithubClient,
    cache: Option<Cache>,
}

impl Checker {
    pub fn new(registry: Registry, github: GithubClient) -> Self {
        Self {
            registry,
            github,
            cache: None,
        }
    }

    /// Serves successful lookups from `cache` while they are fresh.
    pub fn with_cache(self, cache: Cache) -> Self {
        Self {
            cache: Some(cache),
            ..self
        }
    }

    /// Persists the lookup cache, if there is one.
    pub fn save_cache(&self, now: DateTime<Utc>) -> anyhow::Result<()> {
        self.cache.as_ref().map_or(Ok(()), |cache| cache.save(now))
    }

    /// Runs all checks for one dependency. Only a crates.io failure is fatal;
    /// a GitHub failure still yields the version result.
    pub async fn check(&self, spec: &DependencySpec, now: DateTime<Utc>) -> Stage {
        let info = match self.crate_info(&spec.name, now).await {
            Ok(info) => info,
            Err(err) => return Stage::Failed(err.to_string()),
        };

        let new_major = match version::newer_incompatible(&spec.requirement, &info.versions) {
            Ok(new_major) => new_major,
            Err(err) => {
                return Stage::Failed(format!(
                    "invalid version requirement `{}`: {err}",
                    spec.requirement
                ));
            }
        };

        let maintenance = self.maintenance(info.repository.as_deref(), now).await;

        Stage::Done(Report {
            new_major,
            maintenance,
        })
    }

    async fn maintenance(&self, repository: Option<&str>, now: DateTime<Utc>) -> Maintenance {
        let Some(repo) = repository.and_then(github::parse_repo_url) else {
            return Maintenance::NoGithubRepo;
        };

        match self.repo_status(&repo, now).await {
            Ok(status) => classify(&status, now),
            Err(GithubError::NotFound) => Maintenance::RepoNotFound,
            Err(err) => Maintenance::Unknown(err.to_string()),
        }
    }

    async fn crate_info(&self, name: &str, now: DateTime<Utc>) -> Result<CrateInfo, RegistryError> {
        let key = format!("crate/{}", name.to_ascii_lowercase());
        if let Some(info) = self.cached(&key, now) {
            return Ok(info);
        }

        let info = self.registry.fetch(name).await?;
        self.store(&key, &info, now, CRATE_TTL);
        Ok(info)
    }

    async fn repo_status(
        &self,
        repo: &RepoRef,
        now: DateTime<Utc>,
    ) -> Result<RepoStatus, GithubError> {
        let key = format!("repo/{}/{}", repo.owner, repo.repo).to_ascii_lowercase();
        if let Some(status) = self.cached(&key, now) {
            return Ok(status);
        }

        let status = self.github.repo_status(repo).await?;
        self.store(&key, &status, now, REPO_TTL);
        Ok(status)
    }

    fn cached<T: serde::de::DeserializeOwned>(&self, key: &str, now: DateTime<Utc>) -> Option<T> {
        self.cache.as_ref()?.get(key, now)
    }

    fn store<T: serde::Serialize>(&self, key: &str, value: &T, now: DateTime<Utc>, ttl: Duration) {
        if let Some(cache) = &self.cache {
            cache.put(key, value, now, ttl);
        }
    }
}

fn classify(status: &RepoStatus, now: DateTime<Utc>) -> Maintenance {
    if status.archived {
        return Maintenance::Archived;
    }

    match status.last_commit {
        None => Maintenance::NoCommits,
        Some(last) if now - last > Duration::days(STALE_AFTER_DAYS) => Maintenance::Stale(last),
        Some(last) => Maintenance::Active(last),
    }
}

/// Checks all dependencies with at most `concurrency` in flight, reporting
/// progress as `Update`s. Returns once every dependency has finished.
pub async fn run(
    checker: Arc<Checker>,
    specs: Vec<DependencySpec>,
    concurrency: usize,
    updates: UnboundedSender<Update>,
) {
    stream::iter(specs.into_iter().enumerate())
        .for_each_concurrent(concurrency, |(index, spec)| {
            let checker = Arc::clone(&checker);
            let updates = updates.clone();
            async move {
                // A closed receiver just means nobody is watching any more.
                let _ = updates.send(Update {
                    index,
                    stage: Stage::Checking,
                });
                let stage = checker.check(&spec, Utc::now()).await;
                let _ = updates.send(Update { index, stage });
            }
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use semver::Version;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn now() -> DateTime<Utc> {
        "2026-10-16T12:00:00Z".parse().unwrap()
    }

    fn at(timestamp: &str) -> DateTime<Utc> {
        timestamp.parse().unwrap()
    }

    fn spec(name: &str, requirement: &str) -> DependencySpec {
        DependencySpec {
            name: name.to_owned(),
            requirement: requirement.to_owned(),
        }
    }

    fn checker(server: &MockServer) -> Checker {
        let http = reqwest::Client::new();
        Checker::new(
            Registry::new(http.clone(), server.uri()),
            GithubClient::new(http, server.uri(), None),
        )
    }

    async fn mock_crate(
        server: &MockServer,
        name: &str,
        repository: Option<&str>,
        versions: &[&str],
    ) {
        let versions: Vec<_> = versions
            .iter()
            .map(|num| serde_json::json!({ "num": num, "yanked": false }))
            .collect();
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/crates/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "crate": { "repository": repository },
                "versions": versions
            })))
            .mount(server)
            .await;
    }

    async fn mock_github(server: &MockServer, repo: &str, archived: bool, last_commit: &str) {
        Mock::given(method("GET"))
            .and(path(format!("/repos/{repo}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "archived": archived })),
            )
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/{repo}/commits")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "commit": { "committer": { "date": last_commit } } }
            ])))
            .mount(server)
            .await;
    }

    #[test]
    fn classify_prefers_archived_over_staleness() {
        let status = RepoStatus {
            archived: true,
            last_commit: Some(at("2015-01-01T00:00:00Z")),
        };

        assert_eq!(classify(&status, now()), Maintenance::Archived);
    }

    #[test]
    fn classify_uses_a_one_year_threshold() {
        let status = |last: &str| RepoStatus {
            archived: false,
            last_commit: Some(at(last)),
        };

        assert!(matches!(
            classify(&status("2025-10-16T12:00:00Z"), now()),
            Maintenance::Active(_)
        ));
        assert!(matches!(
            classify(&status("2025-10-15T12:00:00Z"), now()),
            Maintenance::Stale(_)
        ));
    }

    #[test]
    fn classify_flags_repositories_without_commits() {
        let status = RepoStatus {
            archived: false,
            last_commit: None,
        };

        assert_eq!(classify(&status, now()), Maintenance::NoCommits);
    }

    #[tokio::test]
    async fn reports_new_major_and_active_repository() {
        let server = MockServer::start().await;
        mock_crate(
            &server,
            "demo",
            Some("https://github.com/o/r"),
            &["1.0.0", "2.1.0"],
        )
        .await;
        mock_github(&server, "o/r", false, "2026-09-01T00:00:00Z").await;

        let stage = checker(&server).check(&spec("demo", "1"), now()).await;

        assert_eq!(
            stage,
            Stage::Done(Report {
                new_major: Some(Version::new(2, 1, 0)),
                maintenance: Maintenance::Active(at("2026-09-01T00:00:00Z")),
            })
        );
    }

    #[tokio::test]
    async fn reports_stale_repository() {
        let server = MockServer::start().await;
        mock_crate(&server, "demo", Some("https://github.com/o/r"), &["1.0.0"]).await;
        mock_github(&server, "o/r", false, "2022-01-01T00:00:00Z").await;

        let stage = checker(&server).check(&spec("demo", "1"), now()).await;

        assert_eq!(
            stage,
            Stage::Done(Report {
                new_major: None,
                maintenance: Maintenance::Stale(at("2022-01-01T00:00:00Z")),
            })
        );
    }

    #[tokio::test]
    async fn crates_without_github_repository_skip_maintenance_checks() {
        let server = MockServer::start().await;
        mock_crate(&server, "demo", Some("https://gitlab.com/o/r"), &["1.0.0"]).await;

        let stage = checker(&server).check(&spec("demo", "1"), now()).await;

        assert_eq!(
            stage,
            Stage::Done(Report {
                new_major: None,
                maintenance: Maintenance::NoGithubRepo,
            })
        );
    }

    #[tokio::test]
    async fn missing_github_repository_is_reported() {
        let server = MockServer::start().await;
        mock_crate(
            &server,
            "demo",
            Some("https://github.com/o/gone"),
            &["1.0.0"],
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let stage = checker(&server).check(&spec("demo", "1"), now()).await;

        assert!(matches!(
            stage,
            Stage::Done(Report {
                maintenance: Maintenance::RepoNotFound,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn github_failure_keeps_the_version_result() {
        let server = MockServer::start().await;
        mock_crate(
            &server,
            "demo",
            Some("https://github.com/o/r"),
            &["1.0.0", "2.0.0"],
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let stage = checker(&server).check(&spec("demo", "1"), now()).await;

        let Stage::Done(report) = stage else {
            panic!("expected a finished report, got {stage:?}");
        };
        assert_eq!(report.new_major, Some(Version::new(2, 0, 0)));
        assert!(matches!(report.maintenance, Maintenance::Unknown(_)));
    }

    #[tokio::test]
    async fn unknown_crate_fails_the_row() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let stage = checker(&server).check(&spec("nope", "1"), now()).await;

        assert_eq!(stage, Stage::Failed("crate not found on crates.io".into()));
    }

    #[tokio::test]
    async fn invalid_requirement_fails_the_row() {
        let server = MockServer::start().await;
        mock_crate(&server, "demo", None, &["1.0.0"]).await;

        let stage = checker(&server).check(&spec("demo", "oops"), now()).await;

        assert!(
            matches!(stage, Stage::Failed(message) if message.contains("invalid version requirement"))
        );
    }

    fn cached_checker(server: &MockServer, refresh: bool) -> Checker {
        checker(server).with_cache(Cache::open(None, refresh))
    }

    async fn request_count(server: &MockServer) -> usize {
        server.received_requests().await.unwrap().len()
    }

    async fn mock_active_crate(server: &MockServer) {
        mock_crate(server, "demo", Some("https://github.com/o/r"), &["1.0.0"]).await;
        mock_github(server, "o/r", false, "2026-09-01T00:00:00Z").await;
    }

    #[tokio::test]
    async fn repeated_checks_are_served_from_the_cache() {
        let server = MockServer::start().await;
        mock_active_crate(&server).await;
        let checker = cached_checker(&server, false);

        let first = checker.check(&spec("demo", "1"), now()).await;
        let second = checker.check(&spec("demo", "1"), now()).await;

        assert_eq!(first, second);
        assert!(matches!(first, Stage::Done(_)));
        // crates.io, GitHub repository and GitHub commits: once each.
        assert_eq!(request_count(&server).await, 3);
    }

    #[tokio::test]
    async fn refresh_bypasses_the_cache() {
        let server = MockServer::start().await;
        mock_active_crate(&server).await;
        let checker = cached_checker(&server, true);

        checker.check(&spec("demo", "1"), now()).await;
        checker.check(&spec("demo", "1"), now()).await;

        assert_eq!(request_count(&server).await, 6);
    }

    #[tokio::test]
    async fn cached_entries_expire_after_their_ttl() {
        let server = MockServer::start().await;
        mock_active_crate(&server).await;
        let checker = cached_checker(&server, false);

        checker.check(&spec("demo", "1"), now()).await;
        // Past the crates.io TTL (6h) but within the GitHub TTL (12h).
        let later = now() + Duration::hours(7);
        checker.check(&spec("demo", "1"), later).await;

        assert_eq!(request_count(&server).await, 4);
    }

    #[tokio::test]
    async fn failed_lookups_are_not_cached() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        mock_crate(&server, "demo", None, &["1.0.0"]).await;
        let checker = cached_checker(&server, false);

        let first = checker.check(&spec("demo", "1"), now()).await;
        let second = checker.check(&spec("demo", "1"), now()).await;

        assert!(matches!(first, Stage::Failed(_)));
        assert!(matches!(second, Stage::Done(_)));
    }

    #[tokio::test]
    async fn run_reports_checking_then_result_for_every_dependency() {
        let server = MockServer::start().await;
        mock_crate(&server, "a", None, &["1.0.0"]).await;
        mock_crate(&server, "b", None, &["1.0.0"]).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        run(
            Arc::new(checker(&server)),
            vec![spec("a", "1"), spec("b", "1")],
            1,
            tx,
        )
        .await;

        let mut updates = Vec::new();
        while let Some(update) = rx.recv().await {
            updates.push(update);
        }
        assert_eq!(updates.len(), 4);
        for index in 0..2 {
            let stages: Vec<_> = updates
                .iter()
                .filter(|u| u.index == index)
                .map(|u| u.stage.is_finished())
                .collect();
            assert_eq!(stages, [false, true]);
        }
    }
}
