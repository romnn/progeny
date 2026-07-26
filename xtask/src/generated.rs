//! Writing a generated crate out and compiling it.
//!
//! "The generated crate compiles" is the assertion that makes the type model's decisions real: a
//! `Box` in the wrong place, an identifier that is a keyword, a recursive type alias — none of them
//! are visible in a model comparison, and all of them are loud here. rustc is the reference
//! implementation of "is this Rust", and reimplementing its opinion would be the wrong kind of
//! thorough.
//!
//! The library performs no I/O, so this is where the filesystem gets involved: `progeny::generate`
//! hands back paths and strings, and xtask decides where they land.

use std::process::Command;

use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use progeny::Output;

/// Where generated crates are written for compiling.
///
/// Under `target/`, so it is already ignored, and outside the workspace's member globs so cargo does
/// not try to build them as part of it.
pub fn scratch_root() -> Utf8PathBuf {
    crate::paths::workspace_root().join("target/generated")
}

/// A shared target directory, so the dependencies are compiled once rather than once per document.
pub fn shared_target() -> Utf8PathBuf {
    scratch_root().join("target")
}

/// Write a generated crate into the scratch area and return its directory.
pub fn write(name: &str, output: &Output) -> Result<Utf8PathBuf> {
    let directory = scratch_root().join(name);
    // A stale file from a previous run with different contents would compile as part of this one.
    if directory.exists() {
        std::fs::remove_dir_all(&directory).with_context(|| format!("clearing {directory}"))?;
    }
    for (path, contents) in &output.files {
        let full = directory.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {parent}"))?;
        }
        let contents = if path == Utf8Path::new("Cargo.toml") {
            // The scratch crate sits inside this repository's directory tree, so cargo would try to
            // read it as a workspace member. An empty `[workspace]` table detaches it. Appended
            // here rather than emitted by the renderer: a real generated crate has no business
            // carrying a marker that only this harness needs.
            format!("{contents}\n[workspace]\n")
        } else {
            contents.clone()
        };
        std::fs::write(&full, contents).with_context(|| format!("writing {full}"))?;
    }
    Ok(directory)
}

/// What compiling a generated crate found.
pub struct Compiled {
    pub ok: bool,
    /// The first few lines of what rustc said, when it said anything.
    pub complaint: String,
}

/// `cargo check` a generated crate.
pub fn check(directory: &Utf8Path, clippy: bool) -> Result<Compiled> {
    let mut command = Command::new("cargo");
    command
        .current_dir(directory)
        .env("CARGO_TARGET_DIR", shared_target())
        // The generated crate is checked as itself, not through this workspace's lint policy.
        .env_remove("RUSTFLAGS")
        .arg(if clippy { "clippy" } else { "check" })
        .arg("--all-targets")
        .arg("--quiet");
    if clippy {
        // Generated warnings are product defects a user sees, so they fail the gate.
        command.args(["--", "-D", "warnings"]);
    }
    let output = command
        .output()
        .with_context(|| format!("running cargo in {directory}"))?;
    if output.status.success() {
        return Ok(Compiled {
            ok: true,
            complaint: String::new(),
        });
    }
    let text = String::from_utf8_lossy(&output.stderr);
    let complaint = text
        .lines()
        .filter(|line| line.starts_with("error") || line.starts_with("warning"))
        .take(4)
        .collect::<Vec<_>>()
        .join("; ");
    Ok(Compiled {
        ok: false,
        complaint: if complaint.is_empty() {
            text.lines().take(4).collect::<Vec<_>>().join("; ")
        } else {
            complaint
        },
    })
}

/// Check that the tooling a compile gate needs is there before promising to run one.
pub fn require_cargo() -> Result<()> {
    let found = Command::new("cargo")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !found {
        bail!("`cargo` is not on the path, so generated crates cannot be compiled");
    }
    Ok(())
}
