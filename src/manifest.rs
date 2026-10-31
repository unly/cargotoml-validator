use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use thiserror::Error;
use toml::{Table, Value};

/// A registry dependency declared in a `Cargo.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencySpec {
    /// Crate name on the registry (honours `package = "..."` renames).
    pub name: String,
    pub requirement: String,
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
}

const DEPENDENCY_TABLES: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

/// Keys that mark a dependency as not coming from crates.io.
const NON_REGISTRY_KEYS: [&str; 3] = ["git", "registry", "registry-index"];

pub fn load(path: &Path) -> Result<Vec<DependencySpec>, ManifestError> {
    let display = path.display().to_string();
    let contents = fs::read_to_string(path).map_err(|source| ManifestError::Read {
        path: display.clone(),
        source,
    })?;
    parse(&contents).map_err(|source| ManifestError::Parse {
        path: display,
        source,
    })
}

/// Collects the direct registry dependencies of a manifest, sorted by crate name.
///
/// Covers `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`,
/// their `[target.*]` variants and `[workspace.dependencies]`. Git, path-only,
/// alternative-registry and `workspace = true` entries are skipped. When a
/// crate is declared several times, the first requirement wins.
pub fn parse(contents: &str) -> Result<Vec<DependencySpec>, toml::de::Error> {
    let doc: Table = contents.parse()?;

    let target_scopes = doc
        .get("target")
        .and_then(Value::as_table)
        .into_iter()
        .flat_map(|targets| targets.values().filter_map(Value::as_table));
    let workspace_scope = doc.get("workspace").and_then(Value::as_table);

    let scopes = std::iter::once(&doc)
        .chain(workspace_scope)
        .chain(target_scopes);

    let mut unique: BTreeMap<String, String> = BTreeMap::new();
    for spec in scopes
        .flat_map(dependency_tables)
        .flat_map(|table| table.iter())
        .filter_map(|(key, value)| spec_from_entry(key, value))
    {
        unique.entry(spec.name).or_insert(spec.requirement);
    }

    Ok(unique
        .into_iter()
        .map(|(name, requirement)| DependencySpec { name, requirement })
        .collect())
}

fn dependency_tables(scope: &Table) -> impl Iterator<Item = &Table> {
    DEPENDENCY_TABLES
        .iter()
        .filter_map(|key| scope.get(*key).and_then(Value::as_table))
}

fn spec_from_entry(key: &str, value: &Value) -> Option<DependencySpec> {
    match value {
        Value::String(requirement) => Some(DependencySpec {
            name: key.to_owned(),
            requirement: requirement.clone(),
        }),
        Value::Table(table) => spec_from_table(key, table),
        _ => None,
    }
}

fn spec_from_table(key: &str, table: &Table) -> Option<DependencySpec> {
    let inherited = table.get("workspace").and_then(Value::as_bool) == Some(true);
    if inherited || NON_REGISTRY_KEYS.iter().any(|k| table.contains_key(*k)) {
        return None;
    }

    let requirement = table.get("version").and_then(Value::as_str)?;
    let name = table.get("package").and_then(Value::as_str).unwrap_or(key);

    Some(DependencySpec {
        name: name.to_owned(),
        requirement: requirement.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, requirement: &str) -> DependencySpec {
        DependencySpec {
            name: name.to_owned(),
            requirement: requirement.to_owned(),
        }
    }

    #[test]
    fn parses_simple_and_table_dependencies() {
        let specs = parse(
            r#"
            [dependencies]
            serde = { version = "1.0", features = ["derive"] }
            anyhow = "1"
            "#,
        )
        .unwrap();

        assert_eq!(specs, vec![spec("anyhow", "1"), spec("serde", "1.0")]);
    }

    #[test]
    fn includes_dev_build_target_and_workspace_dependencies() {
        let specs = parse(
            r#"
            [dependencies]
            a = "1"
            [dev-dependencies]
            b = "2"
            [build-dependencies]
            c = "3"
            [target.'cfg(unix)'.dependencies]
            d = "4"
            [target.'cfg(windows)'.dev-dependencies]
            e = "5"
            [workspace.dependencies]
            f = "6"
            "#,
        )
        .unwrap();

        let names: Vec<_> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c", "d", "e", "f"]);
    }

    #[test]
    fn skips_non_registry_and_inherited_dependencies() {
        let specs = parse(
            r#"
            [dependencies]
            from_git = { git = "https://github.com/a/b" }
            from_git_versioned = { git = "https://github.com/a/b", version = "1" }
            local = { path = "../local" }
            inherited = { workspace = true }
            private = { version = "1", registry = "corp" }
            kept = "1"
            "#,
        )
        .unwrap();

        assert_eq!(specs, vec![spec("kept", "1")]);
    }

    #[test]
    fn keeps_path_dependencies_that_declare_a_version() {
        let specs =
            parse(r#"dependencies = { local = { path = "../x", version = "0.3" } }"#).unwrap();

        assert_eq!(specs, vec![spec("local", "0.3")]);
    }

    #[test]
    fn resolves_package_renames() {
        let specs = parse(
            r#"
            [dependencies]
            json = { package = "serde_json", version = "1" }
            "#,
        )
        .unwrap();

        assert_eq!(specs, vec![spec("serde_json", "1")]);
    }

    #[test]
    fn first_requirement_wins_for_duplicates() {
        let specs = parse(
            r#"
            [dependencies]
            tokio = "1.0"
            [dev-dependencies]
            tokio = "0.2"
            "#,
        )
        .unwrap();

        assert_eq!(specs, vec![spec("tokio", "1.0")]);
    }

    #[test]
    fn manifest_without_dependencies_yields_nothing() {
        assert!(parse("[package]\nname = \"x\"").unwrap().is_empty());
    }

    #[test]
    fn rejects_invalid_toml() {
        assert!(parse("[dependencies\n").is_err());
    }

    #[test]
    fn load_reports_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let err = load(&dir.path().join("Cargo.toml")).unwrap_err();

        assert!(matches!(err, ManifestError::Read { .. }));
    }

    #[test]
    fn load_reads_file_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Cargo.toml");
        fs::write(&path, "[dependencies]\nserde = \"1\"\n").unwrap();

        assert_eq!(load(&path).unwrap(), vec![spec("serde", "1")]);
    }
}
