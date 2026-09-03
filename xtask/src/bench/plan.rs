//! What a run measures: rendering the subjects, recording them, and reusing them later.
//!
//! The plan half exists apart from the measurement half because they want different machines: the
//! measurement needs a quiet one, rendering does not. `--generate-only` writes the plan at the
//! moment the tree is the one under discussion, and `--reuse` measures exactly those crates
//! however far the generator moves in the meantime.

use std::process::Command;

use camino::{Utf8Path, Utf8PathBuf};
use color_eyre::eyre::{self, WrapErr, bail};

use super::{Args, HAND_WRITTEN};

/// One crate to measure, and which rendering of its document it is.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub(super) struct Target {
    /// The document this was generated from, or the directory it was found in.
    pub(super) subject: String,
    pub(super) variant: String,
    /// The serde strategy, separate from `variant` when each workspace member is its own target.
    #[serde(default)]
    pub(super) strategy: String,
    /// Empty for a single crate; otherwise `types`, `client`, or `server`.
    #[serde(default)]
    pub(super) member: String,
    pub(super) package: String,
    pub(super) directory: Utf8PathBuf,
    /// What this rendering held, from [`scope_of`].
    ///
    /// Recorded here rather than recomputed at measurement time because `--reuse` measures crates a
    /// different tree rendered: asking today's generator what it emits would answer about the wrong
    /// crate, which is the exact confusion the field exists to prevent.
    #[serde(default)]
    pub(super) scope: String,
}

/// What one run rendered, and which tree it rendered from.
///
/// Written beside the crates so that a measurement taken days later still says what it measured.
/// A benchmark whose subject cannot be identified afterwards is a number, not evidence.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct Rendering {
    /// The commit the generator was at, and whether the worktree had uncommitted changes.
    revision: String,
    dirty: bool,
    targets: Vec<Target>,
}

/// Where the rendering plan lives, so `--reuse` needs no guesses about directory names.
fn rendering_path() -> Utf8PathBuf {
    crate::generated::scratch_root().join("bench-rendering.toml")
}

/// The baseline key: the document, then the rendering. Stable across runs by construction.
pub(super) fn key_of(target: &Target) -> String {
    format!("{}.{}", target.subject, target.variant)
}

/// The layers a generated crate can hold, outermost question first.
///
/// `support` is deliberately absent: it is plumbing the other modules call into, not surface a
/// reader is weighing. `operations` is the reflection beside the types — dependency-free, but a
/// table row per operation is surface a types-only figure has to say it includes.
const LAYERS: [&str; 4] = ["types", "operations", "client", "server"];

/// What a crate's emitted modules say it holds.
///
/// Derived from what was actually emitted rather than written down beside the numbers, because the
/// hazard is precisely a scope note that stays true only until the next renderer lands. serde is a
/// *shrinking* share of a generated crate as stages add surface the serde change never touches, so
/// the same A/B measured against types alone and against a full client answers two different
/// questions — and the larger number is the one that gets quoted.
fn scope_of<'a>(modules: impl IntoIterator<Item = &'a str>) -> String {
    let modules: Vec<&str> = modules.into_iter().collect();
    let present: Vec<&str> = LAYERS
        .into_iter()
        .filter(|layer| modules.contains(layer))
        .collect();
    match present.as_slice() {
        [] => UNRECORDED.to_owned(),
        ["types"] => "types-only".to_owned(),
        _ => present.join("+"),
    }
}

/// The scope of a rendering taken before the harness recorded one.
pub(super) const UNRECORDED: &str = "unrecorded";

/// The module stems of a crate on disk: the `--crate-dir` path where nothing was rendered, and
/// each Workspace member, whose name says which edge it is but not what it holds.
fn modules_on_disk(crate_dir: &Utf8Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(crate_dir.join("src")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = Utf8PathBuf::from_path_buf(entry.path()).ok()?;
            if path.extension() != Some("rs") {
                return None;
            }
            Some(path.file_stem()?.to_owned())
        })
        .collect()
}

/// What this run will measure, generated and written out, grouped by document.
///
/// Generation happens up front rather than between repetitions: A-B-B-A ordering only means
/// anything if both variants are sitting on disk before the first measurement starts.
pub(super) fn plan(args: &Args) -> eyre::Result<Vec<(String, Vec<Target>)>> {
    if args.reuse {
        return reuse(args);
    }
    if let Some(crate_dir) = &args.crate_dir {
        let package = package_name(crate_dir)?;
        println!(
            "bench-compile: {package} at {crate_dir}, {} reps",
            args.reps
        );
        let modules = modules_on_disk(crate_dir);
        return Ok(vec![(
            package.clone(),
            vec![Target {
                subject: package.clone(),
                variant: "as-is".to_owned(),
                strategy: "as-is".to_owned(),
                member: String::new(),
                package,
                directory: crate_dir.clone(),
                scope: scope_of(modules.iter().map(String::as_str)),
            }],
        )]);
    }

    let wanted = if args.specs.is_empty() {
        crate::corpus::quick_tier()?
    } else {
        args.specs.clone()
    };
    let variants = super::selected_strategies(args.ab, args.hand_written);
    println!(
        "bench-compile: {} documents × {} × {} reps, {} packaging, generated into {}",
        wanted.len(),
        variants.join(" and "),
        args.reps,
        match (args.workspace, args.crate_control) {
            (true, true) => "workspace + crate control",
            (true, false) => "workspace",
            (false, _) => "crate",
        },
        crate::generated::scratch_root()
    );

    let mut planned = Vec::new();
    for (spec, bytes) in &crate::corpus::selected(&wanted)? {
        planned.push((
            spec.name.clone(),
            generated_targets(args, spec, bytes, variants)?,
        ));
    }
    record(&planned)?;
    Ok(planned)
}

fn generated_targets(
    args: &Args,
    spec: &crate::corpus::Spec,
    bytes: &[u8],
    variants: &[&str],
) -> eyre::Result<Vec<Target>> {
    let name = &spec.name;
    let mut targets = Vec::new();
    for &variant in variants {
        let mut config = crate::corpus::config_for(spec);
        config.serde_impl = if variant == HAND_WRITTEN {
            progeny::SerdeImpl::HandWrittenWhereEligible
        } else {
            progeny::SerdeImpl::DeriveAlways
        };
        if args.workspace && args.crate_control {
            let output = progeny::generate(bytes, &config)
                .wrap_err_with(|| format!("generating {name} ({variant}, crate control)"))?;
            let scope = scope_of(
                output
                    .files
                    .keys()
                    .filter(|path| path.parent() == Some(Utf8Path::new("src")))
                    .filter_map(|path| path.file_stem()),
            );
            let directory =
                crate::generated::write(&format!("bench-{variant}-crate-{name}"), &output)?;
            targets.push(Target {
                subject: name.clone(),
                variant: format!("{variant}.crate"),
                strategy: variant.to_owned(),
                member: String::new(),
                package: config.package.name.clone(),
                directory,
                scope,
            });
        }
        if args.workspace {
            config.packaging = progeny::Packaging::Workspace;
        }
        let output = progeny::generate(bytes, &config)
            .wrap_err_with(|| format!("generating {name} ({variant})"))?;
        let directory = crate::generated::write(&format!("bench-{variant}-{name}"), &output)?;
        if args.workspace {
            for member in ["types", "client", "server"] {
                let package = format!("{}-{member}", config.package.name);
                let member_directory = directory.join(&package);
                if !member_directory.join("Cargo.toml").exists() {
                    continue;
                }
                // From the modules the member actually holds rather than from its name: the
                // types member carries the reflection beside the types, and a scope that said
                // `types-only` about it would let a before/after A/B subtract across shapes.
                let modules = modules_on_disk(&member_directory);
                targets.push(Target {
                    subject: name.clone(),
                    variant: format!("{variant}.{member}"),
                    strategy: variant.to_owned(),
                    member: member.to_owned(),
                    package,
                    directory: member_directory,
                    scope: scope_of(modules.iter().map(String::as_str)),
                });
            }
        } else {
            let scope = scope_of(
                output
                    .files
                    .keys()
                    .filter(|path| path.parent() == Some(Utf8Path::new("src")))
                    .filter_map(|path| path.file_stem()),
            );
            targets.push(Target {
                subject: name.clone(),
                variant: variant.to_owned(),
                strategy: variant.to_owned(),
                member: String::new(),
                package: config.package.name.clone(),
                directory,
                scope,
            });
        }
    }
    Ok(targets)
}

/// Write down what was rendered and from which tree.
fn record(planned: &[(String, Vec<Target>)]) -> eyre::Result<()> {
    let (revision, dirty) = revision();
    let rendering = Rendering {
        revision,
        dirty,
        targets: planned
            .iter()
            .flat_map(|(_, targets)| targets.iter().cloned())
            .collect(),
    };
    let path = rendering_path();
    let text = toml::to_string_pretty(&rendering).wrap_err("rendering the bench plan")?;
    std::fs::write(&path, text).wrap_err_with(|| format!("writing {path}"))
}

/// Measure the crates an earlier run rendered, without rendering anything.
fn reuse(args: &Args) -> eyre::Result<Vec<(String, Vec<Target>)>> {
    let path = rendering_path();
    let text = std::fs::read_to_string(&path).wrap_err_with(|| {
        format!("reading {path}; --reuse measures what --generate-only rendered, and nothing has")
    })?;
    let rendering: Rendering = toml::from_str(&text).wrap_err_with(|| format!("parsing {path}"))?;

    let (here, _) = revision();
    println!(
        "bench-compile: reusing {} crates rendered from {}{}, {} reps",
        rendering.targets.len(),
        rendering.revision,
        if rendering.dirty {
            " with uncommitted changes"
        } else {
            ""
        },
        args.reps
    );
    // Said rather than refused: measuring an older rendering is the *point* of `--reuse`, and the
    // only thing that would make it a mistake is not knowing.
    if here != rendering.revision {
        println!(
            "  note: the generator is at {here} now, so this measures the earlier tree and not \
             this one"
        );
    }

    let mut planned: Vec<(String, Vec<Target>)> = Vec::new();
    for mut target in rendering.targets {
        // A rendering from before the harness recorded a scope says so, rather than inheriting
        // whatever today's generator would emit — a wrong scope is worse than an absent one,
        // because the point of the field is to refuse a comparison across scopes.
        if target.scope.is_empty() {
            println!(
                "  note: this rendering predates scope recording, so {} is measured as \
                 `{UNRECORDED}`",
                target.subject
            );
            UNRECORDED.clone_into(&mut target.scope);
        }
        let manifest = target.directory.join("Cargo.toml");
        if !manifest.exists() {
            bail!(
                "{} is gone, so there is nothing to reuse; re-run with --generate-only",
                target.directory
            );
        }
        if !args.specs.is_empty() && !args.specs.contains(&target.subject) {
            continue;
        }
        match planned.iter_mut().find(|(name, _)| name == &target.subject) {
            Some((_, targets)) => targets.push(target),
            None => planned.push((target.subject.clone(), vec![target])),
        }
    }
    if planned.is_empty() {
        bail!("the recorded rendering has nothing matching the documents asked for");
    }
    Ok(planned)
}

/// The commit the generator is at, and whether the worktree has uncommitted changes.
fn revision() -> (String, bool) {
    let git = |arguments: &[&str]| -> Option<String> {
        let output = Command::new("git")
            .current_dir(crate::paths::workspace_root())
            .args(arguments)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let head =
        git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "an unknown tree".to_owned());
    let dirty = git(&["status", "--porcelain"]).is_some_and(|status| !status.is_empty());
    (head, dirty)
}

fn package_name(crate_dir: &Utf8PathBuf) -> eyre::Result<String> {
    #[derive(serde::Deserialize)]
    struct Manifest {
        package: Package,
    }
    #[derive(serde::Deserialize)]
    struct Package {
        name: String,
    }

    let path = crate_dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&path).wrap_err_with(|| format!("reading {path}"))?;
    let manifest: Manifest = toml::from_str(&text).wrap_err_with(|| format!("parsing {path}"))?;
    Ok(manifest.package.name)
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;

    use super::scope_of;

    #[test_util::test]
    fn a_rendering_survives_the_file_it_is_written_to() {
        // `--generate-only` writes this and `--reuse` reads it, possibly days apart and across a
        // rebuild of the tool. A shape that serializes and does not deserialize would strand the
        // one artifact the separation exists to preserve.
        let rendering = super::Rendering {
            revision: "013655a".to_owned(),
            dirty: true,
            targets: vec![super::Target {
                subject: "okta".to_owned(),
                variant: crate::bench::DERIVE.to_owned(),
                strategy: crate::bench::DERIVE.to_owned(),
                member: String::new(),
                package: "corpus-okta".to_owned(),
                directory: camino::Utf8PathBuf::from("/tmp/bench-derive-okta"),
                scope: "types-only".to_owned(),
            }],
        };
        let text = toml::to_string_pretty(&rendering)?;
        let read: super::Rendering = toml::from_str(&text)?;
        assert_eq!(read.revision, "013655a");
        assert!(read.dirty);
        assert_eq!(read.targets.len(), 1);
        assert_eq!(super::key_of(&read.targets[0]), "okta.derive");
        assert_eq!(read.targets[0].directory, "/tmp/bench-derive-okta");
        assert_eq!(read.targets[0].scope, "types-only");
    }

    #[test_util::test]
    fn a_rendering_from_before_the_scope_was_recorded_still_reads() {
        // The archived stage-4 subject was written without the field, and it is the *one* rendering
        // the corrected baseline has to be taken from. A hard parse error here would strand it.
        let text = indoc::indoc! {r#"
            revision = "013655a"
            dirty = true

            [[targets]]
            subject = "okta"
            variant = "derive"
            package = "corpus-okta"
            directory = "/tmp/x"
        "#};
        let read: super::Rendering = toml::from_str(text)?;
        assert_eq!(read.targets[0].scope, "");
    }

    /// The layer list is the renderer's module list, checked against a real rendering.
    ///
    /// `scope_of` is what guards baseline comparability — `unusable()` refuses a cross-scope
    /// comparison by this string — so a renderer that gains a fourth surface module has to
    /// extend `LAYERS`, or two structurally different crates stringify to the same scope and
    /// the refusal stops refusing. The subject is the committed petstore, which renders every
    /// surface module a default configuration can produce.
    #[test_util::test]
    fn the_layer_list_is_the_renderers_module_list() {
        let path = crate::paths::corpus_root().join("specs/petstore-31.yaml");
        let bytes = std::fs::read(&path)?;
        let output = progeny::generate(&bytes, &progeny::Config::default())?;
        for file in output.files.keys() {
            let Some(stem) = file
                .strip_prefix("src")
                .ok()
                .and_then(camino::Utf8Path::file_stem)
            else {
                continue;
            };
            assert!(
                stem == "lib" || stem == "support" || super::LAYERS.contains(&stem),
                "the renderer emits `src/{stem}.rs`, which the bench layer list does not know; \
                 two different crates could now stringify to one scope"
            );
        }
    }

    #[test_util::test]
    fn a_crate_says_what_it_holds() {
        assert_eq!(scope_of(["lib", "types"]), "types-only");
        // `support` is plumbing the other modules call into, not surface being weighed.
        assert_eq!(scope_of(["lib", "types", "support"]), "types-only");
        assert_eq!(
            scope_of(["lib", "types", "client", "support"]),
            "types+client"
        );
        assert_eq!(
            scope_of(["lib", "types", "client", "server", "support"]),
            "types+client+server"
        );
        // The reflection is surface: a types crate carrying it is not `types-only`, or the
        // A/B that measures what it costs could not be refused across the two shapes.
        assert_eq!(
            scope_of(["lib", "types", "operations", "support"]),
            "types+operations"
        );
        assert_eq!(
            scope_of(["lib", "types", "operations", "client", "server", "support"]),
            "types+operations+client+server"
        );
        // Layer order, not the order the files happened to arrive in.
        assert_eq!(scope_of(["client", "types"]), "types+client");
        assert_eq!(scope_of(["lib"]), super::UNRECORDED);
    }

    /// A Workspace member's scope comes from what its `src` holds, not from its name.
    #[test_util::test]
    fn a_workspace_member_is_scoped_by_the_modules_it_holds() {
        let path = crate::paths::corpus_root().join("specs/petstore-31.yaml");
        let bytes = std::fs::read(&path)?;
        let config = progeny::Config {
            packaging: progeny::Packaging::Workspace,
            package: progeny::Package {
                name: "scoped".to_owned(),
                version: "0.0.0".to_owned(),
            },
            ..progeny::Config::default()
        };
        let output = progeny::generate(&bytes, &config)?;
        let directory = crate::generated::write("bench-scope-fixture", &output)?;
        let scope = |member: &str| {
            let modules = super::modules_on_disk(&directory.join(format!("scoped-{member}")));
            scope_of(modules.iter().map(String::as_str))
        };
        assert_eq!(scope("types"), "types+operations");
        assert_eq!(scope("client"), "client");
        assert_eq!(scope("server"), "server");
    }
}
