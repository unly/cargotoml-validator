use std::collections::HashMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

const APP_DIR: &str = "cargotoml-validator";
const CACHE_FILE: &str = "cache.json";
#[cfg(unix)]
const DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const FILE_MODE: u32 = 0o600;

#[derive(Serialize, Deserialize)]
struct Entry {
    expires: DateTime<Utc>,
    value: serde_json::Value,
}

#[derive(Default, Serialize, Deserialize)]
struct Contents {
    entries: HashMap<String, Entry>,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    dirty: bool,
}

/// A small TTL cache that is persisted as JSON between runs.
///
/// Time is passed in by the caller so expiry stays deterministic. Anything
/// that cannot be read back (missing, corrupt or outdated file, values of an
/// unexpected shape) is simply a cache miss.
pub struct Cache {
    path: Option<PathBuf>,
    refresh: bool,
    state: Mutex<State>,
}

/// `<user cache dir>/cargotoml-validator/cache.json`, if the platform has a cache directory.
pub fn default_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|dir| dir.join(APP_DIR).join(CACHE_FILE))
}

impl Cache {
    /// Opens the cache at `path`, or an in-memory one for `None`. With
    /// `refresh`, stored entries are ignored on read but new results are still stored.
    pub fn open(path: Option<PathBuf>, refresh: bool) -> Self {
        let entries = path.as_deref().map(load).unwrap_or_default();
        Self {
            path,
            refresh,
            state: Mutex::new(State {
                entries,
                dirty: false,
            }),
        }
    }

    pub fn get<T: DeserializeOwned>(&self, key: &str, now: DateTime<Utc>) -> Option<T> {
        if self.refresh {
            return None;
        }

        let state = self.lock();
        let entry = state.entries.get(key).filter(|entry| now < entry.expires)?;
        serde_json::from_value(entry.value.clone()).ok()
    }

    pub fn put<T: Serialize>(&self, key: &str, value: &T, now: DateTime<Utc>, ttl: Duration) {
        let Ok(value) = serde_json::to_value(value) else {
            return;
        };

        let mut state = self.lock();
        state.entries.insert(
            key.to_owned(),
            Entry {
                expires: now + ttl,
                value,
            },
        );
        state.dirty = true;
    }

    /// Writes the cache to disk, dropping expired entries. Does nothing when
    /// nothing changed or the cache is in-memory only.
    pub fn save(&self, now: DateTime<Utc>) -> anyhow::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };

        let data = {
            let mut state = self.lock();
            if !state.dirty {
                return Ok(());
            }
            state.entries.retain(|_, entry| now < entry.expires);

            let contents = Contents {
                entries: std::mem::take(&mut state.entries),
            };
            let data = serde_json::to_vec(&contents);
            state.entries = contents.entries;
            data.context("failed to encode cache")?
        };

        write_atomic(path, &data)
            .with_context(|| format!("failed to write cache {}", path.display()))?;
        self.lock().dirty = false;
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn load(path: &Path) -> HashMap<String, Entry> {
    fs::read(path)
        .ok()
        .and_then(|data| serde_json::from_slice::<Contents>(&data).ok())
        .map(|contents| contents.entries)
        .unwrap_or_default()
}

/// Writes to a private temporary file next to `path`, then renames it into place.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    create_private_dir(dir)?;

    let tmp = dir.join(format!(".{CACHE_FILE}.{}.tmp", std::process::id()));
    let result = create_private_file(&tmp)
        .and_then(|mut file| file.write_all(data))
        .and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, DIR_MODE);
    builder.create(dir)
}

fn create_private_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, FILE_MODE);
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(timestamp: &str) -> DateTime<Utc> {
        timestamp.parse().unwrap()
    }

    fn now() -> DateTime<Utc> {
        at("2026-10-31T12:00:00Z")
    }

    fn hours(count: i64) -> Duration {
        Duration::hours(count)
    }

    fn cache_file(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("nested").join(CACHE_FILE)
    }

    #[test]
    fn returns_stored_values_until_they_expire() {
        let cache = Cache::open(None, false);
        cache.put("k", &"value", now(), hours(6));

        assert_eq!(
            cache.get::<String>("k", now() + hours(5)).as_deref(),
            Some("value")
        );
        assert_eq!(cache.get::<String>("k", now() + hours(6)), None);
        assert_eq!(cache.get::<String>("missing", now()), None);
    }

    #[test]
    fn values_of_an_unexpected_shape_are_a_miss() {
        let cache = Cache::open(None, false);
        cache.put("k", &"text", now(), hours(1));

        assert_eq!(cache.get::<u32>("k", now()), None);
    }

    #[test]
    fn refresh_ignores_stored_entries_but_still_stores_new_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_file(&dir);
        let first = Cache::open(Some(path.clone()), false);
        first.put("k", &1_u32, now(), hours(1));
        first.save(now()).unwrap();

        let refreshing = Cache::open(Some(path.clone()), true);
        assert_eq!(refreshing.get::<u32>("k", now()), None);
        refreshing.put("k", &2_u32, now(), hours(1));
        refreshing.save(now()).unwrap();

        let reopened = Cache::open(Some(path), false);
        assert_eq!(reopened.get::<u32>("k", now()), Some(2));
    }

    #[test]
    fn entries_survive_save_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_file(&dir);
        let cache = Cache::open(Some(path.clone()), false);
        cache.put("k", &vec![1_u32, 2], now(), hours(1));
        cache.save(now()).unwrap();

        let reopened = Cache::open(Some(path), false);

        assert_eq!(reopened.get::<Vec<u32>>("k", now()), Some(vec![1, 2]));
    }

    #[test]
    fn save_drops_expired_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_file(&dir);
        let cache = Cache::open(Some(path.clone()), false);
        cache.put("short", &1_u32, now(), hours(1));
        cache.put("long", &2_u32, now(), hours(24));

        cache.save(now() + hours(2)).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("short"));
        assert!(contents.contains("long"));
    }

    #[test]
    fn save_without_changes_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_file(&dir);

        Cache::open(Some(path.clone()), false).save(now()).unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn in_memory_cache_saves_nothing_and_does_not_fail() {
        let cache = Cache::open(None, false);
        cache.put("k", &1_u32, now(), hours(1));

        assert!(cache.save(now()).is_ok());
    }

    #[test]
    fn corrupt_file_is_treated_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        fs::write(&path, "{ not json").unwrap();

        let cache = Cache::open(Some(path.clone()), false);

        assert_eq!(cache.get::<u32>("k", now()), None);
        cache.put("k", &1_u32, now(), hours(1));
        cache.save(now()).unwrap();
        assert_eq!(
            Cache::open(Some(path), false).get::<u32>("k", now()),
            Some(1)
        );
    }

    #[test]
    fn failed_write_is_reported_with_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, "").unwrap();
        let cache = Cache::open(Some(blocker.join(CACHE_FILE)), false);
        cache.put("k", &1_u32, now(), hours(1));

        let err = cache.save(now()).unwrap_err();

        assert!(format!("{err:#}").contains("failed to write cache"));
    }

    #[cfg(unix)]
    #[test]
    fn cache_file_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = cache_file(&dir);
        let cache = Cache::open(Some(path.clone()), false);
        cache.put("k", &1_u32, now(), hours(1));

        cache.save(now()).unwrap();

        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), FILE_MODE);
        assert_eq!(mode(path.parent().unwrap()), DIR_MODE);
    }
}
