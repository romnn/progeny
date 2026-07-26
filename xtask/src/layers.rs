//! The two structural invariants of the generator crate, held mechanically.
//!
//! **Pipeline directionality.** The module graph *is* the pipeline —
//! `load → normalize → schema/doc → shape → contract → api → render` — with dependencies pointing
//! strictly leftward. Private fields already stop a later stage from constructing or mutating an
//! earlier stage's values, but Rust cannot make a leftward `use` a compile error inside one crate,
//! so this walks the crate's `use` graph instead. When workspace build parallelism ever justifies
//! splitting the crate, these module seams are already the crate seams and the rule becomes
//! compiler-enforced for free.
//!
//! **No I/O in the library.** `generate` takes bytes and returns strings. The check is here rather
//! than in a clippy `disallowed_methods` list because clippy's configuration is per-crate and this
//! crate also contains the `progeny` binary, whose entire job is I/O; a crate-wide fence cannot
//! tell them apart, and a second `clippy.toml` would fork the lint policy.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use clap::Args as ClapArgs;
use syn::spanned::Spanned;
use syn::visit::Visit;

use crate::paths;

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Print every import the walk resolved, not only the violations.
    #[arg(long)]
    verbose: bool,
}

/// The pipeline, as ranks. A module may only import from a rank at or below its own.
///
/// Diagnostics, the value reader and the configuration are cross-cutting: every stage reports, and
/// every stage may read what the caller asked for. Everything else is a stage, and the gaps in the
/// numbering are room for a stage that turns out to belong between two others.
const LAYERS: &[(&str, u32)] = &[
    ("diag", 0),
    ("value", 0),
    ("config", 0),
    ("load", 10),
    ("normalize", 20),
    ("schema", 30),
    ("doc", 40),
    ("shape", 50),
    ("contract", 60),
    ("api", 70),
    ("support", 75),
    ("render", 80),
    // The corpus harness and the crate root consume the whole pipeline by design.
    ("harness", 1000),
];

const ROOT_RANK: u32 = 1000;

/// Paths that mean the library reached for the filesystem or for standard I/O.
const IO_ROOTS: [&[&str]; 2] = [&["std", "fs"], &["std", "io"]];

struct Violation {
    file: Utf8PathBuf,
    line: usize,
    detail: String,
}

pub fn run(args: &Args) -> Result<()> {
    let source_root = paths::library_root().join("src");
    let files = rust_files(&source_root)?;
    if files.is_empty() {
        bail!("no Rust sources under {source_root}");
    }

    let ranks: BTreeMap<&str, u32> = LAYERS.iter().copied().collect();
    let mut violations = Vec::new();
    let mut imports = 0usize;

    for file in &files {
        let text = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
        let parsed = syn::parse_file(&text).with_context(|| format!("parsing {file}"))?;
        let module = module_path(&source_root, file);
        let is_binary = module.first().is_some_and(|segment| segment == "bin");
        let layer = module.first().map_or(ROOT_RANK, |segment| {
            ranks.get(segment.as_str()).copied().unwrap_or(ROOT_RANK)
        });

        let mut visitor = Walker { paths: Vec::new() };
        visitor.visit_file(&parsed);

        for (path, line) in &visitor.paths {
            let Some((target, target_rank)) = target_layer(path, &module, &ranks) else {
                continue;
            };
            imports += 1;
            if args.verbose {
                println!(
                    "  {}:{line} {} ({layer}) → {target} ({target_rank})",
                    file.file_name().unwrap_or_default(),
                    module.join("::"),
                );
            }
            if target_rank > layer {
                violations.push(Violation {
                    file: file.clone(),
                    line: *line,
                    detail: format!(
                        "`{}` (stage {layer}) imports `{target}` (stage {target_rank}); the \
                         pipeline only points leftward",
                        module.join("::"),
                    ),
                });
            }
        }

        if !is_binary {
            for (path, line) in &visitor.paths {
                if IO_ROOTS.iter().any(|root| starts_with(path, root)) {
                    violations.push(Violation {
                        file: file.clone(),
                        line: *line,
                        detail: format!(
                            "`{}` reaches for `{}`; the library performs no I/O — bytes in, \
                             strings out",
                            module.join("::"),
                            path.join("::"),
                        ),
                    });
                }
            }
        }
    }

    println!(
        "lint-layers: {} files, {imports} resolved imports, {} violations",
        files.len(),
        violations.len()
    );
    if violations.is_empty() {
        return Ok(());
    }
    for violation in &violations {
        println!(
            "  {}:{}: {}",
            violation.file, violation.line, violation.detail
        );
    }
    bail!("{} layering violations", violations.len())
}

/// Which stage a path reaches into, and that stage's rank.
///
/// `crate::x::…` names stage `x`. `super::…` is resolved against the module doing the importing, so
/// a submodule reaching sideways is still measured against its own stage. Anything that does not
/// resolve to a name in the stage table is not a stage: an item re-exported at the crate root is
/// the public API, and `std::…` is nobody's stage.
fn target_layer<'a>(
    path: &[String],
    module: &[String],
    ranks: &BTreeMap<&'a str, u32>,
) -> Option<(&'a str, u32)> {
    let named = match path.first()?.as_str() {
        "crate" => path.get(1)?,
        "super" => {
            // One `super` per leading segment; from there the next named segment is the target.
            let ups = path
                .iter()
                .take_while(|segment| *segment == "super")
                .count();
            let depth = module.len().checked_sub(ups)?;
            module.get(..depth)?.first().or_else(|| path.get(ups))?
        }
        _ => return None,
    };
    ranks
        .get_key_value(named.as_str())
        .map(|(name, rank)| (*name, *rank))
}

fn starts_with(path: &[String], prefix: &[&str]) -> bool {
    path.len() >= prefix.len()
        && path
            .iter()
            .zip(prefix)
            .all(|(segment, expected)| segment == expected)
}

/// The module path a source file provides, as segments below `src`.
///
/// `src/lib.rs` is the root, `src/schema/mod.rs` is `schema`, `src/schema/parse.rs` is
/// `schema::parse`.
fn module_path(root: &Utf8Path, file: &Utf8Path) -> Vec<String> {
    let Ok(relative) = file.strip_prefix(root) else {
        return Vec::new();
    };
    let mut segments: Vec<String> = relative
        .components()
        .map(|component| component.as_str().to_owned())
        .collect();
    if let Some(last) = segments.pop() {
        let stem = last.trim_end_matches(".rs");
        if stem != "mod" && stem != "lib" && stem != "main" {
            segments.push(stem.to_owned());
        }
    }
    segments
}

fn rust_files(root: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_owned()];
    while let Some(directory) = stack.pop() {
        let entries =
            std::fs::read_dir(&directory).with_context(|| format!("reading {directory}"))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("reading an entry of {directory}"))?;
            let path = Utf8PathBuf::try_from(entry.path())
                .with_context(|| "a source path is not valid UTF-8")?;
            if path.is_dir() {
                stack.push(path);
            } else if path.extension() == Some("rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Collects every path mentioned anywhere in a file, with its line.
///
/// Imports are the usual way a module reaches another, but not the only one: a fully qualified
/// call reaches just as far and would slip past a check that only read `use` items.
struct Walker {
    paths: Vec<(Vec<String>, usize)>,
}

impl Walker {
    fn record(&mut self, segments: Vec<String>, line: usize) {
        if !segments.is_empty() {
            self.paths.push((segments, line));
        }
    }
}

impl<'ast> Visit<'ast> for Walker {
    fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
        let line = line_of(node.span());
        let mut flattened = Vec::new();
        flatten_use_tree(&node.tree, &[], &mut flattened);
        for segments in flattened {
            self.record(segments, line);
        }
        syn::visit::visit_item_use(self, node);
    }

    fn visit_path(&mut self, node: &'ast syn::Path) {
        let segments: Vec<String> = node
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect();
        self.record(segments, line_of(node.span()));
        syn::visit::visit_path(self, node);
    }
}

/// The line a span starts on, one-indexed.
fn line_of(span: proc_macro2::Span) -> usize {
    span.start().line
}

fn flatten_use_tree(tree: &syn::UseTree, prefix: &[String], out: &mut Vec<Vec<String>>) {
    match tree {
        syn::UseTree::Path(path) => {
            let mut next = prefix.to_vec();
            next.push(path.ident.to_string());
            flatten_use_tree(&path.tree, &next, out);
        }
        syn::UseTree::Name(name) => {
            let mut next = prefix.to_vec();
            next.push(name.ident.to_string());
            out.push(next);
        }
        syn::UseTree::Rename(rename) => {
            let mut next = prefix.to_vec();
            next.push(rename.ident.to_string());
            out.push(next);
        }
        syn::UseTree::Glob(_) => out.push(prefix.to_vec()),
        syn::UseTree::Group(group) => {
            for item in &group.items {
                flatten_use_tree(item, prefix, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{module_path, starts_with, target_layer};
    use camino::Utf8Path;

    fn segments(path: &str) -> Vec<String> {
        path.split("::").map(ToOwned::to_owned).collect()
    }

    fn ranks() -> std::collections::BTreeMap<&'static str, u32> {
        super::LAYERS.iter().copied().collect()
    }

    #[test]
    fn module_paths_come_from_file_paths() {
        let root = Utf8Path::new("/w/crates/progeny/src");
        assert!(module_path(root, Utf8Path::new("/w/crates/progeny/src/lib.rs")).is_empty());
        assert_eq!(
            module_path(root, Utf8Path::new("/w/crates/progeny/src/load.rs")),
            ["load"]
        );
        assert_eq!(
            module_path(root, Utf8Path::new("/w/crates/progeny/src/schema/mod.rs")),
            ["schema"]
        );
        assert_eq!(
            module_path(root, Utf8Path::new("/w/crates/progeny/src/schema/parse.rs")),
            ["schema", "parse"]
        );
    }

    #[test]
    fn crate_paths_name_their_layer() {
        let ranks = ranks();
        assert_eq!(
            target_layer(
                &segments("crate::schema::SchemaId"),
                &segments("doc"),
                &ranks
            ),
            Some(("schema", 30))
        );
        // Root re-exports and foreign crates belong to no stage.
        assert_eq!(
            target_layer(&segments("crate::Diagnostic"), &[], &ranks),
            None
        );
        assert_eq!(
            target_layer(&segments("std::fmt::Display"), &[], &ranks),
            None
        );
    }

    #[test]
    fn super_paths_resolve_against_the_importing_module() {
        let ranks = ranks();
        assert_eq!(
            target_layer(
                &segments("super::SchemaObject"),
                &segments("schema::parse"),
                &ranks
            ),
            Some(("schema", 30))
        );
        // Two `super`s from `schema::parse` land at the crate root, so the next segment names
        // the stage.
        assert_eq!(
            target_layer(
                &segments("super::super::load::Thing"),
                &segments("schema::parse"),
                &ranks
            ),
            Some(("load", 10))
        );
    }

    #[test]
    fn io_prefixes_are_matched_exactly() {
        assert!(starts_with(&segments("std::fs::read"), &["std", "fs"]));
        assert!(starts_with(&segments("std::io::Read"), &["std", "io"]));
        assert!(!starts_with(&segments("std::fmt::Write"), &["std", "io"]));
        assert!(!starts_with(&segments("std"), &["std", "fs"]));
    }
}
