//! Which of the baseline crates a generated crate's rendered source names, and behind which
//! feature gates.
//!
//! `serde`, `serde_json`, `indoc` and `thiserror` are named by most renderers and most shipped
//! support files, each under its own conditions.
//! Deciding them from the model would be a second copy of every one of those choices, so the
//! manifest asks the rendered source instead, one crate at a time: a dependency nothing names is
//! one Cargo's `unused_dependencies` lint reports in the consumer's build.
//!
//! The scan reads tokens, never text.
//! A string literal, a doc comment included, is one token whose contents are never looked at, so
//! prose in a description cannot add a dependency.
//! It errs toward declaring: a gate it cannot read counts as no gate, so a misjudged crate is
//! declared where it is not needed rather than missing where it is.

use std::collections::{BTreeMap, BTreeSet};

use camino::{Utf8Path, Utf8PathBuf};
use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};
use quote::ToTokens as _;

/// A crate generated code may name, which the manifest declares only where it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Baseline {
    Serde,
    SerdeJson,
    Indoc,
    Thiserror,
}

impl Baseline {
    const ALL: [Self; 4] = [Self::Serde, Self::SerdeJson, Self::Indoc, Self::Thiserror];

    /// The name the crate is declared and spelled under.
    fn name(self) -> &'static str {
        match self {
            Self::Serde => "serde",
            Self::SerdeJson => "serde_json",
            Self::Indoc => "indoc",
            Self::Thiserror => "thiserror",
        }
    }

    /// The crate's `[dependencies]` line.
    fn dependency(self, optional: bool) -> &'static str {
        match (self, optional) {
            (Self::Serde, false) => r#"serde = { version = "1.0.164", features = ["derive"] }"#,
            (Self::Serde, true) => {
                r#"serde = { version = "1.0.164", optional = true, features = ["derive"] }"#
            }
            (Self::SerdeJson, false) => r#"serde_json = "1""#,
            (Self::SerdeJson, true) => r#"serde_json = { version = "1", optional = true }"#,
            (Self::Indoc, false) => r#"indoc = "2""#,
            (Self::Indoc, true) => r#"indoc = { version = "2", optional = true }"#,
            (Self::Thiserror, false) => r#"thiserror = "2""#,
            (Self::Thiserror, true) => r#"thiserror = { version = "2", optional = true }"#,
        }
    }

    fn named(ident: &proc_macro2::Ident) -> Option<Self> {
        Self::ALL.into_iter().find(|krate| ident == krate.name())
    }
}

/// Where one crate is named.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Usage {
    /// Named somewhere no feature gate covers.
    ungated: bool,
    /// The features that gate its gated uses.
    features: BTreeSet<String>,
}

/// The baseline crates one generated crate names, each with the gates it is named under.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct BaselineUse {
    crates: BTreeMap<Baseline, Usage>,
}

impl BaselineUse {
    /// Scans every `.rs` file under `src` in `files`, the rendered sources of one crate.
    ///
    /// An out-of-line module the crate root declares behind `#[cfg(feature = "…")]` is gated by
    /// that feature throughout its file.
    pub(super) fn scan(files: &BTreeMap<Utf8PathBuf, String>, src: &Utf8Path) -> Self {
        let module_gates = files
            .get(&src.join("lib.rs"))
            .map(|root| module_gates(root))
            .unwrap_or_default();
        let mut usage = Self::default();
        let sources = files
            .iter()
            .filter(|(path, _)| path.starts_with(src) && path.extension() == Some("rs"));
        for (path, text) in sources {
            let module = path
                .strip_prefix(src)
                .ok()
                .and_then(Utf8Path::file_stem)
                .unwrap_or_default();
            let gate = module_gates.get(module).map(String::as_str);
            // A file that does not parse does not compile either, so it needs no dependency.
            if let Ok(file) = syn::parse_file(text) {
                usage.file(&file, gate);
            }
        }
        usage
    }

    /// The `[dependencies]` lines of the named crates: plain where any use is ungated, optional
    /// where every use is behind a feature.
    pub(super) fn dependencies(&self) -> Vec<&'static str> {
        self.crates
            .iter()
            .map(|(krate, usage)| krate.dependency(!usage.ungated))
            .collect()
    }

    /// Each feature that gates an optional crate's uses, with the `dep:` entries it turns on.
    pub(super) fn features(&self) -> BTreeMap<String, Vec<String>> {
        let mut features: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (krate, usage) in &self.crates {
            if usage.ungated {
                continue;
            }
            for feature in &usage.features {
                features
                    .entry(feature.clone())
                    .or_default()
                    .push(format!("dep:{}", krate.name()));
            }
        }
        features
    }
}

/// The feature each out-of-line module declared in a crate root is gated by.
fn module_gates(root: &str) -> BTreeMap<String, String> {
    let Ok(file) = syn::parse_file(root) else {
        return BTreeMap::new();
    };
    file.items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Mod(module) if module.content.is_none() => {
                let feature = module
                    .attrs
                    .iter()
                    .find_map(|attribute| cfg_feature(&attribute.meta))?;
                Some((module.ident.to_string(), feature))
            }
            _ => None,
        })
        .collect()
}

/// The feature a `cfg(feature = "…")` names.
///
/// Any other predicate, `any`, `all` and `not` included, is no gate the scan relies on.
fn cfg_feature(meta: &syn::Meta) -> Option<String> {
    let syn::Meta::List(list) = meta else {
        return None;
    };
    if !list.path.is_ident("cfg") {
        return None;
    }
    simple_feature(list.tokens.clone())
}

/// The feature a bare `feature = "…"` predicate names.
fn simple_feature(predicate: TokenStream) -> Option<String> {
    let predicate = syn::parse2::<syn::MetaNameValue>(predicate).ok()?;
    if !predicate.path.is_ident("feature") {
        return None;
    }
    match predicate.value {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(feature),
            ..
        }) => Some(feature.value()),
        _ => None,
    }
}

impl BaselineUse {
    fn file(&mut self, file: &syn::File, gate: Option<&str>) {
        for attribute in &file.attrs {
            self.tokens(attribute.to_token_stream(), gate);
        }
        for item in &file.items {
            let tokens = item.to_token_stream();
            let own = item_gate(&tokens);
            self.tokens(tokens, own.as_deref().or(gate));
        }
    }

    /// Records every crate followed by `::`, at any depth. String literals and doc comments are
    /// single literal tokens, so prose names nothing.
    fn tokens(&mut self, stream: TokenStream, gate: Option<&str>) {
        let tokens: Vec<TokenTree> = stream.into_iter().collect();
        for (index, token) in tokens.iter().enumerate() {
            match token {
                TokenTree::Group(group) => self.tokens(group.stream(), gate),
                TokenTree::Ident(ident) => {
                    let followed = matches!(
                        tokens.get(index + 1..index + 3),
                        Some([first, second]) if is_path_separator(first, second)
                    );
                    if let Some(krate) = Baseline::named(ident)
                        && followed
                    {
                        self.record(krate, gate);
                    }
                }
                TokenTree::Punct(_) | TokenTree::Literal(_) => {}
            }
        }
    }

    fn record(&mut self, krate: Baseline, gate: Option<&str>) {
        let usage = self.crates.entry(krate).or_default();
        match gate {
            None => usage.ungated = true,
            Some(feature) => {
                usage.features.insert(feature.to_owned());
            }
        }
    }
}

/// The feature an item's own outer `#[cfg(feature = "…")]` names.
fn item_gate(tokens: &TokenStream) -> Option<String> {
    let tokens: Vec<TokenTree> = tokens.clone().into_iter().collect();
    tokens.windows(2).find_map(|pair| match pair {
        [TokenTree::Punct(pound), TokenTree::Group(group)]
            if pound.as_char() == '#' && group.delimiter() == Delimiter::Bracket =>
        {
            syn::parse2::<syn::Meta>(group.stream())
                .ok()
                .and_then(|meta| cfg_feature(&meta))
        }
        _ => None,
    })
}

fn is_path_separator(first: &TokenTree, second: &TokenTree) -> bool {
    matches!(
        (first, second),
        (TokenTree::Punct(first), TokenTree::Punct(second))
            if first.as_char() == ':'
                && first.spacing() == Spacing::Joint
                && second.as_char() == ':'
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use camino::{Utf8Path, Utf8PathBuf};
    use color_eyre::eyre;

    use super::{Baseline, BaselineUse, Usage};

    /// Scans one crate whose `src` holds the given files.
    fn scan(files: &[(&str, &str)]) -> BaselineUse {
        let files: BTreeMap<Utf8PathBuf, String> = files
            .iter()
            .map(|&(path, text)| (Utf8Path::new("src").join(path), text.to_owned()))
            .collect();
        BaselineUse::scan(&files, Utf8Path::new("src"))
    }

    fn ungated() -> Usage {
        Usage {
            ungated: true,
            ..Usage::default()
        }
    }

    fn gated(features: &[&str]) -> Usage {
        Usage {
            ungated: false,
            features: features.iter().map(|&it| it.to_owned()).collect(),
        }
    }

    /// A crate counts wherever its name is followed by `::`, and not where it is a plain
    /// identifier.
    #[test_util::test]
    fn a_crate_is_named_where_a_path_continues_after_it() {
        let usage = scan(&[(
            "lib.rs",
            indoc::indoc! {"
                use serde::Serialize;
                pub fn value() -> ::serde_json::Value {
                    let thiserror = 1;
                    ::serde_json::Value::Null
                }
            "},
        )]);
        assert_eq!(
            usage.crates,
            BTreeMap::from([
                (Baseline::Serde, ungated()),
                (Baseline::SerdeJson, ungated())
            ]),
        );
    }

    /// A crate's name inside a string literal or a doc comment, including an example that would
    /// compile, is prose and names nothing.
    #[test_util::test]
    fn literals_and_doc_comments_name_nothing() {
        let usage = scan(&[(
            "types.rs",
            indoc::indoc! {r#"
                    /// ```
                    /// let value = serde_json::json!({});
                    /// ```
                    #[doc = "thiserror::Error"]
                    pub struct Pet;
                    pub const NAME: &str = "indoc::indoc";
                "#},
        )]);
        assert_eq!(usage.crates, BTreeMap::new());
    }

    /// An item's own `cfg(feature)` gates it, and a module the crate root declares behind a
    /// feature is gated throughout its file.
    #[test_util::test]
    fn a_simple_feature_gate_is_tracked() {
        let usage = scan(&[
            (
                "lib.rs",
                indoc::indoc! {r#"
                        #[cfg(feature = "client")]
                        pub mod client;
                        pub mod types;
                    "#},
            ),
            (
                "client.rs",
                "pub fn text() -> &'static str { indoc::indoc!(\"a\") }",
            ),
            (
                "types.rs",
                indoc::indoc! {r#"
                        #[cfg(feature = "schema")]
                        impl Pet { fn schema() -> serde_json::Value { serde_json::Value::Null } }
                        pub struct Pet;
                    "#},
            ),
        ]);
        assert_eq!(
            usage.crates,
            BTreeMap::from([
                (Baseline::SerdeJson, gated(&["schema"])),
                (Baseline::Indoc, gated(&["client"])),
            ]),
        );
        // Each optional crate is turned on by the feature that gates its uses.
        assert_eq!(
            usage.features(),
            BTreeMap::from([
                ("client".to_owned(), vec!["dep:indoc".to_owned()]),
                ("schema".to_owned(), vec!["dep:serde_json".to_owned()]),
            ]),
        );
        assert_eq!(
            usage.dependencies(),
            [
                r#"serde_json = { version = "1", optional = true }"#,
                r#"indoc = { version = "2", optional = true }"#,
            ],
        );
    }

    /// A predicate other than one bare feature counts as no gate, and an ungated use anywhere
    /// makes the crate a plain dependency however many gated uses it also has.
    #[test_util::test]
    fn any_other_predicate_counts_as_ungated() {
        let usage = scan(&[(
            "lib.rs",
            indoc::indoc! {r#"
                    #[cfg(any(feature = "a", feature = "b"))]
                    pub fn one() -> serde_json::Value { serde_json::Value::Null }
                    #[cfg_attr(all(feature = "a"), derive(thiserror::Error))]
                    pub struct Two;
                    #[cfg(feature = "a")]
                    pub fn three() -> &'static str { indoc::indoc!("a") }
                    pub fn four() -> &'static str { indoc::indoc!("b") }
                "#},
        )]);
        assert_eq!(
            usage.crates,
            BTreeMap::from([
                (Baseline::SerdeJson, ungated()),
                (
                    Baseline::Indoc,
                    Usage {
                        ungated: true,
                        features: ["a".to_owned()].into(),
                    }
                ),
                (Baseline::Thiserror, ungated()),
            ]),
        );
        assert_eq!(
            usage.dependencies(),
            [
                r#"serde_json = "1""#,
                r#"indoc = "2""#,
                r#"thiserror = "2""#
            ],
        );
        assert_eq!(usage.features(), BTreeMap::new());
    }
}
