//! Per-document diagnostics snapshots, keyed by the hash of the document they were taken from.
//!
//! A snapshot makes "what progeny says about this document" a reviewable file: a change that makes
//! the generator quietly degrade more shows up as a diff instead of as silence, and a repair
//! becoming full support deletes lines. That only works if a mismatch means one thing — and for a
//! corpus of documents that are *fetched* rather than committed, it does not: a vendor
//! republishing their description looks exactly like a regression.
//!
//! The hash is what separates the two. Same hash, different diagnostics: progeny changed, and that
//! is a finding. Different hash: upstream changed, and the snapshot needs re-baselining, which is
//! a review task rather than a failure. Without the split, snapshots for vendor documents would
//! have to be abandoned; with it they can be trusted.

use std::fmt::Write as _;

use camino::{Utf8Path, Utf8PathBuf};
use color_eyre::eyre::{self, WrapErr};
use sha2::{Digest, Sha256};

use crate::paths;

/// What comparing a document against its snapshot found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The document is the one the snapshot was taken from, and progeny still says the same thing.
    Match,
    /// No snapshot has been recorded yet.
    Missing,
    /// The same document, different diagnostics. The only outcome that is progeny's fault.
    Regressed {
        added: Vec<String>,
        removed: Vec<String>,
    },
    /// A different document. Re-baseline after reading the diff.
    Republished { diagnostics_changed: bool },
}

impl Verdict {
    /// Whether this verdict should fail the run.
    ///
    /// A republication is loud but not a failure: nobody can make a vendor stop editing their
    /// description, and treating it as a break would train people to ignore the gate.
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Missing | Self::Regressed { .. })
    }

    pub fn headline(&self) -> &'static str {
        match self {
            Self::Match => "matches",
            Self::Missing => "no snapshot recorded",
            Self::Regressed { .. } => "same document, different diagnostics",
            Self::Republished {
                diagnostics_changed: false,
            } => "upstream republished; diagnostics unchanged",
            Self::Republished {
                diagnostics_changed: true,
            } => "upstream republished; diagnostics changed too",
        }
    }
}

/// One recorded snapshot: which document, and what progeny said about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    document: String,
    lines: Vec<String>,
}

impl Snapshot {
    /// Take a snapshot of what progeny said about these bytes.
    ///
    /// Lines are sorted, so a diff reads as a behaviour change rather than as a reordering: the
    /// document order of diagnostics is an artefact of the walk, and the walk is allowed to change.
    pub fn take(document: &[u8], diagnostics: &[String]) -> Self {
        let mut lines = diagnostics.to_vec();
        lines.sort();
        Self {
            document: digest(document),
            lines,
        }
    }

    /// The file's contents: a header naming the document, then one line per diagnostic.
    fn render(&self) -> String {
        let mut out = String::new();
        // The header is itself a JSON line, so the whole file stays machine-readable.
        let _ = writeln!(
            out,
            "{{\"document\":\"{}\",\"diagnostics\":{}}}",
            self.document,
            self.lines.len()
        );
        for line in &self.lines {
            let _ = writeln!(out, "{line}");
        }
        out
    }

    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        let header: serde_json::Value = serde_json::from_str(lines.next()?).ok()?;
        Some(Self {
            document: header.get("document")?.as_str()?.to_owned(),
            lines: lines.map(ToOwned::to_owned).collect(),
        })
    }

    /// Compare against what was recorded for this document.
    pub fn compare(&self, recorded: Option<&Self>) -> Verdict {
        let Some(recorded) = recorded else {
            return Verdict::Missing;
        };
        if recorded.document != self.document {
            return Verdict::Republished {
                diagnostics_changed: recorded.lines != self.lines,
            };
        }
        if recorded.lines == self.lines {
            return Verdict::Match;
        }
        Verdict::Regressed {
            added: difference(&self.lines, &recorded.lines),
            removed: difference(&recorded.lines, &self.lines),
        }
    }
}

/// The lines in `these` that are not in `those`.
fn difference(taken: &[String], recorded: &[String]) -> Vec<String> {
    taken
        .iter()
        .filter(|line| !recorded.contains(line))
        .cloned()
        .collect()
}

pub fn root() -> Utf8PathBuf {
    paths::corpus_root().join("snapshots")
}

pub fn path_for(name: &str) -> Utf8PathBuf {
    root().join(format!("{name}.jsonl"))
}

pub fn read(name: &str) -> Option<Snapshot> {
    let text = std::fs::read_to_string(path_for(name)).ok()?;
    Snapshot::parse(&text)
}

pub fn write(name: &str, snapshot: &Snapshot) -> eyre::Result<()> {
    let path = path_for(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).wrap_err_with(|| format!("creating {parent}"))?;
    }
    std::fs::write(&path, snapshot.render()).wrap_err_with(|| format!("writing {path}"))
}

/// Snapshot files with no document behind them any more.
///
/// A document leaving the manifest should take its snapshot with it, or the directory slowly fills
/// with records of documents nobody runs.
pub fn orphans(known: &[String]) -> Vec<Utf8PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root()) else {
        return found;
    };
    for entry in entries.flatten() {
        let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
            continue;
        };
        if path.extension() != Some("jsonl") {
            continue;
        }
        let stem = path.file_stem().unwrap_or_default();
        if !known.iter().any(|name| name == stem) {
            found.push(path);
        }
    }
    found.sort();
    found
}

fn digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let mut out = String::from("sha256:");
    for byte in hasher.finalize() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The relative path a snapshot lives at, for messages.
pub fn display(name: &str) -> String {
    let path = path_for(name);
    let root = paths::workspace_root();
    path.strip_prefix(&root)
        .map_or_else(|_| path.to_string(), Utf8Path::to_string)
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;

    use super::{Snapshot, Verdict};

    fn snapshot(document: &[u8], lines: &[&str]) -> Snapshot {
        let owned: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
        Snapshot::take(document, &owned)
    }

    #[test_util::test]
    fn the_same_document_saying_the_same_thing_matches() {
        let taken = snapshot(b"doc", &["b", "a"]);
        assert_eq!(taken.compare(Some(&taken)), Verdict::Match);
        // Sorted on the way in, so the order diagnostics were produced in cannot cause a diff.
        assert_eq!(taken.lines, ["a", "b"]);
    }

    #[test_util::test]
    fn the_same_document_saying_something_new_is_a_regression() {
        let recorded = snapshot(b"doc", &["a"]);
        let taken = snapshot(b"doc", &["a", "b"]);
        let verdict = taken.compare(Some(&recorded));
        assert_eq!(
            verdict,
            Verdict::Regressed {
                added: vec!["b".to_owned()],
                removed: Vec::new(),
            }
        );
        assert!(verdict.is_failure());
    }

    #[test_util::test]
    fn a_different_document_is_a_republication_rather_than_a_break() {
        let recorded = snapshot(b"old", &["a"]);
        let taken = snapshot(b"new", &["a", "b"]);
        let verdict = taken.compare(Some(&recorded));
        assert_eq!(
            verdict,
            Verdict::Republished {
                diagnostics_changed: true
            }
        );
        // The whole reason snapshots are keyed by the document: a vendor editing their description
        // must not read as a progeny bug.
        assert!(!verdict.is_failure());
    }

    #[test_util::test]
    fn a_missing_snapshot_fails_rather_than_passing_quietly() {
        let taken = snapshot(b"doc", &[]);
        assert_eq!(taken.compare(None), Verdict::Missing);
        assert!(taken.compare(None).is_failure());
    }

    #[test_util::test]
    fn a_snapshot_round_trips_through_its_file_form() {
        let taken = snapshot(b"doc", &[r#"{"class":"wild-union"}"#]);
        let text = taken.render();
        assert_eq!(Snapshot::parse(&text), Some(taken));
        // Every line is JSON, header included.
        for line in text.lines() {
            serde_json::from_str::<serde_json::Value>(line)?;
        }
    }

    #[test_util::test]
    fn the_hash_is_of_the_document_not_of_the_diagnostics() {
        assert_eq!(
            snapshot(b"doc", &["a"]).document,
            snapshot(b"doc", &[]).document
        );
        assert_ne!(snapshot(b"a", &[]).document, snapshot(b"b", &[]).document);
    }
}
