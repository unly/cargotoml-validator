use chrono::{DateTime, Utc};
use semver::Version;

/// One dependency row and how far its checks have progressed.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub name: String,
    pub requirement: String,
    pub stage: Stage,
}

impl Row {
    pub fn pending(name: &str, requirement: &str) -> Self {
        Self {
            name: name.to_owned(),
            requirement: requirement.to_owned(),
            stage: Stage::Pending,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    Pending,
    Checking,
    Done(Report),
    Failed(String),
}

impl Stage {
    pub fn is_finished(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Failed(_))
    }

    pub fn has_issue(&self) -> bool {
        match self {
            Self::Done(report) => report.has_issue(),
            Self::Failed(_) => true,
            Self::Pending | Self::Checking => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// Newest stable release, set only if it is semver-incompatible with the requirement.
    pub new_major: Option<Version>,
    pub maintenance: Maintenance,
}

impl Report {
    pub fn has_issue(&self) -> bool {
        self.new_major.is_some() || self.maintenance.is_problem()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Maintenance {
    /// Last commit on the default branch is recent.
    Active(DateTime<Utc>),
    /// Last commit is more than a year old.
    Stale(DateTime<Utc>),
    NoCommits,
    Archived,
    /// The crate declares no GitHub repository, so nothing can be checked.
    NoGithubRepo,
    RepoNotFound,
    /// The GitHub lookup failed; carries the reason.
    Unknown(String),
}

impl Maintenance {
    pub fn is_problem(&self) -> bool {
        matches!(self, Self::Stale(_) | Self::Archived | Self::RepoNotFound)
    }
}

/// A progress notification for the row at `index`.
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub index: usize,
    pub stage: Stage,
}

/// Returns `rows` with the update applied.
pub fn apply_update(rows: Vec<Row>, update: Update) -> Vec<Row> {
    rows.into_iter()
        .enumerate()
        .map(|(index, row)| {
            if index == update.index {
                Row {
                    stage: update.stage.clone(),
                    ..row
                }
            } else {
                row
            }
        })
        .collect()
}

/// Marks every row that never finished as failed with `reason`.
pub fn fail_unfinished(rows: Vec<Row>, reason: &str) -> Vec<Row> {
    rows.into_iter()
        .map(|row| {
            if row.stage.is_finished() {
                row
            } else {
                Row {
                    stage: Stage::Failed(reason.to_owned()),
                    ..row
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_update_replaces_only_the_target_row() {
        let rows = vec![Row::pending("a", "1"), Row::pending("b", "2")];

        let rows = apply_update(
            rows,
            Update {
                index: 1,
                stage: Stage::Checking,
            },
        );

        assert_eq!(rows[0].stage, Stage::Pending);
        assert_eq!(rows[1].stage, Stage::Checking);
    }

    #[test]
    fn fail_unfinished_keeps_finished_rows() {
        let done = Row {
            stage: Stage::Done(Report {
                new_major: None,
                maintenance: Maintenance::NoGithubRepo,
            }),
            ..Row::pending("a", "1")
        };
        let rows = fail_unfinished(vec![done.clone(), Row::pending("b", "2")], "timed out");

        assert_eq!(rows[0], done);
        assert_eq!(rows[1].stage, Stage::Failed("timed out".into()));
    }

    #[test]
    fn issues_are_flagged() {
        let clean = Report {
            new_major: None,
            maintenance: Maintenance::NoGithubRepo,
        };
        assert!(!clean.has_issue());
        assert!(
            Report {
                new_major: Some(Version::new(2, 0, 0)),
                ..clean.clone()
            }
            .has_issue()
        );
        assert!(
            Report {
                maintenance: Maintenance::Archived,
                ..clean
            }
            .has_issue()
        );
        assert!(Stage::Failed("x".into()).has_issue());
        assert!(!Stage::Pending.has_issue());
    }
}
