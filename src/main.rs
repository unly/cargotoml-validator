mod cache;
mod checker;
mod github;
mod manifest;
mod model;
mod plain;
mod registry;
mod render;
mod tui;
mod version;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use chrono::Utc;
use clap::Parser;
use tokio::sync::mpsc;

use crate::cache::Cache;
use crate::checker::Checker;
use crate::github::{GITHUB_API_BASE, GithubClient};
use crate::model::Row;
use crate::registry::{CRATES_IO_BASE, Registry};

const MANIFEST_FILE: &str = "Cargo.toml";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const UNAUTHENTICATED_NOTICE: &str =
    "no GITHUB_TOKEN or ~/.netrc entry: GitHub allows only 60 requests/hour";

/// Checks the direct dependencies of a Cargo.toml for new major versions and
/// for upstream GitHub repositories that are archived or have seen no commit
/// in over a year.
///
/// Set GITHUB_TOKEN (or add a github.com entry to ~/.netrc) to avoid GitHub's
/// low unauthenticated rate limit.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Path to a Cargo.toml, or to a directory containing one.
    #[arg(default_value = MANIFEST_FILE)]
    path: PathBuf,

    /// Maximum number of dependencies checked at the same time.
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..=64))]
    concurrency: u32,

    /// Give up on unfinished checks after this many seconds.
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..))]
    timeout_secs: u64,

    /// Print a plain table instead of the interactive UI (implied when stdout is not a terminal).
    #[arg(long)]
    plain: bool,

    /// Exit with status 1 if any dependency has a finding or failed to check.
    #[arg(long)]
    fail_on_issues: bool,

    /// Ignore cached lookups and query crates.io and GitHub again.
    #[arg(long)]
    refresh: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let manifest_path = resolve_manifest_path(&cli.path)?;
    let specs = manifest::load(&manifest_path)?;
    if specs.is_empty() {
        println!(
            "no registry dependencies found in {}",
            manifest_path.display()
        );
        return Ok(ExitCode::SUCCESS);
    }

    let rows: Vec<Row> = specs
        .iter()
        .map(|spec| Row::pending(&spec.name, &spec.requirement))
        .collect();

    let http = build_http_client()?;
    let token = github::find_token();
    let notice = token.is_none().then(|| UNAUTHENTICATED_NOTICE.to_owned());
    let checker = Arc::new(
        Checker::new(
            Registry::new(http.clone(), CRATES_IO_BASE),
            GithubClient::new(http, GITHUB_API_BASE, token),
        )
        .with_cache(Cache::open(cache::default_path(), cli.refresh)),
    );

    let (updates_tx, updates_rx) = mpsc::unbounded_channel();
    let timeout = Duration::from_secs(cli.timeout_secs);
    let concurrency = cli.concurrency as usize;
    let worker = tokio::spawn({
        let checker = Arc::clone(&checker);
        async move {
            // On timeout the future is dropped, which closes the channel and ends the UI loop.
            let _ = tokio::time::timeout(
                timeout,
                checker::run(checker, specs, concurrency, updates_tx),
            )
            .await;
        }
    });

    let final_rows = if cli.plain || !std::io::stdout().is_terminal() {
        if let Some(notice) = &notice {
            eprintln!("note: {notice}");
        }
        plain::run(rows, updates_rx).await
    } else {
        let rows = tokio::task::spawn_blocking(move || tui::run(rows, updates_rx, notice))
            .await
            .context("terminal UI task panicked")??;
        worker.abort();
        rows
    };

    // Saved after a timeout or an early quit too, so finished lookups are kept.
    if let Err(err) = checker.save_cache(Utc::now()) {
        eprintln!("warning: {err:#}");
    }

    let has_issues = final_rows.iter().any(|row| row.stage.has_issue());
    Ok(if cli.fail_on_issues && has_issues {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn resolve_manifest_path(path: &Path) -> anyhow::Result<PathBuf> {
    let absolute = std::path::absolute(path)
        .with_context(|| format!("failed to get absolute path for {}", path.display()))?;

    let manifest = if absolute.is_dir() {
        absolute.join(MANIFEST_FILE)
    } else {
        absolute
    };
    if !manifest.exists() {
        bail!("{} does not exist", manifest.display());
    }
    Ok(manifest)
}

fn build_http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("failed to build HTTP client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_argument_resolves_to_its_manifest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(MANIFEST_FILE), "").unwrap();

        let resolved = resolve_manifest_path(dir.path()).unwrap();

        assert_eq!(resolved, dir.path().join(MANIFEST_FILE));
    }

    #[test]
    fn missing_manifest_is_reported() {
        let dir = tempfile::tempdir().unwrap();

        let err = resolve_manifest_path(&dir.path().join("nope.toml")).unwrap_err();

        assert!(err.to_string().ends_with("nope.toml does not exist"));
    }
}
