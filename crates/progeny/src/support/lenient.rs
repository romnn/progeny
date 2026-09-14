//! Lenient decoding: read a payload as far as it allows, and say what did not match.
//!
//! Shipped into generated crates verbatim, beside the buffered machinery it is built on, and
//! compiled and tested as part of progeny — the same source in both places.
//!
//! A response is whatever the vendor sent.
//! Under this decoder, a member that is absent, `null`, or unreadable becomes `None`; a
//! list element that cannot be read is skipped; an undeclared member is kept; an enum
//! value or a union payload the description does not know lands in its type's open variant.
//! None of that is silent: every tolerated deviation is recorded in a [`Degradations`]
//! report, keyed by its place in the description.
//!
//! What it does not do is conjure a value from nothing.
//! A root that is not what the description declares — an object where a list was
//! promised, a string where an object was — is an error, because there is no value to
//! hand back and no `None` to put it in.
//!
//! The decoder walks a [`Content`] tree the format already produced, which is what lets it be
//! generic over the format and lets a report be threaded through a whole payload where
//! [`serde::Deserialize`] could carry no context at all.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "shipped into generated crates; compiled here to keep its source checked"
    )
)]

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserialize, Deserializer, Error as _};

use super::degradations::{Decoded, DegradationKind, Degradations, Site};
use super::{Content, ContentDeserializer, choice};

/// The error the decoder reasons with: a message, and no position.
///
/// A buffered value has no position in the input to report, and the report keeps the message as
/// a sample rather than the error itself, so serde's own value error is exactly enough.
pub type Problem = de::value::Error;

/// What the description declares about a member's presence: which of absent and `null` it
/// allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Declared {
    /// Always there, never `null`.
    Required,
    /// May be absent; `null` is not allowed.
    Optional,
    /// Always there; may be `null`.
    Nullable,
    /// May be absent and may be `null`.
    OptionalNullable,
}

impl Declared {
    fn allows_absent(self) -> bool {
        matches!(self, Self::Optional | Self::OptionalNullable)
    }

    fn allows_null(self) -> bool {
        matches!(self, Self::Nullable | Self::OptionalNullable)
    }
}

/// The bound of [`Decoded::from_json`]: implemented by every type a response can yield and by
/// the containers of them.
///
/// Nothing here is for a consumer to call or implement. It is public because it is the bound on
/// the two ways to decode, and because the shipped client runtime names it from the other side of
/// a crate boundary under workspace packaging.
/// The contract the generated implementations keep:
///
/// - `Ok` is the value, as far as the payload allowed, with everything tolerated on the way
///   recorded in `report` at `site` or below.
/// - `Err` means no value could be produced at all — the content is not the shape the type is —
///   and the caller decides what that costs: a member becomes `None`, an element is skipped,
///   a root fails the decode.
pub trait Lenient<'de>: Sized {
    /// The root a standalone decode of this type is keyed at.
    ///
    /// A generated type names its own site; a container forwards its element type's, so a
    /// skipped element of a `Vec<Pet>` is reported at `Pet`'s site rather than at nothing; a
    /// scalar or a tuple names no place in the description and keeps the unnamed root.
    #[doc(hidden)]
    const ROOT: Site = Site::UNNAMED;

    /// Read the value out of buffered content.
    ///
    /// # Errors
    ///
    /// Returns the reason when the content is not the shape this type is; a struct handed
    /// a string, a list handed an object.
    /// Anything less is tolerated and reported.
    #[doc(hidden)]
    fn lenient(
        content: Content<'de>,
        site: Site,
        report: &mut Degradations,
    ) -> Result<Self, Problem>;
}

impl<T> Decoded<T> {
    /// One JSON document read leniently and held to the whole of it: a body that goes on after
    /// its value is refused, as `serde_json::from_slice` refuses it.
    ///
    /// The report is keyed at the type's own root — `read::Pet::SITE` for a generated type, and
    /// the element type's site for a container of them — so `report.root()` names something the
    /// generated crate spells.
    ///
    /// # Errors
    ///
    /// Returns `serde_json`'s own error when the input is not one well-formed JSON document,
    /// and a custom error when the root is not the shape `T` is.
    /// Nothing below the root can fail the decode.
    pub fn from_json(json: impl AsRef<[u8]>) -> serde_json::Result<Self>
    where
        T: for<'de> Lenient<'de>,
    {
        let mut deserializer = serde_json::Deserializer::from_slice(json.as_ref());
        let decoded = Self::from_deserializer(&mut deserializer)?;
        deserializer.end()?;
        Ok(decoded)
    }

    /// The same, from a deserializer the caller already has, and without the rule that the input
    /// ends there.
    ///
    /// # Errors
    ///
    /// Returns the format's own error when the input is not well-formed, and a custom error when
    /// the root is not the shape `T` is.
    pub fn from_deserializer<'de, D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
        T: Lenient<'de>,
    {
        Self::from_deserializer_at(T::ROOT, deserializer)
    }

    /// The same, keyed at a root the caller names: the operation and status a client's response
    /// arrived under, which no type in the description is.
    ///
    /// # Errors
    ///
    /// As [`Decoded::from_deserializer`].
    #[doc(hidden)]
    pub fn from_deserializer_at<'de, D>(root: Site, deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
        T: Lenient<'de>,
    {
        let content = Content::deserialize(deserializer)?;
        let mut degradations = Degradations::rooted(root);
        let value = T::lenient(content, root, &mut degradations).map_err(D::Error::custom)?;
        Ok(Self {
            value,
            degradations,
        })
    }
}

/// The members of one object, for a generated struct's lenient decode to take by name.
pub(crate) struct Members<'de> {
    members: Vec<(Content<'de>, Content<'de>)>,
}

/// What one declared member's slot held: nothing, `null`, or a value.
///
/// Visible to the presence-preserving member reader the support module adds beside its
/// `Presence` type, which is the one caller outside the decoder that has to tell all three apart.
#[doc(hidden)]
#[derive(Debug)]
pub(crate) enum Slot<'de> {
    Absent,
    Null,
    Value(Content<'de>),
}

impl<'de> Members<'de> {
    /// The members, or the reason the content is not an object.
    ///
    /// # Errors
    ///
    /// Returns the `invalid type` a strict decode would, naming the struct.
    pub(crate) fn of(content: Content<'de>, name: &'static str) -> Result<Self, Problem> {
        match content {
            Content::Map(members) => Ok(Self { members }),
            other => Err(de::Error::invalid_type(
                other.unexpected(),
                &StructName { name },
            )),
        }
    }

    /// Take one member's slot out, refusing a member written twice.
    ///
    /// Every copy of the name leaves the leftovers, however many there were, so a declared
    /// member can never surface again as an undeclared one; and the leftovers keep their
    /// arrival order, so a name written twice among them resolves to its last value the way
    /// a plain JSON object would.
    fn slot(&mut self, name: &str) -> Result<Slot<'de>, Problem> {
        let mut found = None;
        let mut copies = 0usize;
        let mut index = 0;
        while index < self.members.len() {
            let is_it = self
                .members
                .get(index)
                .is_some_and(|(key, _)| key.as_str() == Some(name));
            if !is_it {
                index += 1;
                continue;
            }
            let (_, value) = self.members.remove(index);
            copies += 1;
            if found.is_none() {
                found = Some(value);
            }
        }
        if copies > 1 {
            return Err(de::Error::duplicate_field("a member"));
        }
        Ok(match found {
            None => Slot::Absent,
            Some(Content::None | Content::Unit) => Slot::Null,
            Some(Content::Some(inner)) => Slot::Value(*inner),
            Some(value) => Slot::Value(value),
        })
    }

    /// Read one declared member, or `None` with the reason recorded.
    ///
    /// The member's own site names it, and is what the report keys the drift at.
    /// An absent member is `None`, reported when the description requires it; a `null` is
    /// `None`, reported when the description does not allow it; a value that does not
    /// read as `T` is `None`, reported with the reason.
    ///
    /// Only a member declared always-there-and-never-`null` is a plain `T` in the strict form;
    /// every other declaration is an `Option<T>` there, which reads both an absent member and a
    /// `null` as `None`.
    /// So absence and `null` are drift from the description either way, but they refuse a
    /// strict decode — and disqualify an untagged variant — only for that one declaration.
    pub(crate) fn take<T>(
        &mut self,
        site: Site,
        declared: Declared,
        report: &mut Degradations,
    ) -> Option<T>
    where
        T: Lenient<'de>,
    {
        let name = site.member().unwrap_or_default();
        let strict_refuses = declared == Declared::Required;
        match self.slot(name) {
            Ok(Slot::Absent) => {
                if !declared.allows_absent() {
                    report.record(site, DegradationKind::RequiredAbsent, None, strict_refuses);
                }
                None
            }
            Ok(Slot::Null) => {
                if !declared.allows_null() {
                    report.record(site, DegradationKind::NullNotAllowed, None, strict_refuses);
                }
                None
            }
            Ok(Slot::Value(content)) => match T::lenient(content, site, report) {
                Ok(value) => Some(value),
                Err(err) => {
                    report.record(
                        site,
                        DegradationKind::Undecodable,
                        Some(err.to_string()),
                        true,
                    );
                    None
                }
            },
            Err(err) => {
                report.record(
                    site,
                    DegradationKind::Undecodable,
                    Some(err.to_string()),
                    true,
                );
                None
            }
        }
    }

    /// The slot of a member whose type reads its own presence, for the presence-preserving
    /// member reader the support module adds beside its `Presence` type.
    ///
    /// A member written twice is reported and read as absent.
    #[doc(hidden)]
    pub(crate) fn take_raw(&mut self, site: Site, report: &mut Degradations) -> Slot<'de> {
        match self.slot(site.member().unwrap_or_default()) {
            Ok(slot) => slot,
            Err(err) => {
                report.record(
                    site,
                    DegradationKind::Undecodable,
                    Some(err.to_string()),
                    true,
                );
                Slot::Absent
            }
        }
    }

    /// The members no declared name claimed, as the struct's capture map.
    ///
    /// `declared` says whether the description itself declares such members
    /// (`additionalProperties`), in which case they arrive by contract and are not drift.
    /// `denied` says whether the strict form refuses them, which is what an untagged
    /// union needs to know when it asks whether a payload fits a variant.
    /// An entry that does not read as `T` is left out and reported as a skipped element.
    pub(crate) fn rest<M, T>(
        self,
        site: Site,
        report: &mut Degradations,
        declared: bool,
        denied: bool,
    ) -> M
    where
        M: FromIterator<(String, T)>,
        T: Lenient<'de>,
    {
        self.members
            .into_iter()
            .filter_map(|(key, value)| {
                let Some(name) = key.as_str().map(str::to_owned) else {
                    report.record(
                        site,
                        DegradationKind::SkippedElement,
                        Some("a member whose name is not a string".to_owned()),
                        true,
                    );
                    return None;
                };
                if !declared {
                    report.record(
                        site,
                        DegradationKind::UndeclaredMember,
                        Some(name.clone()),
                        denied,
                    );
                }
                match T::lenient(value, site, report) {
                    Ok(value) => Some((name, value)),
                    Err(err) => {
                        report.record(
                            site,
                            DegradationKind::SkippedElement,
                            Some(format!("`{name}`: {err}")),
                            true,
                        );
                        None
                    }
                }
            })
            .collect()
    }

    /// Report the members no declared name claimed, for a type that keeps none.
    pub(crate) fn report_rest(self, site: Site, report: &mut Degradations, denied: bool) {
        for (key, _) in self.members {
            report.record(
                site,
                DegradationKind::UndeclaredMember,
                key.as_str().map(str::to_owned),
                denied,
            );
        }
    }

    /// The content with one member removed: a consumed tag, before its variant reads the rest.
    #[doc(hidden)]
    #[must_use]
    pub(crate) fn without(mut self, name: &str) -> Content<'de> {
        self.members.retain(|(key, _)| key.as_str() != Some(name));
        Content::Map(self.members)
    }
}

/// What a struct expected, for an `invalid type` message.
struct StructName {
    name: &'static str,
}

impl de::Expected for StructName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "struct {}", self.name)
    }
}

/// Try one variant of an untagged union: the value when the payload fits it strictly.
///
/// A union told apart by shape needs the strict reading of each variant, because under
/// the lenient one a struct with every member optional fits every object.
/// So each variant is read with a scratch report and accepted only when nothing in that
/// report would have refused a strict decode; the scratch report then joins the real one,
/// so what the accepted variant tolerated — an undeclared member, an unlisted enum
/// value — is still said.
#[doc(hidden)]
pub(crate) fn probe<'de, T>(
    content: &Content<'de>,
    site: Site,
    report: &mut Degradations,
) -> Option<T>
where
    T: Lenient<'de>,
{
    let mut scratch = Degradations::new();
    match T::lenient(content.clone(), site, &mut scratch) {
        Ok(value) if !scratch.strict_refuses() => {
            report.merge(scratch);
            Some(value)
        }
        Ok(_) | Err(_) => None,
    }
}

/// A union payload no variant claims, as arbitrary JSON, with the fact recorded.
///
/// # Errors
///
/// Never in practice: arbitrary JSON reads every buffered value.
/// The type is `Result` so the generated arm reads like its siblings.
#[doc(hidden)]
pub(crate) fn unknown_union<'de>(
    content: Content<'de>,
    site: Site,
    report: &mut Degradations,
    tag: Option<&str>,
) -> Result<serde_json::Value, Problem> {
    report.record(
        site,
        DegradationKind::UnknownUnionValue,
        tag.map(str::to_owned),
        false,
    );
    Deserialize::deserialize(ContentDeserializer::<'de, Problem>::new(content))
}

/// Which variant a tagged union's payload names, by position in `variants`, or `None`.
///
/// # Errors
///
/// Returns the duplicate-member error when the payload writes the tag twice.
#[doc(hidden)]
pub(crate) fn tag_choice(
    content: &Content<'_>,
    tag: &'static str,
    variants: &'static [&'static str],
) -> Result<Option<usize>, Problem> {
    choice(content, tag, variants)
}

/// The tag member's text, for the report's sample when it names no variant.
#[doc(hidden)]
#[must_use]
pub(crate) fn tag_text<'a>(content: &'a Content<'_>, tag: &str) -> Option<&'a str> {
    let Content::Map(members) = content else {
        return None;
    };
    members
        .iter()
        .find(|(key, _)| key.as_str() == Some(tag))
        .and_then(|(_, value)| value.as_str())
}

/// Read a string enum leniently: a value the description does not list is recorded.
///
/// # Errors
///
/// Returns the reason when the content is not a string.
#[doc(hidden)]
pub(crate) fn open_string<'de, T>(
    content: Content<'de>,
    site: Site,
    report: &mut Degradations,
    unlisted: fn(&T) -> Option<&str>,
) -> Result<T, Problem>
where
    T: Deserialize<'de>,
{
    let value: T = Deserialize::deserialize(ContentDeserializer::<'de, Problem>::new(content))?;
    if let Some(raw) = unlisted(&value) {
        report.record(
            site,
            DegradationKind::UnknownEnumValue,
            Some(raw.to_owned()),
            false,
        );
    }
    Ok(value)
}

impl<'de, T> Lenient<'de> for Option<T>
where
    T: Lenient<'de>,
{
    const ROOT: Site = T::ROOT;

    fn lenient(
        content: Content<'de>,
        site: Site,
        report: &mut Degradations,
    ) -> Result<Self, Problem> {
        match content {
            Content::None | Content::Unit => Ok(None),
            Content::Some(inner) => T::lenient(*inner, site, report).map(Some),
            other => T::lenient(other, site, report).map(Some),
        }
    }
}

impl<'de, T> Lenient<'de> for Box<T>
where
    T: Lenient<'de>,
{
    const ROOT: Site = T::ROOT;

    fn lenient(
        content: Content<'de>,
        site: Site,
        report: &mut Degradations,
    ) -> Result<Self, Problem> {
        T::lenient(content, site, report).map(Box::new)
    }
}

/// The elements of a buffered sequence, or the reason the content is not one.
fn elements<'de>(
    content: Content<'de>,
    expected: &'static str,
) -> Result<Vec<Content<'de>>, Problem> {
    match content {
        Content::Seq(items) => Ok(items),
        other => Err(de::Error::invalid_type(other.unexpected(), &expected)),
    }
}

impl<'de, T> Lenient<'de> for Vec<T>
where
    T: Lenient<'de>,
{
    const ROOT: Site = T::ROOT;

    /// Every element that reads; the rest are left out and reported, so one bad record does
    /// not fail the list that holds it.
    fn lenient(
        content: Content<'de>,
        site: Site,
        report: &mut Degradations,
    ) -> Result<Self, Problem> {
        let items = elements(content, "a sequence")?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            match T::lenient(item, site, report) {
                Ok(value) => out.push(value),
                Err(err) => report.record(
                    site,
                    DegradationKind::SkippedElement,
                    Some(err.to_string()),
                    true,
                ),
            }
        }
        Ok(out)
    }
}

/// The entries of a buffered map keyed by strings, with the unreadable ones reported.
fn entries<'de, T>(
    content: Content<'de>,
    site: Site,
    report: &mut Degradations,
) -> Result<Vec<(String, T)>, Problem>
where
    T: Lenient<'de>,
{
    let Content::Map(members) = content else {
        return Err(de::Error::invalid_type(content.unexpected(), &"a map"));
    };
    let mut out = Vec::with_capacity(members.len());
    for (key, value) in members {
        let Some(name) = key.as_str().map(str::to_owned) else {
            report.record(
                site,
                DegradationKind::SkippedElement,
                Some("an entry whose key is not a string".to_owned()),
                true,
            );
            continue;
        };
        match T::lenient(value, site, report) {
            Ok(value) => out.push((name, value)),
            Err(err) => report.record(
                site,
                DegradationKind::SkippedElement,
                Some(format!("`{name}`: {err}")),
                true,
            ),
        }
    }
    Ok(out)
}

impl<'de, T> Lenient<'de> for BTreeMap<String, T>
where
    T: Lenient<'de>,
{
    const ROOT: Site = T::ROOT;

    fn lenient(
        content: Content<'de>,
        site: Site,
        report: &mut Degradations,
    ) -> Result<Self, Problem> {
        entries(content, site, report).map(|pairs| pairs.into_iter().collect())
    }
}

impl<'de, T, const N: usize> Lenient<'de> for [T; N]
where
    T: Lenient<'de>,
{
    const ROOT: Site = T::ROOT;

    /// All or nothing: a fixed arity has no element to leave out.
    fn lenient(
        content: Content<'de>,
        site: Site,
        report: &mut Degradations,
    ) -> Result<Self, Problem> {
        let items = elements(content, "a sequence")?;
        let count = items.len();
        let read = items
            .into_iter()
            .map(|item| T::lenient(item, site, report))
            .collect::<Result<Vec<T>, Problem>>()?;
        read.try_into()
            .map_err(|_| de::Error::invalid_length(count, &Arity { count: N }))
    }
}

/// What a fixed-arity sequence expected, for an `invalid length` message.
struct Arity {
    count: usize,
}

impl de::Expected for Arity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "a sequence of {} elements", self.count)
    }
}

/// Tuples read all or nothing, like fixed arrays, up to the arity serde itself supports.
macro_rules! lenient_tuples {
    ($($count:literal => ($($name:ident),+)),+ $(,)?) => {
        $(
            impl<'de, $($name),+> Lenient<'de> for ($($name,)+)
            where
                $($name: Lenient<'de>,)+
            {
                fn lenient(
                    content: Content<'de>,
                    site: Site,
                    report: &mut Degradations,
                ) -> Result<Self, Problem> {
                    let items = elements(content, "a sequence")?;
                    if items.len() != $count {
                        return Err(de::Error::invalid_length(
                            items.len(),
                            &Arity { count: $count },
                        ));
                    }
                    let mut items = items.into_iter();
                    Ok(($(
                        match items.next() {
                            Some(item) => $name::lenient(item, site, report)?,
                            None => return Err(de::Error::invalid_length(0, &Arity { count: $count })),
                        },
                    )+))
                }
            }
        )+
    };
}

lenient_tuples! {
    1 => (A),
    2 => (A, B),
    3 => (A, B, C),
    4 => (A, B, C, D),
    5 => (A, B, C, D, E),
    6 => (A, B, C, D, E, F),
    7 => (A, B, C, D, E, F, G),
    8 => (A, B, C, D, E, F, G, H),
    9 => (A, B, C, D, E, F, G, H, I),
    10 => (A, B, C, D, E, F, G, H, I, J),
    11 => (A, B, C, D, E, F, G, H, I, J, K),
    12 => (A, B, C, D, E, F, G, H, I, J, K, L),
    13 => (A, B, C, D, E, F, G, H, I, J, K, L, M),
    14 => (A, B, C, D, E, F, G, H, I, J, K, L, M, N),
    15 => (A, B, C, D, E, F, G, H, I, J, K, L, M, N, O),
    16 => (A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P),
}

/// A leaf: a type with nothing inside it to tolerate, read through its own `Deserialize`.
///
/// Used for the scalars here and, from the generated support module, for the format crates a
/// configuration chose, which this file cannot name unconditionally.
macro_rules! lenient_via_serde {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl<'de> Lenient<'de> for $ty {
                fn lenient(
                    content: Content<'de>,
                    _site: Site,
                    _report: &mut Degradations,
                ) -> Result<Self, Problem> {
                    Deserialize::deserialize(ContentDeserializer::<'de, Problem>::new(content))
                }
            }
        )+
    };
}

lenient_via_serde! {
    (),
    bool,
    i64,
    u64,
    f64,
    String,
    serde_json::Value,
    std::net::IpAddr,
    std::net::Ipv4Addr,
    std::net::Ipv6Addr,
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::{self, OptionExt as _};

    use super::super::degradations::{Decoded, Degradation};
    use super::{Declared, DegradationKind, Degradations, Lenient, Members, Site, Slot};
    use crate::support::buffered::Content;

    /// A struct written the way the renderer writes a read form: two declared members and a
    /// capture map, each member taken by the constant that names its site.
    #[derive(Debug, Default, PartialEq)]
    struct Employment {
        kind: Option<String>,
        years: Option<i64>,
        extra: std::collections::BTreeMap<String, serde_json::Value>,
    }

    impl Employment {
        const SITE: Site = Site::new("Employment", "/components/schemas/Employment", None);
        const SITE_KIND: Site =
            Site::new("Employment", "/components/schemas/Employment", Some("type"));
        const SITE_YEARS: Site = Site::new(
            "Employment",
            "/components/schemas/Employment",
            Some("years"),
        );
    }

    impl<'de> Lenient<'de> for Employment {
        const ROOT: Site = Self::SITE;

        fn lenient(
            content: Content<'de>,
            _site: Site,
            report: &mut Degradations,
        ) -> Result<Self, super::Problem> {
            let mut members = Members::of(content, "Employment")?;
            Ok(Self {
                kind: members.take(Self::SITE_KIND, Declared::Required, report),
                years: members.take(Self::SITE_YEARS, Declared::Optional, report),
                extra: members.rest(Self::SITE, report, false, false),
            })
        }
    }

    /// The root a client hands its decode: an operation's response, which is no type's site.
    const ROOT: Site = Site::new("list", "/paths/~1list/get/responses/200", None);

    fn decode<'de, T: Lenient<'de>>(json: &'de str) -> eyre::Result<(T, Degradations)> {
        let mut deserializer = serde_json::Deserializer::from_str(json);
        let decoded = Decoded::<T>::from_deserializer_at(ROOT, &mut deserializer)?;
        Ok((decoded.value, decoded.degradations))
    }

    fn only(report: &Degradations, site: Site) -> eyre::Result<Degradation<'_>> {
        let mut at = report.at(site);
        let first = at.next().ok_or_eyre("something was recorded at the site")?;
        assert!(
            at.next().is_none(),
            "more than one kind at {site}: {report}"
        );
        Ok(first)
    }

    /// A payload that matches the description exactly reports nothing.
    #[test_util::test]
    fn an_exact_payload_reports_nothing() {
        let (value, report) = decode::<Employment>(r#"{"type":"salaried","years":3}"#)?;
        assert_eq!(value.kind.as_deref(), Some("salaried"));
        assert_eq!(value.years, Some(3));
        assert!(report.is_empty(), "{report}");
    }

    /// The Hypofy case: a required member is `null`, the value decodes with `None` there, and
    /// the report names the member and the kind of drift.
    #[test_util::test]
    fn a_required_null_becomes_none_and_is_reported_at_its_site() {
        let (value, report) = decode::<Employment>(r#"{"type":null}"#)?;
        assert_eq!(value.kind, None);
        let entry = only(&report, Employment::SITE_KIND)?;
        assert_eq!(entry.kind, DegradationKind::NullNotAllowed);
        assert_eq!(entry.count, 1);
        assert!(report.strict_refuses());
    }

    #[test_util::test]
    fn an_absent_required_member_is_reported_and_an_absent_optional_one_is_not() {
        let (_, report) = decode::<Employment>("{}")?;
        assert_eq!(only(&report, Employment::SITE_KIND)?.kind, DegradationKind::RequiredAbsent);
        assert!(!report.touches(Employment::SITE_YEARS), "{report}");
    }

    /// A member written more than once is refused as one, however many copies there were:
    /// none of them is captured as an undeclared member, and the leftovers keep their arrival
    /// order, so a name written twice among them resolves to its last value the way a plain
    /// JSON object would.
    #[test_util::test]
    fn a_member_written_more_than_once_is_refused_as_one_and_never_captured() {
        let (value, report) =
            decode::<Employment>(r#"{"type":"a","type":"b","type":"c","x":1,"x":2}"#)?;
        assert_eq!(value.kind, None);
        assert!(!value.extra.contains_key("type"), "{:?}", value.extra);
        assert_eq!(value.extra.get("x"), Some(&serde_json::json!(2)));
        assert_eq!(only(&report, Employment::SITE_KIND)?.kind, DegradationKind::Undecodable);
        assert_eq!(
            only(&report, Employment::SITE)?.kind,
            DegradationKind::UndeclaredMember
        );
    }

    /// Absence and `null` are drift wherever the description does not allow them, but they
    /// refuse a strict decode only where the strict form reads a plain `T`: an `Option<T>`
    /// there reads both as `None`, so an untagged union must not move past such a variant.
    #[test_util::test]
    fn only_a_plain_member_refuses_absence_or_null_strictly() {
        let (value, report) = decode::<Employment>(r#"{"type":"a","years":null}"#)?;
        assert_eq!(value.years, None);
        assert_eq!(only(&report, Employment::SITE_YEARS)?.kind, DegradationKind::NullNotAllowed);
        assert!(!report.strict_refuses(), "{report}");
        let (_, report) = decode::<Address>(r#"{"street":"Main"}"#)?;
        assert_eq!(only(&report, Address::SITE_UNIT)?.kind, DegradationKind::RequiredAbsent);
        assert!(!report.strict_refuses(), "{report}");
        let (_, report) = decode::<Address>(r#"{"unit":null}"#)?;
        assert_eq!(only(&report, Address::SITE_STREET)?.kind, DegradationKind::RequiredAbsent);
        assert!(report.strict_refuses(), "{report}");
    }

    #[test_util::test]
    fn a_member_of_the_wrong_type_becomes_none_with_the_reason_as_a_sample() {
        let (value, report) = decode::<Employment>(r#"{"type":"x","years":"three"}"#)?;
        assert_eq!(value.years, None);
        let entry = only(&report, Employment::SITE_YEARS)?;
        assert_eq!(entry.kind, DegradationKind::Undecodable);
        assert!(
            entry.samples[0].contains("invalid type"),
            "{:?}",
            entry.samples
        );
    }

    /// Undeclared members are kept, written back, and reported by name.
    #[test_util::test]
    fn an_undeclared_member_is_captured_and_reported() {
        let (value, report) = decode::<Employment>(r#"{"type":"x","employer":"ACME"}"#)?;
        assert_eq!(value.extra["employer"], serde_json::json!("ACME"));
        let entry = only(&report, Employment::SITE)?;
        assert_eq!(entry.kind, DegradationKind::UndeclaredMember);
        assert_eq!(entry.samples, ["employer"]);
        // Not a strict refusal: the type does not deny unknown members.
        assert!(!report.strict_refuses());
    }

    /// A list element that cannot be read is left out, and the rest survive.
    #[test_util::test]
    fn a_bad_list_element_is_skipped_and_counted_while_the_rest_decode() {
        let (value, report) =
            decode::<Vec<Employment>>(r#"[{"type":"a"},"not an object",{"type":"b"},7]"#)?;
        assert_eq!(value.len(), 2);
        assert_eq!(value[1].kind.as_deref(), Some("b"));
        let entry = only(&report, ROOT)?;
        assert_eq!(entry.kind, DegradationKind::SkippedElement);
        assert_eq!(entry.count, 2);
    }

    /// A payload decoded on its own is keyed at the type it is, and a container forwards its
    /// element type's root — so an element that could not be read is reported at the element
    /// type's site rather than at nothing.
    #[test_util::test]
    fn a_standalone_decode_is_keyed_at_the_type_the_payload_is() {
        let decoded = Decoded::<Employment>::from_json(r#"{"type":null}"#)?;
        assert_eq!(decoded.degradations.root(), Employment::SITE);
        assert!(decoded.is_degraded());

        let decoded = Decoded::<Vec<Employment>>::from_json(r#"[{"type":"a"},"not an object"]"#)?;
        assert_eq!(decoded.degradations.root(), Employment::SITE);
        assert_eq!(decoded.value.len(), 1);
        assert_eq!(
            only(&decoded.degradations, Employment::SITE)?.kind,
            DegradationKind::SkippedElement
        );

        // A tuple names no place in the description, so it keeps the unnamed root.
        let decoded = Decoded::<(i64, String)>::from_json(r#"[1,"a"]"#)?;
        assert_eq!(decoded.degradations.root(), Site::UNNAMED);
        assert!(!decoded.is_degraded());

        // Held to the whole document, the way `serde_json::from_slice` holds a strict decode.
        assert!(Decoded::<Vec<Employment>>::from_json("[] garbage").is_err());
    }

    /// A member's constant is the site the decoder records its drift at, and the type's own
    /// constant is not: `within` sees the type and its members, `at` sees exactly one place.
    #[test_util::test]
    fn a_member_constant_is_the_site_the_decoder_records_at() {
        let (_, report) = decode::<Employment>(r#"{"type":null,"floor":3}"#)?;
        assert_eq!(
            only(&report, Employment::SITE_KIND)?.kind,
            DegradationKind::NullNotAllowed
        );
        // Ordered by site, and a site with no member sorts before one with a member.
        let within: Vec<DegradationKind> = report
            .within(Employment::SITE)
            .map(|entry| entry.kind)
            .collect();
        assert_eq!(
            within,
            [
                DegradationKind::UndeclaredMember,
                DegradationKind::NullNotAllowed
            ]
        );
        // The type's own site holds only what the type itself tolerated.
        let at: Vec<DegradationKind> = report.at(Employment::SITE).map(|entry| entry.kind).collect();
        assert_eq!(at, [DegradationKind::UndeclaredMember]);
        // And asking `within` with a member's site answers about the type that declares it.
        assert_eq!(report.within(Employment::SITE_KIND).count(), 2);
    }

    /// The aggregation the report exists for: two hundred records with one drift is one line.
    #[test_util::test]
    fn the_same_drift_across_a_page_is_one_entry_with_a_count() {
        let page: Vec<String> = (0..200).map(|_| r#"{"type":null}"#.to_owned()).collect();
        let json = format!("[{}]", page.join(","));
        let (value, report) = decode::<Vec<Employment>>(&json)?;
        assert_eq!(value.len(), 200);
        assert_eq!(report.len(), 1);
        assert_eq!(only(&report, Employment::SITE_KIND)?.count, 200);
        assert_eq!(report.total(), 200);
        let rendered = report.to_string();
        assert!(rendered.contains("Employment.type"), "{rendered}");
        assert!(rendered.contains("×200"), "{rendered}");
        assert!(
            rendered.contains("/components/schemas/Employment/properties/type"),
            "{rendered}"
        );
    }

    /// Leniency cannot conjure a value: a root of the wrong shape is an error, not a `None`.
    #[test_util::test]
    fn a_root_of_the_wrong_shape_is_an_error() {
        let refused = decode::<Employment>("[]");
        let err = refused.err().ok_or_eyre("an array is not a struct")?;
        assert!(err.to_string().contains("struct Employment"), "{err}");
        let refused = decode::<Vec<Employment>>("{}");
        assert!(refused.is_err());
    }

    /// A member written twice has said two things; neither is taken.
    #[test_util::test]
    fn a_duplicate_member_is_reported_rather_than_last_write_wins() {
        let (value, report) = decode::<Employment>(r#"{"type":"a","type":"b"}"#)?;
        assert_eq!(value.kind, None);
        assert_eq!(only(&report, Employment::SITE_KIND)?.kind, DegradationKind::Undecodable);
    }

    /// Samples are capped, counts are not.
    #[test_util::test]
    fn samples_are_capped_and_deduplicated() {
        let mut report = Degradations::new();
        for value in ["a", "b", "a", "c", "d", "e", "f"] {
            report.record(
                Employment::SITE_KIND,
                DegradationKind::UnknownEnumValue,
                Some(value.to_owned()),
                false,
            );
        }
        let entry = only(&report, Employment::SITE_KIND)?;
        assert_eq!(entry.count, 7);
        assert_eq!(entry.samples, ["a", "b", "c", "d"]);
        assert!(!report.strict_refuses());
    }

    #[test_util::test]
    fn a_site_pointer_escapes_the_member_name_the_way_json_pointers_do() {
        let site = Site::new("T", "/components/schemas/T", Some("a/b~c"));
        assert_eq!(site.pointer(), "/components/schemas/T/properties/a~1b~0c");
        assert_eq!(site.to_string(), "T.a/b~c");
        assert_eq!(site.type_name(), "T");
        assert_eq!(site.origin(), "/components/schemas/T");
        assert_eq!(site.member(), Some("a/b~c"));
        // The type's own site is the origin itself, which is what an override is keyed by.
        assert_eq!(
            Site::new("T", "/components/schemas/T", None).pointer(),
            "/components/schemas/T"
        );
    }

    /// Fixed arities are all or nothing, and a nullable element is a `None` element.
    #[test_util::test]
    fn containers_read_the_way_their_shapes_say() {
        let (pair, report) = decode::<(i64, String)>(r#"[1,"a"]"#)?;
        assert_eq!(pair, (1, "a".to_owned()));
        assert!(report.is_empty());
        assert!(decode::<(i64, String)>(r#"[1,"a",2]"#).is_err());
        assert!(decode::<[i64; 2]>("[1]").is_err());
        let (items, _) = decode::<Vec<Option<i64>>>("[1,null,3]")?;
        assert_eq!(items, [Some(1), None, Some(3)]);
        let (map, report) =
            decode::<std::collections::BTreeMap<String, i64>>(r#"{"a":1,"b":"two"}"#)?;
        assert_eq!(map.len(), 1);
        assert_eq!(only(&report, ROOT)?.kind, DegradationKind::SkippedElement);
    }

    /// A shared type, written the way the renderer writes one: no capture map, so what the
    /// description did not declare is reported and dropped; a nullable member and one that may
    /// be absent or null take `null` without a word.
    #[derive(Debug, PartialEq)]
    struct Address {
        street: Option<String>,
        unit: Option<String>,
        note: Option<String>,
    }

    impl Address {
        const SITE: Site = Site::new("Address", "/components/schemas/Address", None);
        const SITE_STREET: Site =
            Site::new("Address", "/components/schemas/Address", Some("street"));
        const SITE_UNIT: Site = Site::new("Address", "/components/schemas/Address", Some("unit"));
        const SITE_NOTE: Site = Site::new("Address", "/components/schemas/Address", Some("note"));
    }

    impl<'de> Lenient<'de> for Address {
        const ROOT: Site = Self::SITE;

        fn lenient(
            content: Content<'de>,
            _site: Site,
            report: &mut Degradations,
        ) -> Result<Self, super::Problem> {
            let mut members = Members::of(content, "Address")?;
            let value = Self {
                street: members.take(Self::SITE_STREET, Declared::Required, report),
                unit: members.take(Self::SITE_UNIT, Declared::Nullable, report),
                note: members.take(Self::SITE_NOTE, Declared::OptionalNullable, report),
            };
            members.report_rest(Self::SITE, report, true);
            Ok(value)
        }
    }

    /// A type without a capture map reports what it drops, and a member declared nullable takes
    /// `null` without a word.
    #[test_util::test]
    fn a_type_that_keeps_nothing_undeclared_still_says_what_it_dropped() {
        let (value, report) =
            decode::<Address>(r#"{"street":"Main","unit":null,"note":null,"floor":3}"#)?;
        assert_eq!(value.street.as_deref(), Some("Main"));
        assert_eq!(value.unit, None);
        assert_eq!(value.note, None);
        let entry = only(&report, Address::SITE)?;
        assert_eq!(entry.kind, DegradationKind::UndeclaredMember);
        assert_eq!(entry.samples, ["floor"]);
        // Denied by the strict form, so a union probing this variant would move on.
        assert!(report.strict_refuses());
        assert!(!report.touches(Address::SITE_UNIT) && !report.touches(Address::SITE_NOTE), "{report}");
    }

    /// A string enum with an open arm, read through `open_string` the way the renderer writes
    /// its decoder.
    #[derive(Debug, PartialEq, serde::Deserialize)]
    #[serde(rename_all = "lowercase")]
    enum Status {
        Active,
        Retired,
        #[serde(untagged)]
        Unknown(String),
    }

    impl Status {
        const SITE: Site = Site::new("Status", "/components/schemas/Status", None);
    }

    impl<'de> Lenient<'de> for Status {
        const ROOT: Site = Self::SITE;

        fn lenient(
            content: Content<'de>,
            site: Site,
            report: &mut Degradations,
        ) -> Result<Self, super::Problem> {
            super::open_string(content, site, report, |value| match value {
                Self::Unknown(raw) => Some(raw.as_str()),
                Self::Active | Self::Retired => None,
            })
        }
    }

    /// An unlisted enum value lands in the open arm, keeps its bytes, and is reported with the
    /// value as the sample; a non-string is still refused.
    #[test_util::test]
    fn an_unlisted_enum_value_is_kept_and_reported() {
        let (listed, report) = decode::<Status>(r#""active""#)?;
        assert_eq!(listed, Status::Active);
        assert!(report.is_empty());
        let (unlisted, report) = decode::<Status>(r#""suspended""#)?;
        assert_eq!(unlisted, Status::Unknown("suspended".to_owned()));
        let entry = only(&report, ROOT)?;
        assert_eq!(entry.kind, DegradationKind::UnknownEnumValue);
        assert_eq!(entry.samples, ["suspended"]);
        // Open, not lenient: a strict decode reads the same value.
        assert!(!report.strict_refuses());
        assert!(decode::<Status>("7").is_err());
    }

    /// An untagged union of two read forms, told apart by what fits strictly.
    #[derive(Debug, PartialEq)]
    enum Contact {
        Employment(Employment),
        Address(Address),
        Unknown(Box<serde_json::Value>),
    }

    impl<'de> Lenient<'de> for Contact {
        fn lenient(
            content: Content<'de>,
            site: Site,
            report: &mut Degradations,
        ) -> Result<Self, super::Problem> {
            if let Some(value) = super::probe::<Employment>(&content, site, report) {
                return Ok(Self::Employment(value));
            }
            if let Some(value) = super::probe::<Address>(&content, site, report) {
                return Ok(Self::Address(value));
            }
            super::unknown_union(content, site, report, None)
                .map(|value| Self::Unknown(Box::new(value)))
        }
    }

    /// The first variant a payload fits strictly wins, even though under the lenient reading
    /// every object fits every struct; what the winner tolerated is still reported.
    #[test_util::test]
    fn an_untagged_union_picks_the_variant_a_strict_decode_would() {
        let (value, report) = decode::<Contact>(r#"{"street":"Main","unit":null}"#)?;
        assert_eq!(
            value,
            Contact::Address(Address {
                street: Some("Main".to_owned()),
                unit: None,
                note: None,
            })
        );
        assert!(report.is_empty(), "{report}");
        // The employment variant captures undeclared members and is not denied by them, so it
        // claims this payload and reports what it kept.
        let (value, report) = decode::<Contact>(r#"{"type":"salaried","floor":3}"#)?;
        assert!(matches!(value, Contact::Employment(_)), "{value:?}");
        assert_eq!(
            only(&report, Employment::SITE)?.kind,
            DegradationKind::UndeclaredMember
        );
        // Nothing fits: the payload is kept whole in the open arm and the union's site says so.
        let (value, report) = decode::<Contact>(r#"{"years":"many"}"#)?;
        assert_eq!(
            value,
            Contact::Unknown(Box::new(serde_json::json!({"years":"many"})))
        );
        assert_eq!(
            only(&report, ROOT)?.kind,
            DegradationKind::UnknownUnionValue
        );
    }

    /// A `null` in an optional member is drift, but the strict `Option` reads it as `None`, so
    /// the variant strict decoding would pick is still the one picked — with the drift reported.
    #[test_util::test]
    fn an_untagged_union_does_not_move_past_a_variant_over_a_null_optional_member() {
        let (value, report) =
            decode::<Contact>(r#"{"type":"salaried","years":null,"street":"Main"}"#)?;
        assert!(matches!(value, Contact::Employment(_)), "{value:?}");
        assert_eq!(only(&report, Employment::SITE_YEARS)?.kind, DegradationKind::NullNotAllowed);
        assert_eq!(
            only(&report, Employment::SITE)?.kind,
            DegradationKind::UndeclaredMember
        );
    }

    /// A tagged union whose tag the union consumes, written the way the renderer writes one.
    #[derive(Debug, PartialEq)]
    enum Event {
        Hired(Employment),
        Moved(Address),
        Unknown(Box<serde_json::Value>),
    }

    impl<'de> Lenient<'de> for Event {
        fn lenient(
            content: Content<'de>,
            site: Site,
            report: &mut Degradations,
        ) -> Result<Self, super::Problem> {
            const TAG: &str = "kind";
            const VARIANTS: &[&str] = &["hired", "moved"];
            match super::tag_choice(&content, TAG, VARIANTS)? {
                Some(0) => {
                    Employment::lenient(Members::of(content, "Event")?.without(TAG), site, report)
                        .map(Self::Hired)
                }
                Some(1) => {
                    Address::lenient(Members::of(content, "Event")?.without(TAG), site, report)
                        .map(Self::Moved)
                }
                _ => {
                    let tag = super::tag_text(&content, TAG).map(str::to_owned);
                    super::unknown_union(content, site, report, tag.as_deref())
                        .map(|value| Self::Unknown(Box::new(value)))
                }
            }
        }
    }

    /// The tag picks the variant and is taken off before the variant reads, so it is not an
    /// undeclared member; a tag naming no variant keeps the payload and is the report's sample.
    #[test_util::test]
    fn a_tagged_union_consumes_its_tag_and_keeps_an_unrecognized_payload() {
        let (value, report) = decode::<Event>(r#"{"kind":"hired","type":"salaried"}"#)?;
        assert!(matches!(value, Event::Hired(_)), "{value:?}");
        assert!(report.is_empty(), "{report}");
        let (value, report) = decode::<Event>(r#"{"kind":"fired","when":"now"}"#)?;
        assert_eq!(
            value,
            Event::Unknown(Box::new(serde_json::json!({"kind":"fired","when":"now"})))
        );
        let entry = only(&report, ROOT)?;
        assert_eq!(entry.kind, DegradationKind::UnknownUnionValue);
        assert_eq!(entry.samples, ["fired"]);
        // A tag written twice is the one thing the choice refuses outright.
        assert!(decode::<Event>(r#"{"kind":"hired","kind":"moved"}"#).is_err());
    }

    /// The raw slot reader the presence-preserving member reader is built on tells absent,
    /// `null` and a value apart, and reports a duplicate.
    #[test_util::test]
    fn the_raw_slot_reader_distinguishes_absent_null_and_present() {
        let mut report = Degradations::new();
        let content: Content<'_> = serde_json::from_str(r#"{"years":null,"type":"x"}"#)?;
        let mut members = Members::of(content, "Employment")?;
        assert!(matches!(members.take_raw(Employment::SITE_YEARS, &mut report), Slot::Null));
        assert!(matches!(
            members.take_raw(Employment::SITE_KIND, &mut report),
            Slot::Value(_)
        ));
        assert!(matches!(members.take_raw(Employment::SITE_KIND, &mut report), Slot::Absent));
        assert!(report.is_empty());
        let content: Content<'_> = serde_json::from_str(r#"{"years":1,"years":2}"#)?;
        let mut members = Members::of(content, "Employment")?;
        assert!(matches!(members.take_raw(Employment::SITE_YEARS, &mut report), Slot::Absent));
        assert_eq!(only(&report, Employment::SITE_YEARS)?.kind, DegradationKind::Undecodable);
    }
}
