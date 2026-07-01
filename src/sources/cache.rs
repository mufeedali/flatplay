use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Content-addressed download cache under `.flatplay/cache`.
#[derive(Debug, Clone)]
pub struct DownloadCache {
    root: PathBuf,
}

impl DownloadCache {
    pub fn new(build_dir: &Path) -> Self {
        Self {
            root: build_dir.join("cache"),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path for a cached blob keyed by sha256 (and a stable filename suffix).
    pub fn cached_path(&self, filename: &str, sha256: &str) -> PathBuf {
        let safe_name = filename.replace('/', "_");
        self.root.join(sha256).join(safe_name)
    }

    /// Path keyed only by URL/path hash when no sha256 is known yet.
    pub fn path_for_url(&self, url_or_path: &str) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(url_or_path.as_bytes());
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(64);
        for byte in digest {
            use std::fmt::Write;
            let _ = write!(&mut hex, "{byte:02x}");
        }
        let name = Path::new(url_or_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("blob");
        self.root.join("by-url").join(hex).join(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_paths_include_hash() {
        let cache = DownloadCache::new(Path::new("/tmp/proj/.flatplay"));
        let p = cache.cached_path("foo.tar.gz", "abc123");
        assert_eq!(
            p,
            PathBuf::from("/tmp/proj/.flatplay/cache/abc123/foo.tar.gz")
        );
    }
}
