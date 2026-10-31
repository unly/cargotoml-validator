use crate::model::{Maintenance, Stage};

pub const COLUMN_HEADERS: [&str; 4] = [
    "Dependency",
    "Requirement",
    "New Major Version",
    "Maintenance Status",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Subtle,
    Ok,
    Warning,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub tone: Tone,
}

impl Cell {
    fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
        }
    }
}

pub fn version_cell(stage: &Stage) -> Cell {
    match stage {
        Stage::Pending => Cell::new("pending", Tone::Subtle),
        Stage::Checking => Cell::new("checking...", Tone::Warning),
        Stage::Failed(_) => Cell::new("-", Tone::Subtle),
        Stage::Done(report) => match &report.new_major {
            None => Cell::new("none", Tone::Ok),
            Some(version) => Cell::new(format!("{version} available"), Tone::Warning),
        },
    }
}

pub fn maintenance_cell(stage: &Stage) -> Cell {
    match stage {
        Stage::Pending => Cell::new("pending", Tone::Subtle),
        Stage::Checking => Cell::new("checking...", Tone::Warning),
        Stage::Failed(reason) => Cell::new(format!("error: {reason}"), Tone::Critical),
        Stage::Done(report) => match &report.maintenance {
            Maintenance::Active(_) => Cell::new("ok", Tone::Ok),
            Maintenance::Stale(last) => Cell::new(
                format!("last commit: {}", last.format("%Y-%m-%d")),
                Tone::Critical,
            ),
            Maintenance::NoCommits => Cell::new("no commits", Tone::Warning),
            Maintenance::Archived => Cell::new("archived", Tone::Critical),
            Maintenance::NoGithubRepo => Cell::new("n/a (no GitHub repository)", Tone::Subtle),
            Maintenance::RepoNotFound => Cell::new("GitHub repository not found", Tone::Critical),
            Maintenance::Unknown(reason) => Cell::new(format!("unknown: {reason}"), Tone::Warning),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Report;
    use semver::Version;

    fn done(new_major: Option<Version>, maintenance: Maintenance) -> Stage {
        Stage::Done(Report {
            new_major,
            maintenance,
        })
    }

    #[test]
    fn version_cell_shows_available_major() {
        let stage = done(Some(Version::new(2, 0, 1)), Maintenance::NoGithubRepo);

        assert_eq!(
            version_cell(&stage),
            Cell::new("2.0.1 available", Tone::Warning)
        );
        assert_eq!(
            version_cell(&done(None, Maintenance::NoGithubRepo)),
            Cell::new("none", Tone::Ok)
        );
    }

    #[test]
    fn maintenance_cell_formats_stale_date() {
        let last = "2022-03-04T05:06:07Z".parse().unwrap();

        assert_eq!(
            maintenance_cell(&done(None, Maintenance::Stale(last))),
            Cell::new("last commit: 2022-03-04", Tone::Critical)
        );
    }

    #[test]
    fn maintenance_cell_covers_progress_and_failure() {
        assert_eq!(maintenance_cell(&Stage::Pending).text, "pending");
        assert_eq!(maintenance_cell(&Stage::Checking).text, "checking...");
        assert_eq!(
            maintenance_cell(&Stage::Failed("boom".into())),
            Cell::new("error: boom", Tone::Critical)
        );
        assert_eq!(
            maintenance_cell(&done(None, Maintenance::Archived)),
            Cell::new("archived", Tone::Critical)
        );
    }
}
