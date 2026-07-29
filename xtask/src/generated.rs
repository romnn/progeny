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

use camino::{Utf8Path, Utf8PathBuf};
use color_eyre::eyre::{self, WrapErr, bail};
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

/// A cargo invocation over a generated crate, set up the way every gate here needs it.
///
/// One build job, and that is the load-bearing part. `cloudflare`'s library alone needs about
/// 9 GiB of rustc through the hand-written path and 14.7 GiB through the derive
/// (`corpus/baseline.toml`), and `--all-targets` compiles that same source a second time as a test
/// binary — two units cargo is free to run at once, which doubles the peak. A 16 GiB CI runner does
/// not survive that: it dies mid-compile with no diagnostic, because the process that would have
/// printed one is the one that was killed. Serialised, the peak is one unit's.
///
/// Set here rather than at each call site because the rule is about what the corpus holds, not
/// about which gate is asking, and a gate added later would otherwise reintroduce the failure by
/// omission.
pub fn cargo(directory: &Utf8Path) -> Command {
    let mut command = Command::new("cargo");
    command
        .current_dir(directory)
        // The shared dependency cache: `serde`, `reqwest` and `axum` are compiled once for the
        // whole corpus rather than once per document.
        .env("CARGO_TARGET_DIR", shared_target())
        .env("CARGO_BUILD_JOBS", "1")
        // The generated crate is checked as itself, not through this workspace's lint policy.
        .env_remove("RUSTFLAGS");
    command
}

/// Write a generated crate into the scratch area and return its directory.
pub fn write(name: &str, output: &Output) -> eyre::Result<Utf8PathBuf> {
    let directory = scratch_root().join(name);
    // A stale file from a previous run with different contents would compile as part of this one.
    if directory.exists() {
        std::fs::remove_dir_all(&directory).wrap_err_with(|| format!("clearing {directory}"))?;
    }
    for (path, contents) in &output.files {
        let full = directory.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).wrap_err_with(|| format!("creating {parent}"))?;
        }
        let contents = if path == Utf8Path::new("Cargo.toml") {
            detached_manifest(contents)
        } else {
            contents.clone()
        };
        std::fs::write(&full, contents).wrap_err_with(|| format!("writing {full}"))?;
    }
    Ok(directory)
}

fn detached_manifest(contents: &str) -> String {
    if contents.lines().any(|line| line.trim() == "[workspace]") {
        return contents.to_owned();
    }
    // The scratch crate sits inside this repository's directory tree, so cargo would try to read it
    // as a workspace member. An empty table detaches it. A generated workspace already carries its
    // own table, and a duplicate would make that manifest invalid.
    indoc::formatdoc! {"
        {contents}
        [workspace]
    "}
}

/// Append the wire harness's test dependencies and write its integration test.
///
/// A generated crate has no business declaring a runtime or a socket — those are the consumer's
/// choices — so the gates that stand a server up (`example`, `probe`) append their shared harness
/// and runtime here. One copy on purpose: when the generated servers start needing another Tokio
/// feature, both gates move together, where a copy updated in one once meant the other failing to
/// compile and reporting it as a *product* verdict — "the two halves disagree on the wire" — from
/// a harness defect.
pub fn write_wire_test(directory: &Utf8Path, file_name: &str, source: &str) -> eyre::Result<()> {
    let manifest = directory.join("Cargo.toml");
    let existing =
        std::fs::read_to_string(&manifest).wrap_err_with(|| format!("reading {manifest}"))?;
    std::fs::write(
        &manifest,
        indoc::formatdoc! {r#"
            {existing}
            [dev-dependencies]
            color-eyre = "0.6"
            test-util = {{ path = "../../../crates/test-util" }}
            tokio = {{ version = "1", features = ["rt-multi-thread", "macros", "net"] }}
        "#},
    )
    .wrap_err_with(|| format!("writing {manifest}"))?;

    let tests = directory.join("tests");
    std::fs::create_dir_all(&tests).wrap_err_with(|| format!("creating {tests}"))?;
    let file = tests.join(file_name);
    std::fs::write(&file, source).wrap_err_with(|| format!("writing {file}"))?;
    Ok(())
}

/// Append the shared synchronous test harness and write an integration test.
pub fn write_test(directory: &Utf8Path, file_name: &str, source: &str) -> eyre::Result<()> {
    let manifest = directory.join("Cargo.toml");
    let existing =
        std::fs::read_to_string(&manifest).wrap_err_with(|| format!("reading {manifest}"))?;
    std::fs::write(
        &manifest,
        indoc::formatdoc! {r#"
            {existing}
            [dev-dependencies]
            color-eyre = "0.6"
            test-util = {{ path = "../../../crates/test-util" }}
        "#},
    )
    .wrap_err_with(|| format!("writing {manifest}"))?;

    let tests = directory.join("tests");
    std::fs::create_dir_all(&tests).wrap_err_with(|| format!("creating {tests}"))?;
    let file = tests.join(file_name);
    std::fs::write(&file, source).wrap_err_with(|| format!("writing {file}"))?;
    Ok(())
}

/// What compiling a generated crate found.
pub struct Compiled {
    pub ok: bool,
    /// The first few lines of what rustc said, when it said anything.
    pub complaint: String,
}

/// `cargo check` a generated crate.
pub fn check(directory: &Utf8Path, clippy: bool) -> eyre::Result<Compiled> {
    let mut command = cargo(directory);
    command
        .arg(if clippy { "clippy" } else { "check" })
        .arg("--all-targets")
        // The client module sits behind a cargo feature so a consumer of the shared types alone
        // does not compile an HTTP stack. Without this the gate would check everything *except*
        // the half of the output that has a network protocol in it.
        .arg("--all-features")
        .arg("--quiet");
    if clippy {
        // Generated warnings are product defects a user sees, so they fail the gate.
        command.args(["--", "-D", "warnings"]);
    }
    let output = command
        .output()
        .wrap_err_with(|| format!("running cargo in {directory}"))?;
    if output.status.success() {
        return Ok(Compiled {
            ok: true,
            complaint: String::new(),
        });
    }
    let text = String::from_utf8_lossy(&output.stderr);
    // Errors before warnings, always. rustc prints them interleaved in source order, so a crate
    // with a warning near the top and an error near the bottom reports as four warnings — which
    // reads as "this compiles, with grumbling" and is the opposite of what happened.
    let lines = |prefix: &str| -> Vec<&str> {
        text.lines()
            .filter(|line| line.starts_with(prefix))
            .collect()
    };
    let complaint = lines("error")
        .into_iter()
        .chain(lines("warning"))
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
pub fn require_cargo() -> eyre::Result<()> {
    let found = Command::new("cargo")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !found {
        bail!("`cargo` is not on the path, so generated crates cannot be compiled");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use camino::Utf8Path;

    /// The job count is what stands between the compile gates and a runner that dies without
    /// saying why, so it is asserted rather than trusted to survive the next edit here.
    #[test]
    fn a_generated_crate_is_compiled_one_job_at_a_time() {
        let command = super::cargo(Utf8Path::new("."));
        let jobs = command
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("CARGO_BUILD_JOBS"))
            .and_then(|(_, value)| value);
        assert_eq!(jobs, Some(std::ffi::OsStr::new("1")));
    }

    #[test]
    fn an_emitted_workspace_is_not_given_a_second_workspace_table() {
        let manifest = indoc::indoc! {"
            [workspace]
            members = [\"types\"]
        "};
        assert_eq!(super::detached_manifest(manifest), manifest);
        assert_eq!(
            super::detached_manifest("[package]\nname = \"api\"\n")
                .matches("[workspace]")
                .count(),
            1
        );
    }
}
