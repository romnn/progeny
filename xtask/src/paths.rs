//! Where things are, relative to the workspace root.

use camino::{Utf8Path, Utf8PathBuf};

/// The workspace root.
///
/// Derived from this crate's own manifest directory at compile time, so it is right whatever
/// directory the task is invoked from.
pub fn workspace_root() -> Utf8PathBuf {
    let xtask = Utf8Path::new(env!("CARGO_MANIFEST_DIR"));
    xtask
        .parent()
        .map_or_else(|| xtask.to_owned(), Utf8Path::to_owned)
}

pub fn corpus_root() -> Utf8PathBuf {
    workspace_root().join("corpus")
}

/// Where fetched vendor documents live. Gitignored: they total roughly 117 MB and their
/// redistribution rights vary by publisher.
pub fn cache_root() -> Utf8PathBuf {
    corpus_root().join("cache")
}

/// Where the committed documents live.
pub fn specs_root() -> Utf8PathBuf {
    corpus_root().join("specs")
}

pub fn library_root() -> Utf8PathBuf {
    workspace_root().join("crates/progeny")
}
