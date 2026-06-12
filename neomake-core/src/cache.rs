//! Content-addressable cache for task outputs.
//!
//! # Cache-key derivation
//!
//! A task's cache key is the SHA-256 of a deterministic byte stream,
//! **format version 1** (documented here because any change to the
//! layout silently invalidates every existing cache entry, so we keep
//! it explicit and easy to evolve):
//!
//! ```text
//! "neomake-cache-v1\n"
//! "command:"    <command-bytes>                "\n"
//! "env:"        for each (K=V) in BTreeMap:    K "=" V "\n"
//!               "."                            "\n"
//! "deps:"       for each upstream dep_key:     <hex>        "\n"
//!               "."                            "\n"
//! "inputs:"     for each (relpath, sha256):    relpath ":" <hex> "\n"
//!               "."                            "\n"
//! ```
//!
//! The terminating `.` lines let us distinguish empty sections from a
//! section whose single element is the literal byte sequence that would
//! otherwise precede the next header — i.e., they make the encoding
//! unambiguous.
//!
//! The per-input hash is the SHA-256 of the file's *contents* (not its
//! mtime), which is what makes the cache robust against trivial
//! touch-based rebuilds.
//!
//! # Entries on disk
//!
//! Each cache hit/miss writes a JSON entry to
//! `<project>/.neomake/cache/<hex>.json` describing the outputs produced.
//! On lookup we verify every declared output file still exists and still
//! hashes to the recorded value; if not, the hit is downgraded to a miss.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::error::CacheError;
use crate::task::Task;

/// Cache-layout version. Bump when the on-disk format changes.
///
/// **v2** normalizes path separators in the hashed input list to `/` so
/// the same project produces identical cache keys on Windows and on
/// POSIX hosts. Existing v1 entries from previous neomake builds will
/// no longer match and can be removed via `neomake cache clean`.
pub const CACHE_FORMAT_VERSION: &str = "neomake-cache-v2";

/// Content-addressable cache rooted at `<project>/.neomake/cache/`.
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
    dir: PathBuf,
}

impl Cache {
    /// Open (or lazily-create) a cache tied to the given project root.
    pub fn open(project_root: impl Into<PathBuf>) -> Result<Self, CacheError> {
        let root = project_root.into();
        let dir = root.join(".neomake").join("cache");
        std::fs::create_dir_all(&dir).map_err(|e| CacheError::Io {
            path: dir.clone(),
            source: e,
        })?;
        Ok(Self { root, dir })
    }

    /// Project root the cache was opened against.
    pub fn project_root(&self) -> &Path {
        &self.root
    }

    /// Directory where cache entries are stored.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Derive the cache key for `task`, given already-computed upstream
    /// dep keys (in dependency declaration order).
    pub fn compute_key(&self, task: &Task, dep_keys: &[String]) -> Result<String, CacheError> {
        let mut hasher = Sha256::new();
        hasher.update(CACHE_FORMAT_VERSION.as_bytes());
        hasher.update(b"\n");

        hasher.update(b"command:");
        hasher.update(task.command.as_bytes());
        hasher.update(b"\n");

        hasher.update(b"env:\n");
        for (k, v) in &task.env {
            hasher.update(k.as_bytes());
            hasher.update(b"=");
            hasher.update(v.as_bytes());
            hasher.update(b"\n");
        }
        hasher.update(b".\n");

        hasher.update(b"deps:\n");
        for dk in dep_keys {
            hasher.update(dk.as_bytes());
            hasher.update(b"\n");
        }
        hasher.update(b".\n");

        hasher.update(b"inputs:\n");
        let inputs = expand_globs(&self.root, &task.inputs)?;
        let manifest = crate::asset::AssetManifest::load(&self.root);
        for (rel, abs) in &inputs {
            let content_hash = if let Some(asset) = manifest.assets.get(abs) {
                match &asset.kind {
                    crate::asset::AssetKind::Git { commit_sha } => commit_sha.clone(),
                    crate::asset::AssetKind::Fetch { content_sha256 } => content_sha256.clone(),
                }
            } else {
                hash_file(abs)?
            };
            hasher.update(normalize_rel(rel).as_bytes());
            hasher.update(b":");
            hasher.update(content_hash.as_bytes());
            hasher.update(b"\n");
        }
        hasher.update(b".\n");

        Ok(hex::encode(hasher.finalize()))
    }

    /// Attempt a cache lookup for `task` with the given computed key.
    ///
    /// Returns `Ok(Some(_))` only when a persisted entry exists *and*
    /// every declared output file still matches its recorded hash.
    pub fn lookup(&self, key: &str) -> Result<Option<CacheEntry>, CacheError> {
        let path = self.entry_path(key);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(CacheError::Io {
                    path: path.clone(),
                    source: e,
                })
            }
        };
        let entry: CacheEntry =
            serde_json::from_slice(&bytes).map_err(|e| CacheError::Corrupt {
                path: path.clone(),
                message: e.to_string(),
            })?;

        for out in &entry.outputs {
            let abs = self.root.join(&out.path);
            match hash_file(&abs) {
                Ok(h) if h == out.sha256 => {}
                _ => return Ok(None),
            }
        }
        Ok(Some(entry))
    }

    /// Persist a cache entry for `task` after a successful run.
    pub fn store(&self, task: &Task, key: &str, exit_code: i32) -> Result<CacheEntry, CacheError> {
        let outputs = expand_globs(&self.root, &task.outputs)?;
        let mut records = Vec::with_capacity(outputs.len());
        for (rel, abs) in outputs {
            let sha256 = hash_file(&abs)?;
            let size = std::fs::metadata(&abs)
                .map_err(|e| CacheError::Io {
                    path: abs.clone(),
                    source: e,
                })?
                .len();
            records.push(OutputRecord {
                path: normalize_rel(&rel),
                sha256,
                size,
            });
        }
        let entry = CacheEntry {
            key: key.to_string(),
            task: task.name.clone(),
            outputs: records,
            exit_code,
            finished_at: now_unix(),
        };
        let path = self.entry_path(key);
        let json = serde_json::to_vec_pretty(&entry).map_err(|e| CacheError::Corrupt {
            path: path.clone(),
            message: e.to_string(),
        })?;
        std::fs::write(&path, json).map_err(|e| CacheError::Io {
            path: path.clone(),
            source: e,
        })?;
        Ok(entry)
    }

    /// Delete every entry in the cache.
    pub fn clean(&self) -> Result<(), CacheError> {
        if !self.dir.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(&self.dir).map_err(|e| CacheError::Io {
            path: self.dir.clone(),
            source: e,
        })? {
            let entry = entry.map_err(|e| CacheError::Io {
                path: self.dir.clone(),
                source: e,
            })?;
            let p = entry.path();
            if p.is_file() {
                std::fs::remove_file(&p).map_err(|e| CacheError::Io {
                    path: p.clone(),
                    source: e,
                })?;
            }
        }
        Ok(())
    }

    /// Summarize cache contents.
    pub fn status(&self) -> Result<CacheStatus, CacheError> {
        let mut entries = 0usize;
        let mut bytes = 0u64;
        if !self.dir.exists() {
            return Ok(CacheStatus { entries, bytes });
        }
        for entry in std::fs::read_dir(&self.dir).map_err(|e| CacheError::Io {
            path: self.dir.clone(),
            source: e,
        })? {
            let entry = entry.map_err(|e| CacheError::Io {
                path: self.dir.clone(),
                source: e,
            })?;
            if entry.path().extension().and_then(|s| s.to_str()) == Some("json") {
                entries += 1;
                bytes += entry
                    .metadata()
                    .map(|m| m.len())
                    .map_err(|e| CacheError::Io {
                        path: entry.path(),
                        source: e,
                    })?;
            }
        }
        Ok(CacheStatus { entries, bytes })
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }
}

/// Summary returned by [`Cache::status`].
#[derive(Debug, Clone, Copy)]
pub struct CacheStatus {
    /// Number of entries currently on disk.
    pub entries: usize,
    /// Total size of those entries in bytes.
    pub bytes: u64,
}

/// A persisted cache record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// Content-addressed key (hex SHA-256).
    pub key: String,
    /// Task name this entry was produced by.
    pub task: String,
    /// Record of every output file that was present after the task ran.
    pub outputs: Vec<OutputRecord>,
    /// Exit code reported by the task's command.
    pub exit_code: i32,
    /// Seconds since the Unix epoch at which the entry was written.
    pub finished_at: u64,
}

/// One output file captured in a [`CacheEntry`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputRecord {
    /// Path relative to the project root.
    pub path: String,
    /// Hex SHA-256 of the file's contents.
    pub sha256: String,
    /// File size in bytes.
    pub size: u64,
}

fn hash_file(path: &Path) -> Result<String, CacheError> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| CacheError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| CacheError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Expand a list of glob patterns relative to `root` into a sorted list
/// of `(relative_path, absolute_path)` pairs.
fn expand_globs(root: &Path, patterns: &[String]) -> Result<Vec<(PathBuf, PathBuf)>, CacheError> {
    use globset::{GlobBuilder, GlobSetBuilder};

    if patterns.is_empty() {
        return Ok(Vec::new());
    }

    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        let glob = GlobBuilder::new(p)
            .literal_separator(true)
            .build()
            .map_err(|e| CacheError::Corrupt {
                path: root.to_path_buf(),
                message: format!("invalid glob `{p}`: {e}"),
            })?;
        builder.add(glob);
    }
    let set = builder.build().map_err(|e| CacheError::Corrupt {
        path: root.to_path_buf(),
        message: format!("glob set build failed: {e}"),
    })?;

    let mut found: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    for entry in WalkDir::new(root).follow_links(false).into_iter() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        // Skip the cache directory itself to avoid circular hashing.
        let rel = match entry.path().strip_prefix(root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        if rel.starts_with(".neomake") {
            continue;
        }
        // Globset patterns use `/` separators; on Windows, `WalkDir`
        // produces `\\`-separated relative paths. Match against a
        // normalized form so portable patterns work on every host.
        let rel_match = normalize_rel(&rel);
        if set.is_match(&rel_match) {
            found.insert(rel.clone(), entry.path().to_path_buf());
        }
    }
    Ok(found.into_iter().collect())
}

/// Render a relative path using `/` as the separator regardless of host
/// OS. Used to keep glob matching and cache keys portable.
fn normalize_rel(rel: &Path) -> String {
    let s = rel.to_string_lossy();
    if std::path::MAIN_SEPARATOR == '/' {
        s.into_owned()
    } else {
        s.replace(std::path::MAIN_SEPARATOR, "/")
    }
}

fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn mk_task(name: &str, command: &str, inputs: &[&str], outputs: &[&str]) -> Task {
        Task {
            name: name.into(),
            command: command.into(),
            deps: vec![],
            inputs: inputs.iter().map(|s| s.to_string()).collect(),
            outputs: outputs.iter().map(|s| s.to_string()).collect(),
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn key_is_stable_across_calls() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let task = mk_task("t", "cat a.txt", &["a.txt"], &[]);
        let k1 = cache.compute_key(&task, &[]).unwrap();
        let k2 = cache.compute_key(&task, &[]).unwrap();
        assert_eq!(k1, k2);
    }

    #[test]
    fn key_changes_when_input_changes() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let task = mk_task("t", "cat a.txt", &["a.txt"], &[]);
        let k1 = cache.compute_key(&task, &[]).unwrap();
        fs::write(tmp.path().join("a.txt"), b"world!").unwrap();
        let k2 = cache.compute_key(&task, &[]).unwrap();
        assert_ne!(k1, k2);
    }

    #[test]
    fn key_changes_when_command_or_env_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let mut t1 = mk_task("t", "echo hi", &[], &[]);
        let k1 = cache.compute_key(&t1, &[]).unwrap();
        t1.command = "echo HI".into();
        let k2 = cache.compute_key(&t1, &[]).unwrap();
        assert_ne!(k1, k2);
        t1.command = "echo hi".into();
        t1.env.insert("X".into(), "1".into());
        let k3 = cache.compute_key(&t1, &[]).unwrap();
        assert_ne!(k1, k3);
    }

    #[test]
    fn key_propagates_upstream_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let t = mk_task("t", "echo", &[], &[]);
        let k1 = cache.compute_key(&t, &["aaaa".into()]).unwrap();
        let k2 = cache.compute_key(&t, &["bbbb".into()]).unwrap();
        assert_ne!(k1, k2);
    }

    #[test]
    fn store_and_lookup_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("out.txt"), b"result").unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let task = mk_task("t", "cp src out.txt", &[], &["out.txt"]);
        let key = cache.compute_key(&task, &[]).unwrap();
        cache.store(&task, &key, 0).unwrap();
        let hit = cache.lookup(&key).unwrap();
        assert!(hit.is_some());
    }

    #[test]
    fn lookup_miss_when_output_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("out.txt"), b"r").unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let task = mk_task("t", "cp", &[], &["out.txt"]);
        let key = cache.compute_key(&task, &[]).unwrap();
        cache.store(&task, &key, 0).unwrap();
        fs::remove_file(tmp.path().join("out.txt")).unwrap();
        let hit = cache.lookup(&key).unwrap();
        assert!(hit.is_none());
    }

    #[test]
    fn lookup_miss_when_output_mutated() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("out.txt"), b"r").unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let task = mk_task("t", "cp", &[], &["out.txt"]);
        let key = cache.compute_key(&task, &[]).unwrap();
        cache.store(&task, &key, 0).unwrap();
        fs::write(tmp.path().join("out.txt"), b"tampered").unwrap();
        let hit = cache.lookup(&key).unwrap();
        assert!(hit.is_none());
    }

    #[test]
    fn clean_removes_entries() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("o"), b"x").unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let t = mk_task("t", "echo", &[], &["o"]);
        let k = cache.compute_key(&t, &[]).unwrap();
        cache.store(&t, &k, 0).unwrap();
        assert!(cache.status().unwrap().entries >= 1);
        cache.clean().unwrap();
        assert_eq!(cache.status().unwrap().entries, 0);
    }
}
