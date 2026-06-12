use reqwest::blocking::Client;
use reqwest::header::CONTENT_LENGTH;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// terminal progress bar for downloads
pub struct ProgressBar {
    start: Instant,
    total_size: Option<u64>,
    downloaded: u64,
}

impl ProgressBar {
    /// create a new progress bar
    pub fn new(total_size: Option<u64>) -> Self {
        Self {
            start: Instant::now(),
            total_size,
            downloaded: 0,
        }
    }

    /// update the progress bar with downloaded bytes
    pub fn update(&mut self, bytes: u64) {
        self.downloaded += bytes;
        let elapsed = self.start.elapsed().as_secs_f64();
        let speed = if elapsed > 0.0 {
            (self.downloaded as f64 / 1_048_576.0) / elapsed
        } else {
            0.0
        };

        let speed_str = format!("{:.2} MB/s", speed);
        let mut out = std::io::stdout();

        if let Some(total) = self.total_size {
            let percent = (self.downloaded as f64 / total as f64).clamp(0.0, 1.0);
            let bar_len = 14;
            let filled = (percent * bar_len as f64).round() as usize;
            let mut bar = String::new();
            for _ in 0..filled {
                bar.push('—');
            }
            for _ in filled..bar_len {
                bar.push('—');
            }

            let eta_str = if speed > 0.0 {
                let remaining_bytes = total.saturating_sub(self.downloaded);
                let remaining_secs = (remaining_bytes as f64 / 1_048_576.0) / speed;
                let mins = (remaining_secs / 60.0).floor() as u64;
                let secs = (remaining_secs % 60.0).floor() as u64;
                format!("{:02}:{:02}", mins, secs)
            } else {
                "??:??".to_string()
            };

            write!(out, "\r[{}] {} ETA {}", bar, speed_str, eta_str).unwrap();
        } else {
            let downloaded_mb = self.downloaded as f64 / 1_048_576.0;
            let mut bar = String::new();
            for _ in 0..14 {
                bar.push('—');
            }
            write!(out, "\r[{}] {:.2} MB {} ETA ??:??", bar, downloaded_mb, speed_str).unwrap();
        }
        out.flush().unwrap();
    }

    /// finish progress bar rendering
    pub fn finish(&self) {
        println!();
    }
}

/// extract the base name of a url or repo path
pub fn extract_basename(url_or_repo: &str) -> String {
    // Basic basename extraction
    let mut name = if let Some(idx) = url_or_repo.rfind('/') {
        &url_or_repo[idx + 1..]
    } else {
        url_or_repo
    };
    if let Some(stripped) = name.strip_suffix(".git") {
        name = stripped;
    }
    name.to_string()
}

/// resolve `%` in destination template with base name
pub fn resolve_wildcard(dest: &str, basename: &str, alias: Option<&str>) -> String {
    let name = alias.unwrap_or(basename);
    dest.replace("%", name)
}

/// download a file over http with progress tracking and sha validation
pub fn http_fetch(
    url: &str,
    dest_str: &str,
    sha256_expected: Option<&str>,
    extract: bool,
) -> Result<(PathBuf, String), String> {
    let dest_path = Path::new(dest_str);
    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("failed to create dir: {e}"))?;
    }

    let client = Client::builder()
        .build()
        .map_err(|e| format!("failed to create http client: {e}"))?;

    let mut response = client
        .get(url)
        .send()
        .map_err(|e| format!("http request failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("http request failed with status: {}", response.status()));
    }

    let total_size = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s: &str| s.parse::<u64>().ok());

    let mut pb = ProgressBar::new(total_size);
    let mut temp_path = dest_path.to_path_buf();
    temp_path.set_extension("downloading");

    let mut file = File::create(&temp_path).map_err(|e| format!("failed to create temp file: {e}"))?;
    let mut buffer = [0; 32 * 1024]; // 32KB chunk size
    let mut hasher = Sha256::new();

    println!("Downloading {}...", url);
    loop {
        let n = response.read(&mut buffer).map_err(|e| format!("download error: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buffer[..n]).map_err(|e| format!("write error: {e}"))?;
        hasher.update(&buffer[..n]);
        pb.update(n as u64);
    }
    pb.finish();

    let computed_hash = hex::encode(hasher.finalize());
    if let Some(expected) = sha256_expected {
        if expected != computed_hash {
            let _ = std::fs::remove_file(&temp_path);
            return Err(format!("hash mismatch! expected {expected}, got {computed_hash}"));
        }
    }

    std::fs::rename(&temp_path, dest_path).map_err(|e| format!("failed to rename file: {e}"))?;

    if extract {
        let parent = dest_path.parent().unwrap_or_else(|| Path::new("."));
        let ext = dest_path.extension().and_then(|s| s.to_str()).unwrap_or("");
        println!("Extracting to {}...", parent.display());
        if dest_path.to_string_lossy().ends_with(".tar.gz") || dest_path.to_string_lossy().ends_with(".tgz") {
            let tar_gz = File::open(dest_path).map_err(|e| format!("failed to open tarball: {e}"))?;
            let tar = flate2::read::GzDecoder::new(tar_gz);
            let mut archive = tar::Archive::new(tar);
            archive.unpack(parent).map_err(|e| format!("failed to extract tar.gz: {e}"))?;
        } else if dest_path.to_string_lossy().ends_with(".tar.xz") {
            let tar_xz = File::open(dest_path).map_err(|e| format!("failed to open tar.xz: {e}"))?;
            let tar = xz2::read::XzDecoder::new(tar_xz);
            let mut archive = tar::Archive::new(tar);
            archive.unpack(parent).map_err(|e| format!("failed to extract tar.xz: {e}"))?;
        } else {
            return Err(format!("unsupported archive extension: {}", ext));
        }
    }

    Ok((dest_path.to_path_buf(), computed_hash))
}
/// resolve a git repository string into a cloneable url
pub fn resolve_git_url(repo: &str) -> String {
    if repo.contains("://") || repo.starts_with("git@") {
        repo.to_string()
    } else if let Some((provider, path)) = repo.split_once(':') {
        match provider.to_lowercase().as_str() {
            "github" | "gh" => format!("https://github.com/{}", path),
            "gitlab" | "gl" => format!("https://gitlab.com/{}", path),
            "codeberg" | "cb" => format!("https://codeberg.org/{}", path),
            "bitbucket" | "bb" => format!("https://bitbucket.org/{}", path),
            _ => format!("https://github.com/{}", repo),
        }
    } else {
        format!("https://github.com/{}", repo)
    }
}

/// clone a git repository and check out a tag/branch
pub fn git_clone(
    repo: &str,
    tag: &str,
    dest_str: &str,
    submodules: bool,
    depth: Option<u32>,
) -> Result<(PathBuf, String), String> {
    let dest_path = Path::new(dest_str);
    
    if dest_path.exists() {
        let r = git2::Repository::open(dest_path).map_err(|e| format!("failed to open existing repo: {e}"))?;
        let head = r.head().map_err(|e| format!("failed to get HEAD: {e}"))?;
        let commit = head.peel_to_commit().map_err(|e| format!("failed to peel commit: {e}"))?;
        return Ok((dest_path.to_path_buf(), commit.id().to_string()));
    }

    let url = resolve_git_url(repo);

    println!("Cloning {}...", url);
    let mut builder = git2::build::RepoBuilder::new();
    let mut fetch_opts = git2::FetchOptions::new();
    if let Some(d) = depth {
        println!("note: libgit2 shallow clones are not fully supported, doing full clone.");
    }
    
    let repo_obj = builder.fetch_options(fetch_opts).clone(&url, dest_path).map_err(|e| format!("git clone failed: {e}"))?;

    let obj = repo_obj.revparse_single(tag).map_err(|e| format!("failed to find tag/rev {}: {}", tag, e))?;
    repo_obj.checkout_tree(&obj, None).map_err(|e| format!("checkout failed: {e}"))?;
    repo_obj.set_head_detached(obj.id()).map_err(|e| format!("set head failed: {e}"))?;

    let commit_sha = obj.id().to_string();

    if submodules {
        for mut sub in repo_obj.submodules().map_err(|e| format!("submodules error: {e}"))? {
            let mut sub_opts = git2::SubmoduleUpdateOptions::new();
            sub.update(true, Some(&mut sub_opts)).map_err(|e| format!("submodule update failed: {e}"))?;
        }
    }

    Ok((dest_path.to_path_buf(), commit_sha))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_basename() {
        assert_eq!(extract_basename("https://github.com/foo/bar"), "bar");
        assert_eq!(extract_basename("https://github.com/foo/bar.git"), "bar");
        assert_eq!(extract_basename("foo/bar.git"), "bar");
        assert_eq!(extract_basename("bar.git"), "bar");
        assert_eq!(extract_basename("bar"), "bar");
    }

    #[test]
    fn test_resolve_git_url() {
        assert_eq!(resolve_git_url("https://github.com/foo/bar"), "https://github.com/foo/bar");
        assert_eq!(resolve_git_url("git@github.com:foo/bar.git"), "git@github.com:foo/bar.git");
        assert_eq!(resolve_git_url("git://github.com/foo/bar"), "git://github.com/foo/bar");

        assert_eq!(resolve_git_url("github:foo/bar"), "https://github.com/foo/bar");
        assert_eq!(resolve_git_url("gh:foo/bar"), "https://github.com/foo/bar");
        assert_eq!(resolve_git_url("gitlab:foo/bar"), "https://gitlab.com/foo/bar");
        assert_eq!(resolve_git_url("gl:foo/bar"), "https://gitlab.com/foo/bar");
        assert_eq!(resolve_git_url("codeberg:foo/bar"), "https://codeberg.org/foo/bar");
        assert_eq!(resolve_git_url("cb:foo/bar"), "https://codeberg.org/foo/bar");
        assert_eq!(resolve_git_url("bitbucket:foo/bar"), "https://bitbucket.org/foo/bar");
        assert_eq!(resolve_git_url("bb:foo/bar"), "https://bitbucket.org/foo/bar");

        assert_eq!(resolve_git_url("foo/bar"), "https://github.com/foo/bar");

        assert_eq!(resolve_git_url("unknown:foo/bar"), "https://github.com/unknown:foo/bar");
    }
}
