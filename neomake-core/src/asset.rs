use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// kinds of assets we can download
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AssetKind {
    /// a git repository clone
    Git {
        /// git commit hash
        commit_sha: String,
    },
    /// an http downloaded file
    Fetch {
        /// file sha256 checksum
        content_sha256: String,
    },
}

/// record of a downloaded asset
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetRecord {
    /// kind of this asset
    pub kind: AssetKind,
    /// source url or repo target
    pub source: String,
}

/// list of all known project assets
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AssetManifest {
    /// mapping from path to asset record
    pub assets: BTreeMap<PathBuf, AssetRecord>,
}

impl AssetManifest {
    /// load manifest from project root
    pub fn load(project_root: &Path) -> Self {
        let path = project_root.join(".neomake").join("assets.json");
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(manifest) = serde_json::from_slice(&bytes) {
                return manifest;
            }
        }
        Self::default()
    }

    /// save manifest to project root
    pub fn save(&self, project_root: &Path) {
        let dir = project_root.join(".neomake");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("assets.json");
        if let Ok(bytes) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(path, bytes);
        }
    }

    /// add a new asset to manifest
    pub fn add(&mut self, path: PathBuf, record: AssetRecord) {
        self.assets.insert(path, record);
    }
}
