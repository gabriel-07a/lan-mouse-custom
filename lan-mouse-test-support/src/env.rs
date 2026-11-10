//! Isolated filesystem environment for tests.
//!
//! Every [`TestEnv`] owns a unique temporary directory tree, so tests never
//! share `config.toml`, certificates, audit logs, sockets or portal tokens.
//! The helper deliberately does **not** mutate process-wide environment
//! variables: tests that need `HOME`/`XDG_*` set must use their serial group
//! and the helpers here to compute the paths.

use std::{
    fs,
    path::{Path, PathBuf},
};

use tempfile::TempDir;

/// A disposable per-test directory layout.
#[derive(Debug)]
pub struct TestEnv {
    root: TempDir,
    label: String,
}

impl TestEnv {
    /// Create the directory layout for one test.
    ///
    /// The label only appears in diagnostics; uniqueness comes from
    /// `tempfile::TempDir`.
    pub fn new(label: impl Into<String>) -> Self {
        let root = tempfile::Builder::new()
            .prefix("lan-mouse-test-")
            .tempdir()
            .expect("create test environment");
        let label = label.into();
        let env = Self { root, label };
        for dir in [
            env.home(),
            env.config_dir(),
            env.state_dir(),
            env.cache_dir(),
            env.runtime_dir(),
        ] {
            fs::create_dir_all(dir).expect("create test directory");
        }
        env
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Root of the isolated tree.
    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    pub fn config_dir(&self) -> PathBuf {
        self.root.path().join("config")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.path().join("state")
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.root.path().join("cache")
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.root.path().join("runtime")
    }

    /// Path for a file below the environment root, creating parents.
    pub fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
        let path = self.root.path().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        path
    }

    /// Write a text file below the environment root.
    pub fn write(&self, relative: impl AsRef<Path>, contents: &str) -> PathBuf {
        let path = self.path(relative);
        fs::write(&path, contents).expect("write test file");
        path
    }

    /// Read a text file below the environment root.
    pub fn read(&self, relative: impl AsRef<Path>) -> String {
        fs::read_to_string(self.root.path().join(relative)).expect("read test file")
    }

    /// Keep the directory on disk and return its path (for debugging a failing
    /// test). Without this the `Drop` removes it.
    pub fn keep(self) -> PathBuf {
        self.root.keep()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separate_environments_do_not_share_files() {
        let a = TestEnv::new("a");
        let b = TestEnv::new("b");
        a.write("config.toml", "a");
        assert_eq!(a.read("config.toml"), "a");
        assert!(!b.root().join("config.toml").exists());
    }

    #[test]
    fn write_creates_parent_directories() {
        let env = TestEnv::new("nested");
        let path = env.write("a/b/c.txt", "data");
        assert_eq!(fs::read_to_string(path).expect("read"), "data");
    }
}
