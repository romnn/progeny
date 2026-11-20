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
use syn::parse::Parser as _;

use crate::config::ExtraDerives;

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
    /// In the types module, a derive that one of `extra_derives` configures is left out: the
    /// caller supplies the crate it comes from.
    pub(super) fn scan(
        files: &BTreeMap<Utf8PathBuf, String>,
        src: &Utf8Path,
        extra_derives: &[ExtraDerives],
    ) -> Self {
        let caller_derives: BTreeSet<String> = extra_derives
            .iter()
            .flat_map(|rule| &rule.derives)
            .filter_map(|path| syn::parse_str::<syn::Path>(path).ok())
            .map(|path| path.to_token_stream().to_string())
            .collect();
        let none = BTreeSet::new();
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
            let skipped = if module == "types" {
                &caller_derives
            } else {
                &none
            };
            let mut scanner = Scanner {
                usage: &mut usage,
                skipped,
            };
            match syn::parse_file(text) {
                Ok(file) => scanner.file(&file, gate),
                // Unformatted fallback text the scan cannot read: every crate counts as named.
                Err(_) => {
                    for krate in Baseline::ALL {
                        scanner.record(krate, None);
                    }
                }
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

/// One file's walk, recording into the crate's usage.
struct Scanner<'a> {
    usage: &'a mut BaselineUse,
    /// The caller's derive paths, as token text, which this file's derives do not count.
    skipped: &'a BTreeSet<String>,
}

impl Scanner<'_> {
    fn file(&mut self, file: &syn::File, gate: Option<&str>) {
        for attribute in &file.attrs {
            self.tokens(attribute.to_token_stream(), gate);
        }
        for item in &file.items {
            let tokens: Vec<TokenTree> = item.to_token_stream().into_iter().collect();
            let own = item_gate(&tokens);
            self.trees(&tokens, own.as_deref().or(gate));
        }
    }

    fn tokens(&mut self, stream: TokenStream, gate: Option<&str>) {
        let tokens: Vec<TokenTree> = stream.into_iter().collect();
        self.trees(&tokens, gate);
    }

    fn trees(&mut self, tokens: &[TokenTree], gate: Option<&str>) {
        let mut rest = tokens;
        let mut index = 0;
        while let Some((token, after)) = rest.split_first() {
            if let Some((attribute, after)) = attribute(rest) {
                self.attribute(attribute.stream(), gate);
                index += rest.len() - after.len();
                rest = after;
                continue;
            }
            match token {
                TokenTree::Group(group) => self.tokens(group.stream(), gate),
                TokenTree::Ident(ident) => {
                    if let Some(krate) = Baseline::named(ident)
                        && begins_path(tokens, index)
                    {
                        self.record(krate, gate);
                    }
                }
                TokenTree::Punct(_) | TokenTree::Literal(_) => {}
            }
            index += 1;
            rest = after;
        }
    }

    /// The inside of one `#[…]`.
    ///
    /// A `cfg_attr` on a simple feature gates what it applies by that feature, and a derive list
    /// is read path by path so the caller's own derives can be left out.
    fn attribute(&mut self, stream: TokenStream, gate: Option<&str>) {
        let tokens: Vec<TokenTree> = stream.into_iter().collect();
        if let [TokenTree::Ident(name), TokenTree::Group(arguments)] = tokens.as_slice()
            && arguments.delimiter() == Delimiter::Parenthesis
        {
            if name == "derive" {
                self.derives(arguments.stream(), gate);
                return;
            }
            if name == "cfg_attr" {
                self.cfg_attr(arguments.stream(), gate);
                return;
            }
        }
        self.trees(&tokens, gate);
    }

    fn cfg_attr(&mut self, arguments: TokenStream, gate: Option<&str>) {
        let mut pieces = split_commas(arguments).into_iter();
        let feature = pieces.next().and_then(simple_feature);
        let gate = feature.as_deref().or(gate);
        for attribute in pieces {
            self.attribute(attribute, gate);
        }
    }

    fn derives(&mut self, list: TokenStream, gate: Option<&str>) {
        let parser = syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated;
        let Ok(paths) = parser.parse2(list.clone()) else {
            self.tokens(list, gate);
            return;
        };
        for path in paths {
            let path = path.to_token_stream();
            if !self.skipped.contains(&path.to_string()) {
                self.tokens(path, gate);
            }
        }
    }

    fn record(&mut self, krate: Baseline, gate: Option<&str>) {
        let usage = self.usage.crates.entry(krate).or_default();
        match gate {
            None => usage.ungated = true,
            Some(feature) => {
                usage.features.insert(feature.to_owned());
            }
        }
    }
}

/// The `[…]` group of an attribute that starts `tokens`, `#[…]` or `#![…]`, with what follows it.
fn attribute(tokens: &[TokenTree]) -> Option<(&proc_macro2::Group, &[TokenTree])> {
    let [TokenTree::Punct(pound), rest @ ..] = tokens else {
        return None;
    };
    if pound.as_char() != '#' {
        return None;
    }
    let rest = match rest {
        [TokenTree::Punct(bang), rest @ ..] if bang.as_char() == '!' => rest,
        _ => rest,
    };
    match rest {
        [TokenTree::Group(group), rest @ ..] if group.delimiter() == Delimiter::Bracket => {
            Some((group, rest))
        }
        _ => None,
    }
}

/// The feature an item's own outer `#[cfg(feature = "…")]` names.
fn item_gate(tokens: &[TokenTree]) -> Option<String> {
    let mut rest = tokens;
    while let Some((group, after)) = attribute(rest) {
        if let Ok(meta) = syn::parse2::<syn::Meta>(group.stream())
            && let Some(feature) = cfg_feature(&meta)
        {
            return Some(feature);
        }
        rest = after;
    }
    None
}

/// Whether the identifier at `index` begins a path: followed by `::` and not itself following
/// `<segment>::`.
///
/// A leading `::serde::x` begins one as much as a bare `serde::x` does.
fn begins_path(tokens: &[TokenTree], index: usize) -> bool {
    let Some((_, after)) = tokens
        .split_at_checked(index)
        .and_then(|(_, at)| at.split_first())
    else {
        return false;
    };
    let followed = matches!(after, [first, second, ..] if is_path_separator(first, second));
    let before = tokens.get(..index).unwrap_or_default();
    let continues = matches!(
        before,
        [.., TokenTree::Ident(segment), first, second]
            if is_path_separator(first, second) && is_segment(segment)
    );
    followed && !continues
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

/// Whether an identifier before `::` is a path segment rather than a keyword that can precede a
/// path, such as `use ::serde::Serialize` or `impl ::serde::Serialize for`.
fn is_segment(ident: &proc_macro2::Ident) -> bool {
    /// Rust's keywords, less the four that are path segments themselves.
    const KEYWORDS: &[&str] = &[
        "abstract", "as", "async", "await", "become", "box", "break", "const", "continue", "do",
        "dyn", "else", "enum", "extern", "false", "final", "fn", "for", "gen", "if", "impl", "in",
        "let", "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref",
        "return", "static", "struct", "trait", "true", "try", "type", "typeof", "unsafe",
        "unsized", "use", "virtual", "where", "while", "yield",
    ];
    !KEYWORDS.contains(&ident.to_string().as_str())
}

/// An argument list split at its top-level commas.
fn split_commas(stream: TokenStream) -> Vec<TokenStream> {
    let mut pieces = vec![TokenStream::new()];
    for token in stream {
        match (&token, pieces.last_mut()) {
            (TokenTree::Punct(comma), _) if comma.as_char() == ',' => {
                pieces.push(TokenStream::new());
            }
            (_, Some(piece)) => piece.extend([token]),
            (_, None) => {}
        }
    }
    pieces.retain(|piece| !piece.is_empty());
    pieces
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use camino::{Utf8Path, Utf8PathBuf};
    use color_eyre::eyre;

    use super::{Baseline, BaselineUse, Usage};
    use crate::config::{ExtraDerives, SerdeAttributes, TypeSelection};

    /// Scans one crate whose `src` holds the given files.
    fn scan(files: &[(&str, &str)], extra_derives: &[ExtraDerives]) -> BaselineUse {
        let files: BTreeMap<Utf8PathBuf, String> = files
            .iter()
            .map(|&(path, text)| (Utf8Path::new("src").join(path), text.to_owned()))
            .collect();
        BaselineUse::scan(&files, Utf8Path::new("src"), extra_derives)
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

    fn derives(paths: &[&str]) -> ExtraDerives {
        ExtraDerives {
            derives: paths.iter().map(|&it| it.to_owned()).collect(),
            types: TypeSelection::All,
            nested: true,
            feature: None,
            serde_attributes: SerdeAttributes::Ignored,
        }
    }

    /// A crate counts where it begins a path, bare or with a leading `::`, and not where it is
    /// a later segment of another crate's path or is not followed by `::` at all.
    #[test_util::test]
    fn a_crate_is_named_where_it_begins_a_path() {
        let usage = scan(
            &[(
                "lib.rs",
                indoc::indoc! {"
                    use serde::Serialize;
                    pub fn value() -> ::serde_json::Value {
                        ::fixtures::indoc::Fixture::default();
                        other::thiserror::Error::new();
                        let thiserror = 1;
                        ::serde_json::Value::Null
                    }
                "},
            )],
            &[],
        );
        assert_eq!(
            usage.crates,
            BTreeMap::from([
                (Baseline::Serde, ungated()),
                (Baseline::SerdeJson, ungated())
            ]),
        );
    }

    /// A keyword before `::` is no path segment, so the path after it still begins with the crate.
    #[test_util::test]
    fn a_path_after_a_keyword_begins_with_its_crate() {
        let usage = scan(
            &[(
                "lib.rs",
                indoc::indoc! {"
                    impl ::serde::Serialize for Pet {}
                    pub fn value(value: &mut ::serde_json::Value) -> bool {
                        matches!(value, x if x == &::indoc::indoc!(\"a\"))
                    }
                "},
            )],
            &[],
        );
        assert_eq!(
            usage.crates,
            BTreeMap::from([
                (Baseline::Serde, ungated()),
                (Baseline::SerdeJson, ungated()),
                (Baseline::Indoc, ungated()),
            ]),
        );
    }

    /// A crate's name inside a string literal or a doc comment, including an example that would
    /// compile, is prose and names nothing.
    #[test_util::test]
    fn literals_and_doc_comments_name_nothing() {
        let usage = scan(
            &[(
                "types.rs",
                indoc::indoc! {r#"
                    /// ```
                    /// let value = serde_json::json!({});
                    /// ```
                    #[doc = "thiserror::Error"]
                    pub struct Pet;
                    pub const NAME: &str = "indoc::indoc";
                "#},
            )],
            &[],
        );
        assert_eq!(usage.crates, BTreeMap::new());
    }

    /// An item's own `cfg(feature)` gates it, a module the crate root declares behind a feature
    /// is gated throughout its file, and a `cfg_attr` on a feature gates what it applies.
    #[test_util::test]
    fn a_simple_feature_gate_is_tracked() {
        let usage = scan(
            &[
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
                        #[cfg_attr(feature = "errors", derive(thiserror::Error))]
                        pub struct Pet;
                    "#},
                ),
            ],
            &[],
        );
        assert_eq!(
            usage.crates,
            BTreeMap::from([
                (Baseline::SerdeJson, gated(&["schema"])),
                (Baseline::Indoc, gated(&["client"])),
                (Baseline::Thiserror, gated(&["errors"])),
            ]),
        );
        // Each optional crate is turned on by the feature that gates its uses.
        assert_eq!(
            usage.features(),
            BTreeMap::from([
                ("client".to_owned(), vec!["dep:indoc".to_owned()]),
                ("errors".to_owned(), vec!["dep:thiserror".to_owned()]),
                ("schema".to_owned(), vec!["dep:serde_json".to_owned()]),
            ]),
        );
        assert_eq!(
            usage.dependencies(),
            [
                r#"serde_json = { version = "1", optional = true }"#,
                r#"indoc = { version = "2", optional = true }"#,
                r#"thiserror = { version = "2", optional = true }"#,
            ],
        );
    }

    /// A predicate other than one bare feature counts as no gate, and an ungated use anywhere
    /// makes the crate a plain dependency however many gated uses it also has.
    #[test_util::test]
    fn any_other_predicate_counts_as_ungated() {
        let usage = scan(
            &[(
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
            )],
            &[],
        );
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

    /// The caller's configured derives are skipped in the types module, matched as parsed paths,
    /// and still count everywhere else.
    #[test_util::test]
    fn the_callers_derives_count_only_outside_the_types_module() {
        let source = indoc::indoc! {r#"
            #[derive(Debug, ::thiserror::Error)]
            #[cfg_attr(feature = "fixtures", derive(serde :: Serialize))]
            pub struct Pet;
        "#};
        let configured = [derives(&["thiserror::Error", "::serde::Serialize"])];
        let configured_bare = [derives(&["::thiserror::Error", "serde::Serialize"])];

        let types = scan(&[("types.rs", source)], &configured_bare);
        assert_eq!(types.crates, BTreeMap::new());

        // A path that differs from the configured one only by its leading `::` is another path.
        let types = scan(&[("types.rs", source)], &configured);
        assert_eq!(
            types.crates,
            BTreeMap::from([
                (Baseline::Serde, gated(&["fixtures"])),
                (Baseline::Thiserror, ungated()),
            ]),
        );

        let support = scan(&[("support.rs", source)], &configured_bare);
        assert_eq!(
            support.crates,
            BTreeMap::from([
                (Baseline::Serde, gated(&["fixtures"])),
                (Baseline::Thiserror, ungated()),
            ]),
        );
    }

    /// A file the scan cannot parse names every crate, so its manifest declares them all.
    #[test_util::test]
    fn an_unreadable_file_names_every_crate() {
        let usage = scan(&[("lib.rs", "pub fn (")], &[]);
        assert_eq!(
            usage.dependencies(),
            [
                r#"serde = { version = "1.0.164", features = ["derive"] }"#,
                r#"serde_json = "1""#,
                r#"indoc = "2""#,
                r#"thiserror = "2""#,
            ],
        );
    }
}
