//! Rendering types: a direct transcription of the contract records.
//!
//! **This module makes no decisions.** Every question it could ask — what a field is called on the
//! wire, whether an absent key is legal, which derives appear, how long a tuple is — was answered in
//! [`crate::contract`] and is sitting in the record it is handed. When the derive strategy is
//! selected the same record renders as `#[serde(...)]` attributes; when the hand-written strategy is
//! selected it renders as function bodies instead. Two renderings of one record, which is the whole
//! discipline in one sentence.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::config::{BytesRepr, Config, DateTimeCrate, MapKind, UuidCrate};
use crate::contract::{
    ContractKind, Contracts, DeserStrategy, FieldContract, RustIdent, SkipRule, Tagging,
    TypeContract, TypeRef,
};
use crate::shape::{Docs, Format};

/// Render every type in the contract set.
pub(super) fn render(contracts: &Contracts, config: &Config) -> TokenStream {
    let items = contracts
        .types()
        .iter()
        .map(|contract| one(contract, contracts, config));
    quote! { #(#items)* }
}

fn one(contract: &TypeContract, contracts: &Contracts, config: &Config) -> TokenStream {
    let name = ident(contract.rust_name());
    // A type the document documented says what it is; one it did not says where it came from, which
    // is the next most useful thing for someone reading checked-in generated source.
    let docs = if contract.docs().is_empty() {
        let origin = format!(" Generated from `{}`.", contract.origin());
        quote! { #[doc = #origin] }
    } else {
        docs(contract.docs())
    };
    // The serde derives join the typed derive set in one attribute: which of them appears is the
    // serde strategy's business, and the strategy was decided by the eligibility function.
    let derives = derives(contract);

    match contract.kind() {
        ContractKind::Struct { fields } => {
            let deny = match (contract.deser(), contract.unknown_fields()) {
                (DeserStrategy::Derive, crate::config::UnknownFields::Deny) => {
                    quote! { #[serde(deny_unknown_fields)] }
                }
                _ => quote! {},
            };
            let members = fields
                .iter()
                .map(|field| member(field, contract, contracts, config));
            let allow = deprecated_use(fields.iter().map(|field| &field.ty), contracts);
            // A field's type is as nested as the schema that produced it — `cloudflare` writes an
            // `Option<Vec<BTreeMap<String, …>>>` — and `clippy::type_complexity` asks for it to be
            // factored into an alias. progeny would have to *invent that alias's name*, and every
            // name in the output comes from the document. A named type the document never mentions
            // is a worse outcome than a long one that says exactly what the schema said.
            let nesting = quote! { #[allow(clippy::type_complexity)] };
            quote! {
                #docs
                #derives
                #deny
                #allow
                #nesting
                pub struct #name {
                    #(#members)*
                }
            }
        }
        ContractKind::Enum { variants } => {
            let body = data_enum(variants, contract, contracts, config);
            quote! {
                #docs
                #derives
                #body
            }
        }
        ContractKind::StringEnum { variants } => {
            let with_serde = contract.deser() == DeserStrategy::Derive;
            let arms = variants.iter().map(|variant| {
                let variant_ident = ident(&variant.rust_name);
                let wire = &variant.wire_name;
                // Only under the derive: with no serde derive on the item, a `#[serde(...)]` helper
                // attribute does not even resolve, and the crate fails to compile. Stripping them is
                // mandatory rather than tidy.
                let rename = (with_serde && variant_ident != *wire)
                    .then(|| quote! { #[serde(rename = #wire)] });
                quote! { #rename #variant_ident, }
            });
            quote! {
                #docs
                #derives
                pub enum #name {
                    #(#arms)*
                }
            }
        }
        ContractKind::Newtype { inner } => {
            let ty = type_ref(inner, contracts, config);
            let allow = deprecated_use([inner], contracts);
            quote! {
                #docs
                #derives
                #allow
                pub struct #name(pub #ty);
            }
        }
        ContractKind::Tuple { items } => {
            let members = items.iter().map(|item| {
                let ty = type_ref(item, contracts, config);
                quote! { pub #ty, }
            });
            let allow = deprecated_use(items.iter(), contracts);
            quote! {
                #docs
                #derives
                #allow
                pub struct #name(#(#members)*);
            }
        }
        ContractKind::Alias { target } => {
            let ty = type_ref(target, contracts, config);
            let allow = deprecated_use([target], contracts);
            quote! {
                #docs
                #allow
                pub type #name = #ty;
            }
        }
    }
}

/// A data-carrying enum: how it says which variant it is, and what each variant holds.
///
/// The tagging attribute and the per-variant rename are two readings of one contract field, which
/// is why they are written together: `Untagged` has no variant names on the wire to rename, and
/// `Internal` has exactly one name per variant and no choice about using it.
fn data_enum(
    variants: &[crate::contract::VariantContract],
    contract: &TypeContract,
    contracts: &Contracts,
    config: &Config,
) -> TokenStream {
    let name = ident(contract.rust_name());
    let tagging = match (contract.deser(), contract.tagging()) {
        (DeserStrategy::Derive, Tagging::Untagged) => quote! { #[serde(untagged)] },
        (DeserStrategy::Derive, Tagging::Internal { tag }) => quote! { #[serde(tag = #tag)] },
        // The hand-written path reads the same contract and consults no attributes, so leaving one
        // on would be a second source of truth — and would not resolve without the derive anyway.
        (DeserStrategy::HandWrittenBuffered | DeserStrategy::HandWrittenFieldless, _) => quote! {},
    };
    let with_serde = contract.deser() == DeserStrategy::Derive;
    let arms = variants.iter().map(|variant| {
        let variant_ident = ident(&variant.rust_name);
        let ty = type_ref(&variant.ty, contracts, config);
        let rename = variant
            .tag_value
            .as_deref()
            .filter(|wire| with_serde && variant_ident != *wire)
            .map(|wire| quote! { #[serde(rename = #wire)] });
        quote! { #rename #variant_ident(#ty), }
    });
    let allow = deprecated_use(variants.iter().map(|variant| &variant.ty), contracts);
    let sized = variant_sizes();
    quote! {
        #tagging
        #allow
        #sized
        pub enum #name {
            #(#arms)*
        }
    }
}

/// The one lint on generated types that suppression is the right answer to.
///
/// `clippy::large_enum_variant` fires when an enum's variants differ enough in size, and the fix it
/// asks for is a `Box` — a change to the type the consumer receives. Deciding that here would mean
/// knowing the layout of every generated type, and **progeny cannot**: a field's type may be
/// `chrono::DateTime`, `time::OffsetDateTime`, `uuid::Uuid`, or whichever map the configuration
/// picked, and those layouts belong to crates at versions this build never sees. The threshold is
/// clippy's own, configurable and free to move between releases. A `Box` placed on an estimate is a
/// worse outcome than the wart: it is an API change made on a guess, and it would appear and
/// disappear as an unrelated configuration knob moved.
///
/// So the size question stays the consumer's, who can measure it, and the warning stops being
/// noise in a build about code nobody wrote. On the enum rather than the crate, so it says which
/// construct it is about, and `#[allow]` rather than `#[expect]` because most enums never trip it —
/// an expectation would be unfulfilled on nearly all of them.
pub(super) fn variant_sizes() -> TokenStream {
    quote! { #[allow(clippy::large_enum_variant)] }
}

fn member(
    field: &FieldContract,
    contract: &TypeContract,
    contracts: &Contracts,
    config: &Config,
) -> TokenStream {
    let name = ident(&field.rust_name);
    let ty = type_ref(&field.ty, contracts, config);
    let docs = with_default(docs(&field.docs), field);
    if contract.deser() != DeserStrategy::Derive {
        // No serde attributes at all: the hand-written implementation reads the same contract and
        // does not consult attributes, so leaving them on would be a second source of truth.
        return quote! { #docs pub #name: #ty, };
    }

    let mut attributes = Vec::new();
    if field.flatten {
        attributes.push(quote! { flatten });
    } else if name != field.wire_name {
        let wire = &field.wire_name;
        attributes.push(quote! { rename = #wire });
    }
    if field.skip_serializing_if == SkipRule::WhenNone && !field.flatten {
        attributes.push(quote! { skip_serializing_if = "Option::is_none" });
    }
    let serde = (!attributes.is_empty()).then(|| quote! { #[serde(#(#attributes),*)] });
    quote! { #docs #serde pub #name: #ty, }
}

/// A field's declared default, said in its documentation rather than applied on deserialize.
///
/// **`#[serde(default = "…")]` would be a wire defect.** OpenAPI's `default` states what the *server*
/// assumes when a member is absent; serde's fills the field in on the way *in*, and the field is
/// then written on the way *out*. On a request body that turns "the caller said nothing" into "the
/// caller said `false`" — a different request, sent silently, which is the one forbidden failure
/// mode. The payload gate caught it on its first run over the corpus: 60 examples across three
/// documents came back carrying members they never had.
///
/// Nothing is lost by dropping the attribute, because every non-required field is an `Option` and
/// serde reads an absent `Option` as `None` without being told to. What *is* lost is the convenience
/// of reading a server's default off an absent member, so the value is said out loud instead.
fn with_default(docs: TokenStream, field: &FieldContract) -> TokenStream {
    let Some(default) = &field.default else {
        return docs;
    };
    let note = format!(" The server assumes `{default}` when this member is absent.");
    quote! { #docs #[doc = ""] #[doc = #note] }
}

fn derives(contract: &TypeContract) -> TokenStream {
    if matches!(contract.kind(), ContractKind::Alias { .. }) {
        return quote! {};
    }
    let names = contract
        .derives()
        .iter()
        .map(|derive| format_ident!("{}", derive.name()));
    // The serde derives are only present under the derive strategy; the hand-written path carries
    // no serde attributes at all, so it must carry no serde derive either.
    let serde = match contract.deser() {
        DeserStrategy::Derive => quote! { , serde::Serialize, serde::Deserialize },
        DeserStrategy::HandWrittenBuffered | DeserStrategy::HandWrittenFieldless => quote! {},
    };
    quote! { #[derive(#(#names),* #serde)] }
}

/// A type reference, spelled out.
pub(super) fn type_ref(ty: &TypeRef, contracts: &Contracts, config: &Config) -> TokenStream {
    reference(ty, contracts, config, false)
}

/// The allowance an item needs when any type it names is deprecated.
///
/// A document may deprecate a schema without deprecating the properties that refer to it — `okta`
/// deprecates the `MtlsTrustCredentialsRevocation` component and not the `revocation` property that
/// holds one — and both renderings are then faithful. rustc lints the *use* regardless, so the
/// generated crate warns on every build about code its consumer did not write. Being deprecated
/// itself exempts nothing: a `#[deprecated]` item is still linted for the deprecated types it names.
///
/// **On the item, not on the field.** A field-level `#[allow]` silences the field's own declaration
/// and not the `Deserialize` the derive expands from it, which names the same type at the same span
/// — so the warning survives at half its old count, which is the worst of both. The item level is
/// the narrowest one that covers a derive, and it hides nothing from the consumer: it governs uses
/// *inside* this item, while their own use of a deprecated field or type is linted at their site.
pub(super) fn deprecated_use<'a>(
    types: impl IntoIterator<Item = &'a TypeRef>,
    contracts: &Contracts,
) -> TokenStream {
    let mut named = Vec::new();
    for ty in types {
        ty.named(&mut named);
    }
    let touches_deprecated = named
        .iter()
        .any(|index| contracts.get(*index).is_some_and(|it| it.docs().deprecated));
    // `allow` rather than `expect`: this lands in someone else's crate, compiled by a rustc this
    // build never sees, and an expectation that turns out unfulfilled there is a warning of its own.
    if touches_deprecated {
        quote! { #[allow(deprecated)] }
    } else {
        TokenStream::new()
    }
}

/// The same type, named from outside the `types` module.
///
/// A named type renders as a bare identifier inside `types.rs` and must not anywhere else: the
/// client module re-exports `Error` from the support module, and a document with a schema called
/// `Error` — the petstore has one — would otherwise produce `Error<Error>` whose two `Error`s are
/// different types. The bug is silent, because it still compiles.
pub(super) fn type_path(ty: &TypeRef, contracts: &Contracts, config: &Config) -> TokenStream {
    reference(ty, contracts, config, true)
}

fn reference(ty: &TypeRef, contracts: &Contracts, config: &Config, qualified: bool) -> TokenStream {
    let type_ref = |inner: &TypeRef| reference(inner, contracts, config, qualified);
    match ty {
        TypeRef::Named(index) => {
            if let Some(contract) = contracts.get(*index) {
                let name = ident(contract.rust_name());
                if qualified {
                    quote! { super::types::#name }
                } else {
                    quote! { #name }
                }
            } else {
                // Unreachable: every index comes from the contract set it is rendered against.
                quote! { serde_json::Value }
            }
        }
        TypeRef::Unit => quote! { () },
        TypeRef::Bool => quote! { bool },
        TypeRef::I64 => quote! { i64 },
        TypeRef::U64 => quote! { u64 },
        TypeRef::F64 => quote! { f64 },
        TypeRef::String => quote! { String },
        TypeRef::Format(format) => format_type(*format, config),
        TypeRef::Value => quote! { serde_json::Value },
        TypeRef::Option(inner) => {
            let inner = type_ref(inner);
            quote! { Option<#inner> }
        }
        TypeRef::Vec(inner) => {
            let inner = type_ref(inner);
            quote! { Vec<#inner> }
        }
        TypeRef::Map(inner) => {
            let inner = type_ref(inner);
            match config.map {
                MapKind::BTreeMap => quote! { std::collections::BTreeMap<String, #inner> },
                MapKind::HashMap => quote! { std::collections::HashMap<String, #inner> },
                MapKind::IndexMap => quote! { indexmap::IndexMap<String, #inner> },
            }
        }
        TypeRef::Array(inner, len) => {
            let inner = type_ref(inner);
            let len = usize::try_from(*len).unwrap_or(0);
            quote! { [#inner; #len] }
        }
        TypeRef::Tuple(items) => {
            let items = items.iter().map(&type_ref);
            quote! { (#(#items),*) }
        }
        TypeRef::Boxed(inner) => {
            let inner = type_ref(inner);
            quote! { Box<#inner> }
        }
    }
}

/// The type a format renders as, which is the caller's choice.
///
/// `Base64` and `Binary` are `String` whatever the byte representation is set to, and that is not an
/// omission: inside a JSON payload a base64 or binary property *is* a string, and turning it into
/// bytes needs a codec the generated crate does not have. The byte representation applies to a raw
/// binary request or response body, which is the API model's business.
fn format_type(format: Format, config: &Config) -> TokenStream {
    match format {
        Format::DateTime => match config.formats.date_time {
            DateTimeCrate::String => quote! { String },
            DateTimeCrate::Chrono => quote! { chrono::DateTime<chrono::Utc> },
            DateTimeCrate::Time => quote! { time::OffsetDateTime },
            DateTimeCrate::Jiff => quote! { jiff::Timestamp },
        },
        Format::Date => match config.formats.date_time {
            DateTimeCrate::String => quote! { String },
            DateTimeCrate::Chrono => quote! { chrono::NaiveDate },
            DateTimeCrate::Time => quote! { time::Date },
            DateTimeCrate::Jiff => quote! { jiff::civil::Date },
        },
        Format::Time => match config.formats.date_time {
            DateTimeCrate::String => quote! { String },
            DateTimeCrate::Chrono => quote! { chrono::NaiveTime },
            DateTimeCrate::Time => quote! { time::Time },
            DateTimeCrate::Jiff => quote! { jiff::civil::Time },
        },
        Format::Uuid => match config.formats.uuid {
            UuidCrate::String => quote! { String },
            UuidCrate::Uuid => quote! { uuid::Uuid },
        },
        Format::Base64 | Format::Binary => match config.formats.bytes {
            BytesRepr::Vec | BytesRepr::Bytes => quote! { String },
        },
    }
}

fn ident(name: &RustIdent) -> proc_macro2::Ident {
    format_ident!("{}", name.as_str())
}

/// Doc comments, one attribute per line so a multi-line description reads as one.
pub(super) fn docs(docs: &Docs) -> TokenStream {
    let mut lines: Vec<String> = Vec::new();
    if let Some(title) = &docs.title {
        lines.extend(wrap(title));
    }
    if let Some(description) = &docs.description {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.extend(wrap(description));
    }
    let attributes = lines.iter().map(|line| {
        let text = format!(" {line}");
        quote! { #[doc = #text] }
    });
    let deprecated = docs.deprecated.then(|| quote! { #[deprecated] });
    quote! { #(#attributes)* #deprecated }
}

/// Split a description into doc-comment lines, in markdown a consumer's build will not complain
/// about.
///
/// Vendor prose is transcribed, never rewritten — but the transcription has to survive being read
/// as rustdoc markdown, and three things in real descriptions do not. A **tab**, whose width
/// rustdoc does not define. And the two forms of **lazy continuation**: a paragraph line belonging
/// to a list item or a blockquote that leaves out the indent, or the `>`, that would say so
/// explicitly. `CommonMark` defines each as equivalent to its explicit form, so writing the explicit
/// form renders identically and removes a warning the consumer would otherwise get in their own
/// build, about prose they did not write.
///
/// This is not hypothetical markdown pedantry. `posthog` describes an endpoint with a parenthesis
/// that wraps onto a line starting `+ the spec it derived…`, which markdown reads as a list item
/// and every line after it as a lazy continuation of one — 79 warnings from one habit of writing.
///
/// Fenced code keeps its indentation, because inside a fence indentation is content. Its tabs are
/// still expanded — the lint fires there too, and four-column stops are what rustdoc would have
/// shown anyway.
fn wrap(text: &str) -> Vec<String> {
    let text = text.replace('\r', "");
    let mut out = Vec::new();
    let mut fence: Option<String> = None;
    // What a lazy line in the block now open should have been written with, and whether a
    // paragraph is open inside it. Two pieces of state rather than one, because a blank line ends
    // the paragraph *without* closing the block: a list item continues across one, and `okta`
    // writes a second paragraph inside an item and then wraps it lazily back to column zero.
    // Collapsing them loses the item at the blank line and leaves everything after it unindented.
    let mut continuation: Option<String> = None;
    let mut paragraph = false;
    for raw in text.lines() {
        let line = expand_tabs(raw);
        if let Some(open) = &fence {
            if closes_fence(&line, open) {
                fence = None;
            }
            out.push(line);
            continue;
        }
        if let Some(open) = opens_fence(&line) {
            fence = Some(open);
            paragraph = false;
            out.push(line);
            continue;
        }
        if line.trim().is_empty() {
            paragraph = false;
            out.push(line);
            continue;
        }
        let indent = line.len() - line.trim_start_matches(' ').len();
        // Where the innermost open block's content starts. Every indentation question below is
        // asked relative to this rather than to column zero, which is the whole difficulty with
        // nested lists: `orb` writes a sub-item at column 4, and read absolutely that is an
        // indented code block, while read against its parent's content column of 2 it is what it
        // looks like. Getting that backwards flattens the sub-item and strands its continuation.
        let content = continuation.as_ref().map_or(0, String::len);
        if indent >= content + 4 {
            out.push(line);
            continue;
        }
        if let Some(prefix) = block_prefix(&line, indent) {
            continuation = Some(prefix);
            paragraph = true;
            out.push(line);
            continue;
        }
        if paragraph && let Some(prefix) = &continuation {
            out.push(continued(&line, prefix));
            continue;
        }
        // The first line of a paragraph after a blank one. It stays inside the open block when it
        // is indented into it, and closes the block when it is not.
        if indent < content {
            continuation = None;
        }
        paragraph = true;
        out.push(line);
    }
    out
}

/// Tabs, as the four-column stops rustdoc assumes and does not promise.
fn expand_tabs(line: &str) -> String {
    if !line.contains('\t') {
        return line.to_owned();
    }
    let mut out = String::with_capacity(line.len());
    for character in line.chars() {
        if character == '\t' {
            let width = 4 - (out.chars().count() % 4);
            out.extend(std::iter::repeat_n(' ', width));
        } else {
            out.push(character);
        }
    }
    out
}

fn opens_fence(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    for marker in ['`', '~'] {
        let run = trimmed.chars().take_while(|it| *it == marker).count();
        if run >= 3 {
            return Some(std::iter::repeat_n(marker, run).collect());
        }
    }
    None
}

fn closes_fence(line: &str, open: &str) -> bool {
    let trimmed = line.trim();
    let marker = open.chars().next().unwrap_or('`');
    trimmed.len() >= open.len() && trimmed.chars().all(|it| it == marker)
}

/// What continuations of the block this line opens have to be written with, if it opens one.
///
/// The caller has already ruled out an indented code block, relative to the block now open.
fn block_prefix(line: &str, indent: usize) -> Option<String> {
    let rest = &line[indent..];
    if rest.starts_with('>') {
        return Some(format!("{}> ", " ".repeat(indent)));
    }
    let marker = list_marker(rest)?;
    // A list item's continuations line up with its content, which is where clippy points.
    Some(" ".repeat(indent + marker))
}

/// The width of the list marker this line starts with, including the space after it.
fn list_marker(rest: &str) -> Option<usize> {
    let mut chars = rest.chars();
    let first = chars.next()?;
    let width = if matches!(first, '-' | '*' | '+') {
        1
    } else if first.is_ascii_digit() {
        // `1.` and `1)` both start an ordered list; `1` alone starts a sentence.
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if !matches!(rest.chars().nth(digits), Some('.' | ')')) {
            return None;
        }
        digits + 1
    } else {
        return None;
    };
    // Without a space it is emphasis, a horizontal rule, or a number — not a list.
    let after = &rest.get(width..)?;
    let spaces = after.len() - after.trim_start_matches(' ').len();
    (spaces > 0).then_some(width + spaces)
}

/// A lazy line, written out with the prefix it left implicit.
fn continued(line: &str, prefix: &str) -> String {
    format!("{prefix}{}", line.trim_start_matches(' '))
}

#[cfg(test)]
mod doc_tests {
    use super::wrap;

    fn normalized(text: &str) -> String {
        wrap(text).join("\n")
    }

    #[test]
    fn a_lazy_list_continuation_is_written_out() {
        // `posthog`: a parenthesis wraps onto a line starting `+ `, which markdown reads as a list
        // item — and every line after it as a lazy continuation of one.
        let found = normalized(
            "ask the janitor to seal it (the janitor returns the sha\n+ the spec it derived), then stamp the\nrow. No rollback.",
        );
        assert_eq!(
            found,
            "ask the janitor to seal it (the janitor returns the sha\n+ the spec it derived), then stamp the\n  row. No rollback."
        );
    }

    #[test]
    fn an_overindented_list_continuation_is_pulled_back_to_its_content() {
        // The same rule from the other side: the continuation belongs at the item's content
        // column, whether the vendor wrote too little indentation or too much.
        let found = normalized(
            "* If part index is included: the file matching the index (as ordered\n    by key) is downloaded.",
        );
        assert_eq!(
            found,
            "* If part index is included: the file matching the index (as ordered\n  by key) is downloaded."
        );
    }

    #[test]
    fn a_lazy_quote_continuation_gets_its_marker() {
        // `okta` writes deprecation notices as blockquotes whose second line drops the `>`.
        let found =
            normalized("> **Note:** This property isn't supported.\nSee the deprecation notice.");
        assert_eq!(
            found,
            "> **Note:** This property isn't supported.\n> See the deprecation notice."
        );
    }

    #[test]
    fn a_blank_line_ends_the_block_rather_than_capturing_what_follows() {
        // Lazy continuation is a within-paragraph rule. Indenting past a blank line would move a
        // new paragraph *into* the list, which changes what the document says.
        let found = normalized("* an item\n\nA new paragraph.");
        assert_eq!(found, "* an item\n\nA new paragraph.");
    }

    #[test]
    fn fenced_code_is_left_exactly_as_written() {
        // Inside a fence, indentation is content.
        let found =
            normalized("* an item\n```\nnot   a continuation\n    indented on purpose\n```\ntail");
        assert_eq!(
            found,
            "* an item\n```\nnot   a continuation\n    indented on purpose\n```\ntail"
        );
    }

    #[test]
    fn tabs_become_spaces_because_rustdoc_does_not_define_their_width() {
        assert_eq!(normalized("a\tb"), "a   b");
        assert_eq!(normalized("\tindented"), "    indented");
    }

    #[test]
    fn what_only_looks_like_a_list_is_left_alone() {
        // Emphasis, a horizontal rule and a sentence that opens with a number all start with a
        // list marker's first character and none of them is a list.
        assert_eq!(normalized("*emphasis*\ncontinues"), "*emphasis*\ncontinues");
        assert_eq!(normalized("---\ncontinues"), "---\ncontinues");
        assert_eq!(
            normalized("2024 was the year\nit changed"),
            "2024 was the year\nit changed"
        );
    }

    #[test]
    fn an_indented_code_block_under_a_list_item_keeps_its_indentation() {
        let found = normalized("* an item\n      code, four past the content column");
        assert_eq!(found, "* an item\n      code, four past the content column");
    }
}

#[cfg(test)]
mod nested_doc_tests {
    use super::wrap;

    #[test]
    fn a_sub_item_is_read_against_its_parent_rather_than_column_zero() {
        // `orb` writes a sub-item at column 4 under an item whose content starts at column 2.
        // Read absolutely that is an indented code block; read against its parent it is a list.
        // The first reading flattens the sub-item and strands its own continuation above it.
        let found = wrap(
            "- outer item wrapping\n  its continuation:\n    - inner item wrapping\n      its continuation.",
        )
        .join("\n");
        assert_eq!(
            found,
            "- outer item wrapping\n  its continuation:\n    - inner item wrapping\n      its continuation."
        );
    }

    #[test]
    fn a_lazy_line_under_a_sub_item_lines_up_with_the_sub_item() {
        let found = wrap("- outer\n    - inner item wrapping\nits lazy continuation.").join("\n");
        assert_eq!(
            found,
            "- outer\n    - inner item wrapping\n      its lazy continuation."
        );
    }
}

#[cfg(test)]
mod paragraph_doc_tests {
    use super::wrap;

    #[test]
    fn a_second_paragraph_inside_a_list_item_keeps_the_item_open() {
        // `okta` writes a list item, a blank line, a second paragraph still inside the item, and
        // then wraps that paragraph lazily back to column zero. Treating the blank line as closing
        // the *item* rather than the paragraph loses the indent for everything after it.
        let found = wrap(
            "  * An optional filter. This is a rule.\n    See the guide.\n\n    Additionally, you can specify a key\nyou must supply when calling.\nEach call.",
        )
        .join("\n");
        assert_eq!(
            found,
            "  * An optional filter. This is a rule.\n    See the guide.\n\n    Additionally, you can specify a key\n    you must supply when calling.\n    Each call."
        );
    }

    #[test]
    fn a_paragraph_that_leaves_the_item_closes_it() {
        // The other half of the same rule: after the blank line, a line at column zero is a new
        // paragraph outside the list, and indenting it into the item would change what it says.
        let found = wrap("  * an item\n\nBack to the body text.\nStill the body text.").join("\n");
        assert_eq!(
            found,
            "  * an item\n\nBack to the body text.\nStill the body text."
        );
    }
}
