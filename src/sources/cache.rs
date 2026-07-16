use std::path::{Path, PathBuf};

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

    /// Path for a cached blob keyed by sha256 (and a stable filename suffix).
    pub fn cached_path(&self, filename: &str, sha256: &str) -> PathBuf {
        let safe_name = filename.replace('/', "_");
        self.root.join(sha256).join(safe_name)
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
