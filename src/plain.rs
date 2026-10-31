use tokio::sync::mpsc::UnboundedReceiver;

use crate::model::{Row, Update, apply_update, fail_unfinished};
use crate::render::{COLUMN_HEADERS, maintenance_cell, version_cell};

const COLUMN_GAP: &str = "  ";

/// Waits until the checks are over, prints the result table and returns the final rows.
pub async fn run(rows: Vec<Row>, mut updates: UnboundedReceiver<Update>) -> Vec<Row> {
    let mut rows = rows;
    while let Some(update) = updates.recv().await {
        rows = apply_update(rows, update);
    }

    let rows = fail_unfinished(rows, "timed out");
    print!("{}", format_table(&rows));
    rows
}

pub fn format_table(rows: &[Row]) -> String {
    let body: Vec<[String; 4]> = rows
        .iter()
        .map(|row| {
            [
                row.name.clone(),
                row.requirement.clone(),
                version_cell(&row.stage).text,
                maintenance_cell(&row.stage).text,
            ]
        })
        .collect();

    let widths: [usize; 4] = std::array::from_fn(|column| {
        body.iter()
            .map(|cells| cells[column].chars().count())
            .chain(std::iter::once(COLUMN_HEADERS[column].len()))
            .max()
            .unwrap_or(0)
    });

    let header = COLUMN_HEADERS.map(str::to_owned);
    std::iter::once(&header)
        .chain(&body)
        .map(|cells| format_line(cells, &widths))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn format_line(cells: &[String; 4], widths: &[usize; 4]) -> String {
    let line = cells
        .iter()
        .zip(widths)
        .map(|(cell, width)| format!("{cell:<width$}"))
        .collect::<Vec<_>>()
        .join(COLUMN_GAP);

    line.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Maintenance, Report, Stage};
    use semver::Version;

    #[test]
    fn formats_aligned_table() {
        let rows = vec![
            Row {
                stage: Stage::Done(Report {
                    new_major: Some(Version::new(2, 0, 0)),
                    maintenance: Maintenance::Archived,
                }),
                ..Row::pending("serde", "1.0")
            },
            Row {
                stage: Stage::Failed("crate not found on crates.io".into()),
                ..Row::pending("a-much-longer-name", "0.3")
            },
        ];

        let table = format_table(&rows);

        let expected = "\
Dependency          Requirement  New Major Version  Maintenance Status
serde               1.0          2.0.0 available    archived
a-much-longer-name  0.3          -                  error: crate not found on crates.io
";
        assert_eq!(table, expected);
    }

    #[tokio::test]
    async fn run_applies_updates_and_fails_unfinished_rows() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(Update {
            index: 0,
            stage: Stage::Done(Report {
                new_major: None,
                maintenance: Maintenance::NoGithubRepo,
            }),
        })
        .unwrap();
        drop(tx);

        let rows = run(vec![Row::pending("a", "1"), Row::pending("b", "1")], rx).await;

        assert!(matches!(rows[0].stage, Stage::Done(_)));
        assert_eq!(rows[1].stage, Stage::Failed("timed out".into()));
    }
}
