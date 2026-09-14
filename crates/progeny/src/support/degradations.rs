//! The degradation report: where a lenient decode deviated from the description, and how.
//!
//! Shipped into generated crates verbatim and compiled here, like the decoder it serves.
//! Kept apart from the decoder because the client runtime carries it whichever way
//! responses are decoded: a strictly decoded response has an empty report, and the shape
//! of what a client returns does not change with the configuration.
//!
//! A report is **keyed by place in the description** rather than by place in the payload, so a
//! page of two hundred records with the same drift is one entry rather than two hundred, and the
//! entry names the pointer an override would use.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "shipped into generated crates; compiled here to keep its source checked"
    )
)]

use std::collections::BTreeMap;
use std::fmt;

/// Where in the description a degradation happened.
///
/// A generated type and one of its members, by the names the description uses: the Rust
/// type, the pointer it was generated from, and the member's wire name.
/// Equality is by these three, which is what folds every occurrence of one drift across a
/// payload into one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Site {
    /// The generated type — or, for a response body's root, the operation that received it.
    pub type_name: &'static str,
    /// Where the type is written in the description, as a JSON Pointer.
    ///
    /// One generated type has one origin. An inline schema written identically in more than one
    /// place is generated once, so a report about it names the place the type was generated
    /// from, which may be another operation's copy of the same schema.
    pub origin: &'static str,
    /// The member, by its wire name, or `None` for the type itself.
    pub member: Option<&'static str>,
}

impl Site {
    /// The JSON Pointer of the member's schema, which is what an override would be keyed by.
    #[must_use]
    pub fn pointer(&self) -> String {
        match self.member {
            Some(member) => {
                let escaped = member.replace('~', "~0").replace('/', "~1");
                format!("{}/properties/{escaped}", self.origin)
            }
            None => self.origin.to_owned(),
        }
    }
}

impl fmt::Display for Site {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.member {
            Some(member) => write!(formatter, "{}.{member}", self.type_name),
            None => formatter.write_str(self.type_name),
        }
    }
}

/// What a lenient decode tolerated at one site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DegradationKind {
    /// A member the description requires was not there.
    RequiredAbsent,
    /// A member was `null` where the description, overrides included, does not allow it.
    NullNotAllowed,
    /// A member was present and could not be read as the type the description declares.
    Undecodable,
    /// A string enum carried a value the description does not list.
    UnknownEnumValue,
    /// A union carried a payload whose tag names no variant, or that fits no variant's shape.
    UnknownUnionValue,
    /// An element of a list, or an entry of a map, could not be read and was left out.
    SkippedElement,
    /// A member the description does not declare.
    UndeclaredMember,
}

impl DegradationKind {
    /// The kind in words, for a report a person reads.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::RequiredAbsent => "a required member was absent",
            Self::NullNotAllowed => "null where the description does not allow it",
            Self::Undecodable => "a member could not be read as its declared type",
            Self::UnknownEnumValue => "an enum value the description does not list",
            Self::UnknownUnionValue => "a union payload that names no declared variant",
            Self::SkippedElement => "an element could not be read and was left out",
            Self::UndeclaredMember => "a member the description does not declare",
        }
    }

    /// Whether a strict decode would have refused the payload over this, for the kinds that
    /// say so on their own.
    ///
    /// An unlisted enum value or an unrecognized union payload is accepted by the strict
    /// decode too — the types are open under both — so they never count.
    /// A member that is absent or `null` counts only when the strict form reads it as a plain
    /// `T` rather than an `Option<T>`, and an undeclared member only when the strict type
    /// denies unknown members; the recorder knows both and says so when it records.
    fn strict_refuses(self) -> bool {
        match self {
            Self::Undecodable | Self::SkippedElement => true,
            Self::RequiredAbsent
            | Self::NullNotAllowed
            | Self::UnknownEnumValue
            | Self::UnknownUnionValue
            | Self::UndeclaredMember => false,
        }
    }
}

impl fmt::Display for DegradationKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.describe())
    }
}

/// How many times one kind of degradation happened at one site, with a few examples.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Tally {
    count: u32,
    samples: Vec<String>,
}

/// How many examples a tally keeps: enough to see the shape of a drift, few enough that a page
/// of a thousand distinct values costs nothing.
const SAMPLES: usize = 4;

impl Tally {
    /// Keep a sample, until enough distinct ones are kept.
    fn note(&mut self, sample: Option<String>) {
        let Some(sample) = sample else {
            return;
        };
        if self.samples.len() < SAMPLES && !self.samples.contains(&sample) {
            self.samples.push(sample);
        }
    }
}

/// Everything a lenient decode tolerated, aggregated by site and kind.
///
/// Empty for a payload that matched the description exactly.
/// Ordered by site, so a report reads in the description's order rather than the payload's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Degradations {
    entries: BTreeMap<(Site, DegradationKind), Tally>,
    /// Whether a strict decode would have refused the payload: the question an untagged union
    /// asks of each variant when it decides which one a payload is.
    strict_refuses: bool,
}

/// One line of a report: one kind of degradation at one site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Degradation<'a> {
    /// Where.
    pub site: &'a Site,
    /// What.
    pub kind: DegradationKind,
    /// How many times, across the whole payload.
    pub count: u32,
    /// A few of the values or messages involved, in arrival order.
    pub samples: &'a [String],
}

impl Degradations {
    /// An empty report.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the payload matched the description exactly.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many distinct site and kind pairs were recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// How many degradations were recorded in total, occurrences included.
    #[must_use]
    pub fn total(&self) -> u32 {
        self.entries.values().map(|tally| tally.count).sum()
    }

    /// Every entry, in site order.
    pub fn iter(&self) -> impl Iterator<Item = Degradation<'_>> {
        self.entries
            .iter()
            .map(|((site, kind), tally)| Degradation {
                site,
                kind: *kind,
                count: tally.count,
                samples: &tally.samples,
            })
    }

    /// The entries recorded at one site.
    pub fn at(&self, site: &Site) -> impl Iterator<Item = Degradation<'_>> {
        let site = *site;
        self.iter().filter(move |entry| *entry.site == site)
    }

    /// Whether anything was recorded at a site.
    #[must_use]
    pub fn touches(&self, site: &Site) -> bool {
        self.at(site).next().is_some()
    }

    /// Record one occurrence.
    ///
    /// `strict_refuses` says whether the strict decode of the same payload would have failed
    /// here, for the kinds where the recorder knows more than the kind does: an undeclared
    /// member is refused only by a type that denies them.
    #[doc(hidden)]
    pub fn record(
        &mut self,
        site: &Site,
        kind: DegradationKind,
        sample: Option<String>,
        strict_refuses: bool,
    ) {
        let tally = self.entries.entry((*site, kind)).or_default();
        tally.count = tally.count.saturating_add(1);
        tally.note(sample);
        self.strict_refuses |= strict_refuses || kind.strict_refuses();
    }

    /// Fold another report into this one.
    #[doc(hidden)]
    pub fn merge(&mut self, other: Self) {
        for ((site, kind), tally) in other.entries {
            let mine = self.entries.entry((site, kind)).or_default();
            mine.count = mine.count.saturating_add(tally.count);
            for sample in tally.samples {
                if mine.samples.len() < SAMPLES && !mine.samples.contains(&sample) {
                    mine.samples.push(sample);
                }
            }
        }
        self.strict_refuses |= other.strict_refuses;
    }

    /// Whether a strict decode of the payload this report describes would have failed.
    #[doc(hidden)]
    #[must_use]
    pub fn strict_refuses(&self) -> bool {
        self.strict_refuses
    }
}

impl fmt::Display for Degradations {
    /// One line per entry: the site, the kind, the count, a few samples, and the pointer.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, entry) in self.iter().enumerate() {
            if index > 0 {
                formatter.write_str("\n")?;
            }
            write!(
                formatter,
                "{}: {} ×{}",
                entry.site,
                entry.kind.describe(),
                entry.count
            )?;
            if !entry.samples.is_empty() {
                write!(formatter, ", e.g. {}", entry.samples.join(", "))?;
            }
            write!(formatter, " ({})", entry.site.pointer())?;
        }
        Ok(())
    }
}

/// A value read leniently, and what reading it tolerated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Decoded<T> {
    /// The value, as far as the payload allowed.
    pub value: T,
    /// What did not match the description on the way.
    pub degradations: Degradations,
}
