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

/// Pairs of documents describing one API in both dialects. Committed, hand-written, hermetic.
pub fn convergence_root() -> Utf8PathBuf {
    corpus_root().join("convergence")
}

/// The workspace's own edition, which progeny writes into every generated crate too.
///
/// Read from the root manifest rather than restated, so a harness that assembles a crate by hand
/// compiles it under the edition the generated crates use.
pub fn workspace_edition() -> color_eyre::eyre::Result<String> {
    let manifest = workspace_root().join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)?;
    let parsed: toml::Table = toml::from_str(&text)?;
    parsed
        .get("workspace")
        .and_then(|workspace| workspace.get("package"))
        .and_then(|package| package.get("edition"))
        .and_then(toml::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| color_eyre::eyre::eyre!("{manifest} sets no `workspace.package.edition`"))
}

pub fn library_root() -> Utf8PathBuf {
    workspace_root().join("crates/progeny")
}
