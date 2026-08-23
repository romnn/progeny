//! Runtime support for the hand-written `Deserialize` implementations.
//!
//! This file is shipped into generated crates verbatim, and it is also compiled and unit-tested as
//! part of progeny itself — the same source in both places, so a change cannot pass the tests here
//! and break there.
//!
//! The machinery is monomorphic and lives once per generated crate; the per-type code is two thin
//! function bodies that call into it. That ratio is the whole point: the serde derive expands to
//! about nine bodies per struct, and the cost of compiling a generated crate is dominated by
//! function bodies rather than by type count.
//!
//! Three behaviours here are not conveniences but requirements, each one a bug the predecessor had:
//!
//! * **Members are held in an association list, never a map.** A map would silently keep the last
//!   of two members with the same name; the serde derive rejects that, so this has to as well.
//! * **A bare `Option<T>` accepts an absent member**, with no `default` attribute. serde routes a
//!   missing member through a deserializer whose `deserialize_option` answers `visit_none`, and
//!   [`Missing`] mirrors that exactly. Getting it wrong would reject valid responses.
//! * **Replaying a buffered member reports the same error the format would have.** The error type is
//!   the format's own, so `invalid type` and `unknown variant` read the way they always do.

#![expect(
    dead_code,
    reason = "this file is shipped into generated crates; inside progeny it exists to be compiled and tested"
)]

use std::fmt;

use serde::de::{
    self, Deserialize, DeserializeSeed, Deserializer, EnumAccess, IntoDeserializer, MapAccess,
    SeqAccess, VariantAccess, Visitor,
};

/// One buffered value, in whatever form the format handed it over.
///
/// Borrowed where the format allows it (`Str`, `Bytes`) so that reading a member out of the buffer
/// does not copy a string that the input already holds.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Content<'de> {
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    Str(&'de str),
    String(String),
    Bytes(&'de [u8]),
    ByteBuf(Vec<u8>),
    Unit,
    None,
    Some(Box<Content<'de>>),
    Seq(Vec<Content<'de>>),
    Map(Vec<(Content<'de>, Content<'de>)>),
    Newtype(Box<Content<'de>>),
}

impl Content<'_> {
    /// What this value is, for an `invalid type` message.
    fn unexpected(&self) -> de::Unexpected<'_> {
        match self {
            Content::Bool(value) => de::Unexpected::Bool(*value),
            Content::I64(value) => de::Unexpected::Signed(*value),
            Content::U64(value) => de::Unexpected::Unsigned(*value),
            Content::F64(value) => de::Unexpected::Float(*value),
            Content::Str(value) => de::Unexpected::Str(value),
            Content::String(value) => de::Unexpected::Str(value),
            Content::Bytes(value) => de::Unexpected::Bytes(value),
            Content::ByteBuf(value) => de::Unexpected::Bytes(value),
            Content::Unit => de::Unexpected::Unit,
            Content::None | Content::Some(_) => de::Unexpected::Option,
            Content::Seq(_) => de::Unexpected::Seq,
            Content::Map(_) => de::Unexpected::Map,
            Content::Newtype(_) => de::Unexpected::NewtypeStruct,
        }
    }

    /// The text of a member name, when the value is one.
    fn as_str(&self) -> Option<&str> {
        match self {
            Content::Str(value) => Some(value),
            Content::String(value) => Some(value),
            Content::Bytes(value) => std::str::from_utf8(value).ok(),
            Content::ByteBuf(value) => std::str::from_utf8(value).ok(),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for Content<'de> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(ContentVisitor)
    }
}

struct ContentVisitor;

impl<'de> Visitor<'de> for ContentVisitor {
    type Value = Content<'de>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("any value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Content::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Content::I64(value))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Content::U64(value))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E> {
        Ok(Content::F64(value))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Content::String(value.to_owned()))
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E> {
        Ok(Content::Str(value))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(Content::String(value))
    }

    fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E> {
        Ok(Content::ByteBuf(value.to_vec()))
    }

    fn visit_borrowed_bytes<E>(self, value: &'de [u8]) -> Result<Self::Value, E> {
        Ok(Content::Bytes(value))
    }

    fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E> {
        Ok(Content::ByteBuf(value))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Content::Unit)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Content::None)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        Deserialize::deserialize(deserializer).map(|content| Content::Some(Box::new(content)))
    }

    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        Deserialize::deserialize(deserializer).map(|content| Content::Newtype(Box::new(content)))
    }

    fn visit_seq<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut items = Vec::with_capacity(access.size_hint().unwrap_or(0));
        while let Some(item) = access.next_element()? {
            items.push(item);
        }
        Ok(Content::Seq(items))
    }

    fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut members = Vec::with_capacity(access.size_hint().unwrap_or(0));
        while let Some(entry) = access.next_entry()? {
            members.push(entry);
        }
        // serde_json's `arbitrary_precision` spells every number it cannot hand over as a
        // primitive — floats, and integers past 64 bits — as a one-entry map keyed by its
        // private token, delivered through `deserialize_any` exactly like a real map. Kept as
        // a map, the replay hands a float visitor a `Map` and every `type: number` member
        // stops decoding the moment *any* crate in the consumer's build graph turns the
        // feature on. The token is stable public behavior: serde_json's own
        // `Number::deserialize` matches on it the same way.
        if let [(key, value)] = members.as_slice() {
            let token = match key {
                Content::Str(text) => Some(*text),
                Content::String(text) => Some(text.as_str()),
                _ => None,
            };
            if token == Some("$serde_json::private::Number") {
                let digits = match value {
                    Content::Str(text) => Some(*text),
                    Content::String(text) => Some(text.as_str()),
                    _ => None,
                };
                if let Some(digits) = digits {
                    if let Ok(number) = digits.parse::<u64>() {
                        return Ok(Content::U64(number));
                    }
                    if let Ok(number) = digits.parse::<i64>() {
                        return Ok(Content::I64(number));
                    }
                    if let Ok(number) = digits.parse::<f64>() {
                        return Ok(Content::F64(number));
                    }
                }
            }
        }
        Ok(Content::Map(members))
    }
}

/// A deserializer that replays one buffered value.
pub(crate) struct ContentDeserializer<'de, E> {
    content: Content<'de>,
    error: std::marker::PhantomData<E>,
}

impl<'de, E> ContentDeserializer<'de, E> {
    pub(crate) fn new(content: Content<'de>) -> Self {
        Self {
            content,
            error: std::marker::PhantomData,
        }
    }
}

impl<'de, E> Deserializer<'de> for ContentDeserializer<'de, E>
where
    E: de::Error,
{
    type Error = E;

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, E>
    where
        V: Visitor<'de>,
    {
        match self.content {
            Content::Bool(value) => visitor.visit_bool(value),
            Content::I64(value) => visitor.visit_i64(value),
            Content::U64(value) => visitor.visit_u64(value),
            Content::F64(value) => visitor.visit_f64(value),
            Content::Str(value) => visitor.visit_borrowed_str(value),
            Content::String(value) => visitor.visit_string(value),
            Content::Bytes(value) => visitor.visit_borrowed_bytes(value),
            Content::ByteBuf(value) => visitor.visit_byte_buf(value),
            Content::Unit => visitor.visit_unit(),
            Content::None => visitor.visit_none(),
            Content::Some(value) => visitor.visit_some(ContentDeserializer::new(*value)),
            // `end()` is the arity check, and it is not optional. A visitor asked for a fixed
            // arity — a tuple, a `[T; N]` — stops as soon as it has that many and never asks
            // again, so without this the leftovers are dropped **silently**. When the format reads
            // the sequence itself the format notices (`serde_json` answers a three-element array
            // read as a pair with a trailing-characters error); replay makes progeny the format,
            // and it has to notice on its own behalf. serde's own `SeqDeserializer::deserialize_any`
            // does exactly this, and handing the access straight to the visitor is what skips it.
            Content::Seq(items) => {
                let mut sequence =
                    de::value::SeqDeserializer::new(items.into_iter().map(
                        |item| -> ContentDeserializer<'de, E> { ContentDeserializer::new(item) },
                    ));
                let value = visitor.visit_seq(&mut sequence)?;
                sequence.end()?;
                Ok(value)
            }
            Content::Map(members) => {
                let mut map =
                    de::value::MapDeserializer::new(members.into_iter().map(|(key, value)| {
                        (
                            ContentDeserializer::<'de, E>::new(key),
                            ContentDeserializer::<'de, E>::new(value),
                        )
                    }));
                let value = visitor.visit_map(&mut map)?;
                map.end()?;
                Ok(value)
            }
            Content::Newtype(value) => {
                visitor.visit_newtype_struct(ContentDeserializer::new(*value))
            }
        }
    }

    /// An absent member and an explicit `null` both answer `visit_none`, which is what the derive
    /// does and therefore what this must do.
    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, E>
    where
        V: Visitor<'de>,
    {
        match self.content {
            Content::None | Content::Unit => visitor.visit_none(),
            Content::Some(value) => visitor.visit_some(ContentDeserializer::new(*value)),
            other => visitor.visit_some(ContentDeserializer::new(other)),
        }
    }

    fn deserialize_newtype_struct<V>(self, _name: &'static str, visitor: V) -> Result<V::Value, E>
    where
        V: Visitor<'de>,
    {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, E>
    where
        V: Visitor<'de>,
    {
        let (variant, value) = match self.content {
            Content::Map(mut members) if members.len() == 1 => match members.pop() {
                Some((variant, value)) => (variant, Some(value)),
                None => (Content::Unit, None),
            },
            content @ (Content::Str(_) | Content::String(_)) => (content, None),
            other => {
                return Err(de::Error::invalid_type(
                    other.unexpected(),
                    &"string or map",
                ));
            }
        };
        visitor.visit_enum(EnumReplay {
            variant,
            value,
            error: std::marker::PhantomData,
        })
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        unit unit_struct seq tuple tuple_struct map struct identifier ignored_any
    }
}

impl<'de, E> IntoDeserializer<'de, E> for ContentDeserializer<'de, E>
where
    E: de::Error,
{
    type Deserializer = Self;

    fn into_deserializer(self) -> Self {
        self
    }
}

/// Replays a buffered enum.
struct EnumReplay<'de, E> {
    variant: Content<'de>,
    value: Option<Content<'de>>,
    error: std::marker::PhantomData<E>,
}

impl<'de, E> EnumAccess<'de> for EnumReplay<'de, E>
where
    E: de::Error,
{
    type Error = E;
    type Variant = Self;

    fn variant_seed<V>(mut self, seed: V) -> Result<(V::Value, Self), E>
    where
        V: DeserializeSeed<'de>,
    {
        let variant = std::mem::replace(&mut self.variant, Content::Unit);
        let value = seed.deserialize(ContentDeserializer::new(variant))?;
        Ok((value, self))
    }
}

impl<'de, E> VariantAccess<'de> for EnumReplay<'de, E>
where
    E: de::Error,
{
    type Error = E;

    fn unit_variant(self) -> Result<(), E> {
        match self.value {
            None => Ok(()),
            Some(value) => Err(de::Error::invalid_type(value.unexpected(), &"unit variant")),
        }
    }

    fn newtype_variant_seed<T>(self, seed: T) -> Result<T::Value, E>
    where
        T: DeserializeSeed<'de>,
    {
        match self.value {
            Some(value) => seed.deserialize(ContentDeserializer::new(value)),
            None => Err(de::Error::invalid_type(
                de::Unexpected::UnitVariant,
                &"newtype variant",
            )),
        }
    }

    fn tuple_variant<V>(self, _len: usize, visitor: V) -> Result<V::Value, E>
    where
        V: Visitor<'de>,
    {
        match self.value {
            Some(value) => ContentDeserializer::new(value).deserialize_any(visitor),
            None => Err(de::Error::invalid_type(
                de::Unexpected::UnitVariant,
                &"tuple variant",
            )),
        }
    }

    fn struct_variant<V>(self, _fields: &'static [&'static str], visitor: V) -> Result<V::Value, E>
    where
        V: Visitor<'de>,
    {
        match self.value {
            Some(value) => ContentDeserializer::new(value).deserialize_any(visitor),
            None => Err(de::Error::invalid_type(
                de::Unexpected::UnitVariant,
                &"struct variant",
            )),
        }
    }
}

/// The deserializer a member that was not there is read from.
///
/// `deserialize_option` answers `visit_none`, which is exactly why a bare `Option<T>` field accepts
/// an absent member without `#[serde(default)]`. Everything else reports the member as missing.
pub(crate) struct Missing<E> {
    name: &'static str,
    error: std::marker::PhantomData<E>,
}

impl<E> Missing<E> {
    pub(crate) fn new(name: &'static str) -> Self {
        Self {
            name,
            error: std::marker::PhantomData,
        }
    }
}

impl<'de, E> Deserializer<'de> for Missing<E>
where
    E: de::Error,
{
    type Error = E;

    fn deserialize_any<V>(self, _visitor: V) -> Result<V::Value, E>
    where
        V: Visitor<'de>,
    {
        Err(de::Error::missing_field(self.name))
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, E>
    where
        V: Visitor<'de>,
    {
        visitor.visit_none()
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        unit unit_struct newtype_struct seq tuple tuple_struct map struct enum identifier
        ignored_any
    }
}

/// What to do with a member the type does not declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unknown {
    Ignore,
    Deny,
}

/// The members of one object, buffered by name.
pub(crate) struct Buffer<'de> {
    members: Vec<(&'static str, Content<'de>)>,
}

impl<'de> Buffer<'de> {
    /// Read a declared member out of the buffer.
    ///
    /// The scan is linear over a field list that is known and short at compile time, which is both
    /// faster than hashing and the reason duplicates are detectable at all.
    pub(crate) fn take<T, E>(&mut self, name: &'static str) -> Result<T, E>
    where
        T: Deserialize<'de>,
        E: de::Error,
    {
        match self.position(name) {
            Some(content) => T::deserialize(ContentDeserializer::new(content)),
            None => T::deserialize(Missing::new(name)),
        }
    }

    fn position(&mut self, name: &'static str) -> Option<Content<'de>> {
        let at = self
            .members
            .iter()
            .position(|(member, _)| *member == name)?;
        // `swap_remove` rather than `remove`: order does not matter once the names are known, and
        // every member is taken exactly once.
        Some(self.members.swap_remove(at).1)
    }
}

/// A type the buffered path can assemble from a buffer of its members.
///
/// A trait rather than a closure because the assembling has to be generic over the format's error
/// type, and a closure cannot be. That is not a detail: assembling has to happen **inside** the
/// visitor, so that the format sees the error come out of `visit_map` and attaches its own position
/// to it. Doing it afterwards produces a "missing field" message with no position, where the derive
/// produces one ending "at line 1 column 14"; the differential harness is what found that out.
pub(crate) trait Assemble<'de>: Sized {
    /// The struct's name, as the format is told it.
    const NAME: &'static str;
    /// Every member's wire name, in declaration order, which is also the positional order.
    const FIELDS: &'static [&'static str];
    /// Whether each member has a declared default, parallel to `FIELDS`.
    ///
    /// Only the positional form needs this: a member with a default that is absent from a sequence
    /// is not an error, and the *next* member without one is what the length complaint names. The
    /// derive behaves this way, so this has to.
    const DEFAULTED: &'static [bool];

    /// Build the value from its buffered members.
    fn assemble<E>(buffer: &mut Buffer<'de>) -> Result<Self, E>
    where
        E: de::Error;
}

/// Buffers an object's members, checking for duplicates and unknown names, then assembles it.
pub(crate) struct BufferVisitor<T> {
    unknown: Unknown,
    value: std::marker::PhantomData<T>,
}

impl<T> BufferVisitor<T> {
    pub(crate) fn new(unknown: Unknown) -> Self {
        Self {
            unknown,
            value: std::marker::PhantomData,
        }
    }
}

impl<'de, T> Visitor<'de> for BufferVisitor<T>
where
    T: Assemble<'de>,
{
    type Value = T;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "struct {}", T::NAME)
    }

    fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let fields = T::FIELDS;
        let mut buffer = Buffer {
            members: Vec::with_capacity(fields.len()),
        };
        while let Some(key) = access.next_key::<Content<'de>>()? {
            let declared = key
                .as_str()
                .and_then(|name| fields.iter().find(|field| **field == name).copied());
            match declared {
                Some(field) => {
                    if buffer.members.iter().any(|(member, _)| *member == field) {
                        return Err(de::Error::duplicate_field(field));
                    }
                    buffer.members.push((field, access.next_value()?));
                }
                None => match self.unknown {
                    Unknown::Ignore => {
                        access.next_value::<de::IgnoredAny>()?;
                    }
                    Unknown::Deny => {
                        let name = key.as_str().unwrap_or_default().to_owned();
                        return Err(de::Error::unknown_field(&name, fields));
                    }
                },
            }
        }
        T::assemble(&mut buffer)
    }

    /// A struct read from a sequence, positionally.
    ///
    /// The derive supports this, so this has to as well — including the shape of its error, which
    /// names the element count rather than the type.
    fn visit_seq<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let fields = T::FIELDS;
        let mut buffer = Buffer {
            members: Vec::with_capacity(fields.len()),
        };
        for (index, field) in fields.iter().enumerate() {
            match access.next_element::<Content<'de>>()? {
                Some(content) => buffer.members.push((field, content)),
                // A member with a default fills itself in; everything past the end of the sequence
                // is absent too, so the walk continues until it meets one that cannot.
                None if T::DEFAULTED.get(index).copied().unwrap_or(false) => {}
                None => {
                    return Err(de::Error::invalid_length(
                        index,
                        &Elements {
                            name: T::NAME,
                            count: fields.len(),
                        },
                    ));
                }
            }
        }
        T::assemble(&mut buffer)
    }
}

/// What a struct read from a sequence expected, worded the way the derive words it.
struct Elements {
    name: &'static str,
    count: usize,
}

impl de::Expected for Elements {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "struct {} with {} element{}",
            self.name,
            self.count,
            if self.count == 1 { "" } else { "s" }
        )
    }
}

/// Resolves a carried-tag enum: reads the tag member of a buffered payload and replays the whole
/// payload — tag included — into the variant type it names.
///
/// The variant types of such an enum declare the tag member themselves, which is why nothing is
/// taken out of the payload before the replay: the type keeps the member and writes it back when
/// serialized, so a value round-trips with its tag where the wire had it.
///
/// The resolver receives the position of the matched name in `variants`, always in range: an
/// unmatched name has already been refused as an unknown variant. The generated match binds its
/// last variant with a `_` arm on the strength of that guarantee, which is what keeps the match
/// total without a dead error arm.
pub(crate) fn dispatch<'de, T, E, F>(
    name: &'static str,
    tag: &'static str,
    variants: &'static [&'static str],
    payload: Content<'de>,
    resolve: F,
) -> Result<T, E>
where
    E: de::Error,
    F: FnOnce(usize, ContentDeserializer<'de, E>) -> Result<T, E>,
{
    let choice = choice(&payload, name, tag, variants)?;
    resolve(choice, ContentDeserializer::new(payload))
}

/// Which variant a payload names in its tag member.
fn choice<E>(
    payload: &Content<'_>,
    name: &'static str,
    tag: &'static str,
    variants: &'static [&'static str],
) -> Result<usize, E>
where
    E: de::Error,
{
    let Content::Map(members) = payload else {
        return Err(de::Error::invalid_type(
            payload.unexpected(),
            &TaggedPayload { name },
        ));
    };
    let mut found: Option<&str> = None;
    for (key, value) in members {
        if key.as_str() != Some(tag) {
            continue;
        }
        // Refused here rather than left to the replay so that a payload writing two tags fails
        // the same way whichever variant implementation would have read it second.
        if found.is_some() {
            return Err(de::Error::duplicate_field(tag));
        }
        let Some(text) = value.as_str() else {
            return Err(de::Error::invalid_type(value.unexpected(), &"a string"));
        };
        found = Some(text);
    }
    let Some(text) = found else {
        return Err(de::Error::missing_field(tag));
    };
    variants
        .iter()
        .position(|variant| *variant == text)
        .ok_or_else(|| de::Error::unknown_variant(text, variants))
}

/// What a carried-tag enum expected, for an `invalid type` message.
struct TaggedPayload {
    name: &'static str,
}

impl de::Expected for TaggedPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "tagged enum {}", self.name)
    }
}

/// Resolves a fieldless enum from the variant name alone.
///
/// No buffering: the variant is decided by the identifier, so this keeps working with formats that
/// are not self-describing and reports the format's own error positions.
pub(crate) struct UnitVariants<T> {
    name: &'static str,
    variants: &'static [&'static str],
    resolve: fn(&str) -> Option<T>,
}

impl<T> UnitVariants<T> {
    pub(crate) fn new(
        name: &'static str,
        variants: &'static [&'static str],
        resolve: fn(&str) -> Option<T>,
    ) -> Self {
        Self {
            name,
            variants,
            resolve,
        }
    }
}

impl<'de, T> Visitor<'de> for UnitVariants<T> {
    type Value = T;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "enum {}", self.name)
    }

    fn visit_enum<A>(self, access: A) -> Result<Self::Value, A::Error>
    where
        A: EnumAccess<'de>,
    {
        let (value, variant) = access.variant_seed(NameSeed {
            variants: self.variants,
            resolve: self.resolve,
        })?;
        variant.unit_variant()?;
        Ok(value)
    }
}

/// Reads a variant identifier and resolves it, or says which names were expected.
struct NameSeed<T> {
    variants: &'static [&'static str],
    resolve: fn(&str) -> Option<T>,
}

impl<'de, T> DeserializeSeed<'de> for NameSeed<T> {
    type Value = T;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_identifier(self)
    }
}

impl<T> Visitor<'_> for NameSeed<T> {
    type Value = T;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("variant identifier")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        (self.resolve)(value).ok_or_else(|| de::Error::unknown_variant(value, self.variants))
    }

    fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        match std::str::from_utf8(value) {
            Ok(text) => self.visit_str(text),
            Err(_) => Err(de::Error::invalid_type(de::Unexpected::Bytes(value), &self)),
        }
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        // The derive accepts a variant index too, so this does as well.
        match usize::try_from(value)
            .ok()
            .and_then(|index| self.variants.get(index))
        {
            Some(name) => {
                (self.resolve)(name).ok_or_else(|| de::Error::unknown_variant(name, self.variants))
            }
            None => Err(de::Error::invalid_value(
                de::Unexpected::Unsigned(value),
                &"variant index",
            )),
        }
    }
}

/// Serialize a carried-tag variant with its tag member guaranteed on the wire.
///
/// The enum variant is the tag's source of truth: a hand-built payload may hold nothing, or a
/// stale value, in its own tag member, and the wire must name the arm the caller chose — the
/// same arm this deserializer would dispatch back to. The member is written with the variant's
/// value whatever the payload holds.
pub fn serialize_carried<T, S>(
    value: &T,
    tag: &'static str,
    variant: &str,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    T: serde::Serialize,
    S: serde::Serializer,
{
    value.serialize(TagInjector {
        inner: serializer,
        tag,
        variant,
    })
}

/// A serializer that writes one payload as a map with its tag entry rewritten.
///
/// The tag goes out first with the variant's value; the payload's own entry for that key — set,
/// unset, or stale — is dropped. Forwarding field by field is what keeps the payload's values
/// the serializer's own problem: a detour through `serde_json::Value` turned a non-finite float
/// into `null` where direct serialization errors, and re-spelled every number through `Value`'s
/// model, which under `arbitrary_precision` is not the identity.
struct TagInjector<'a, S> {
    inner: S,
    tag: &'static str,
    variant: &'a str,
}

/// The map underway, with the tag already written.
struct TaggedMap<M> {
    inner: M,
    tag: &'static str,
    /// Set between a key that matched the tag and its value, which is skipped.
    skip_value: bool,
}

impl<S: serde::Serializer> serde::Serializer for TagInjector<'_, S> {
    type Ok = S::Ok;
    type Error = S::Error;
    type SerializeSeq = serde::ser::Impossible<S::Ok, S::Error>;
    type SerializeTuple = serde::ser::Impossible<S::Ok, S::Error>;
    type SerializeTupleStruct = serde::ser::Impossible<S::Ok, S::Error>;
    type SerializeTupleVariant = serde::ser::Impossible<S::Ok, S::Error>;
    type SerializeMap = TaggedMap<S::SerializeMap>;
    type SerializeStruct = TaggedMap<S::SerializeMap>;
    type SerializeStructVariant = serde::ser::Impossible<S::Ok, S::Error>;

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        // No length hint on purpose: dropping the payload's tag entry and adding this one can
        // move the count in either direction, and a self-describing format never needs it.
        let mut inner = self.inner.serialize_map(None)?;
        serde::ser::SerializeMap::serialize_entry(&mut inner, self.tag, self.variant)?;
        Ok(TaggedMap {
            inner,
            tag: self.tag,
            skip_value: false,
        })
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        self.serialize_map(None)
    }

    fn serialize_newtype_struct<T: serde::Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        value.serialize(self)
    }

    fn serialize_some<T: serde::Serialize + ?Sized>(
        self,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        value.serialize(self)
    }

    fn is_human_readable(&self) -> bool {
        self.inner.is_human_readable()
    }

    // Everything below is unreachable through the generator — `refuse_outright` admits only
    // struct payloads into a carried-tag union — and refuses out loud rather than writing a
    // tagless value if that ever stopped being true.
    fn serialize_bool(self, _value: bool) -> Result<Self::Ok, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_i8(self, _value: i8) -> Result<Self::Ok, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_i16(self, _value: i16) -> Result<Self::Ok, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_i32(self, _value: i32) -> Result<Self::Ok, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_i64(self, _value: i64) -> Result<Self::Ok, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_u8(self, _value: u8) -> Result<Self::Ok, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_u16(self, _value: u16) -> Result<Self::Ok, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_u32(self, _value: u32) -> Result<Self::Ok, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_u64(self, _value: u64) -> Result<Self::Ok, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_f32(self, _value: f32) -> Result<Self::Ok, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_f64(self, _value: f64) -> Result<Self::Ok, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_char(self, _value: char) -> Result<Self::Ok, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_str(self, _value: &str) -> Result<Self::Ok, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_bytes(self, _value: &[u8]) -> Result<Self::Ok, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_newtype_variant<T: serde::Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Err(not_an_object())
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        Err(not_an_object())
    }
}

fn not_an_object<E: serde::ser::Error>() -> E {
    E::custom("a carried-tag payload must serialize as an object")
}

impl<M: serde::ser::SerializeMap> serde::ser::SerializeStruct for TaggedMap<M> {
    type Ok = M::Ok;
    type Error = M::Error;

    fn serialize_field<T: serde::Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        if key == self.tag {
            return Ok(());
        }
        self.inner.serialize_entry(key, value)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.inner.end()
    }
}

impl<M: serde::ser::SerializeMap> serde::ser::SerializeMap for TaggedMap<M> {
    type Ok = M::Ok;
    type Error = M::Error;

    fn serialize_key<T: serde::Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Self::Error> {
        // A payload with a captured or flattened member serializes as a map, so the key is a
        // value rather than a `&'static str`; the probe answers whether it spells the tag
        // without committing anything to the inner serializer.
        if key.serialize(KeyProbe { tag: self.tag }).unwrap_or(false) {
            self.skip_value = true;
            return Ok(());
        }
        self.inner.serialize_key(key)
    }

    fn serialize_value<T: serde::Serialize + ?Sized>(
        &mut self,
        value: &T,
    ) -> Result<(), Self::Error> {
        if self.skip_value {
            self.skip_value = false;
            return Ok(());
        }
        self.inner.serialize_value(value)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.inner.end()
    }
}

/// Answers whether a key serializes as exactly the tag's text.
struct KeyProbe<'a> {
    tag: &'a str,
}

#[derive(Debug)]
struct NotText;

impl fmt::Display for NotText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("not a text key")
    }
}

impl std::error::Error for NotText {}

impl serde::ser::Error for NotText {
    fn custom<T: fmt::Display>(_message: T) -> Self {
        Self
    }
}

impl serde::Serializer for KeyProbe<'_> {
    type Ok = bool;
    type Error = NotText;
    type SerializeSeq = serde::ser::Impossible<bool, NotText>;
    type SerializeTuple = serde::ser::Impossible<bool, NotText>;
    type SerializeTupleStruct = serde::ser::Impossible<bool, NotText>;
    type SerializeTupleVariant = serde::ser::Impossible<bool, NotText>;
    type SerializeMap = serde::ser::Impossible<bool, NotText>;
    type SerializeStruct = serde::ser::Impossible<bool, NotText>;
    type SerializeStructVariant = serde::ser::Impossible<bool, NotText>;

    fn serialize_str(self, value: &str) -> Result<bool, NotText> {
        Ok(value == self.tag)
    }

    fn serialize_char(self, value: char) -> Result<bool, NotText> {
        let mut buffer = [0u8; 4];
        Ok(value.encode_utf8(&mut buffer) == self.tag)
    }

    fn serialize_newtype_struct<T: serde::Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<bool, NotText> {
        value.serialize(self)
    }

    fn serialize_bool(self, _value: bool) -> Result<bool, NotText> {
        Err(NotText)
    }
    fn serialize_i8(self, _value: i8) -> Result<bool, NotText> {
        Err(NotText)
    }
    fn serialize_i16(self, _value: i16) -> Result<bool, NotText> {
        Err(NotText)
    }
    fn serialize_i32(self, _value: i32) -> Result<bool, NotText> {
        Err(NotText)
    }
    fn serialize_i64(self, _value: i64) -> Result<bool, NotText> {
        Err(NotText)
    }
    fn serialize_u8(self, _value: u8) -> Result<bool, NotText> {
        Err(NotText)
    }
    fn serialize_u16(self, _value: u16) -> Result<bool, NotText> {
        Err(NotText)
    }
    fn serialize_u32(self, _value: u32) -> Result<bool, NotText> {
        Err(NotText)
    }
    fn serialize_u64(self, _value: u64) -> Result<bool, NotText> {
        Err(NotText)
    }
    fn serialize_f32(self, _value: f32) -> Result<bool, NotText> {
        Err(NotText)
    }
    fn serialize_f64(self, _value: f64) -> Result<bool, NotText> {
        Err(NotText)
    }
    fn serialize_bytes(self, _value: &[u8]) -> Result<bool, NotText> {
        Err(NotText)
    }
    fn serialize_none(self) -> Result<bool, NotText> {
        Err(NotText)
    }
    fn serialize_some<T: serde::Serialize + ?Sized>(self, _value: &T) -> Result<bool, NotText> {
        Err(NotText)
    }
    fn serialize_unit(self) -> Result<bool, NotText> {
        Err(NotText)
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<bool, NotText> {
        Err(NotText)
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
    ) -> Result<bool, NotText> {
        Err(NotText)
    }
    fn serialize_newtype_variant<T: serde::Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<bool, NotText> {
        Err(NotText)
    }
    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, NotText> {
        Err(NotText)
    }
    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, NotText> {
        Err(NotText)
    }
    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, NotText> {
        Err(NotText)
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, NotText> {
        Err(NotText)
    }
    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, NotText> {
        Err(NotText)
    }
    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, NotText> {
        Err(NotText)
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, NotText> {
        Err(NotText)
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::{self, OptionExt as _};

    #[test]
    fn a_carried_tag_is_written_whatever_the_payload_holds() {
        #[derive(serde::Serialize)]
        struct Payment {
            kind: Option<String>,
            amount: u32,
        }
        // Unset in the payload: the variant's value is inserted.
        let unset = Payment {
            kind: None,
            amount: 5,
        };
        let written =
            super::serialize_carried(&unset, "kind", "card", serde_json::value::Serializer)
                .unwrap_or(serde_json::Value::Null);
        assert_eq!(
            written.get("kind").and_then(serde_json::Value::as_str),
            Some("card")
        );
        assert_eq!(
            written.get("amount").and_then(serde_json::Value::as_u64),
            Some(5)
        );
        // Stale in the payload: the variant's value wins, because the variant chose the arm.
        let stale = Payment {
            kind: Some("cash".to_owned()),
            amount: 7,
        };
        let written =
            super::serialize_carried(&stale, "kind", "card", serde_json::value::Serializer)
                .unwrap_or(serde_json::Value::Null);
        assert_eq!(
            written.get("kind").and_then(serde_json::Value::as_str),
            Some("card")
        );
    }

    #[test]
    fn a_flattened_payload_serializes_as_a_map_and_still_carries_its_tag() {
        // A payload with a captured member serializes through `serialize_map`, where keys are
        // values rather than static strings; the tag has to win there too.
        #[derive(serde::Serialize)]
        struct Card {
            kind: Option<String>,
            #[serde(flatten)]
            extra: std::collections::BTreeMap<String, serde_json::Value>,
        }
        let payload = Card {
            kind: Some("stale".to_owned()),
            extra: [("note".to_owned(), serde_json::json!("kept"))]
                .into_iter()
                .collect(),
        };
        let written =
            super::serialize_carried(&payload, "kind", "card", serde_json::value::Serializer)
                .unwrap_or(serde_json::Value::Null);
        assert_eq!(
            written.get("kind").and_then(serde_json::Value::as_str),
            Some("card")
        );
        assert_eq!(
            written.get("note").and_then(serde_json::Value::as_str),
            Some("kept")
        );
    }

    /// This workspace builds with `serde_json/arbitrary_precision` on, so the test fails the
    /// moment the token handling regresses: under the feature, `1.5` arrives through
    /// `deserialize_any` as a one-entry map keyed `$serde_json::private::Number`, and an
    /// unconverted capture replays it into `f64::deserialize` as "invalid type: map".
    #[test_util::test]
    fn a_float_survives_the_buffered_path_under_arbitrary_precision() {
        let captured: Content =
            serde_json::from_str("{\"amount\": 1.5, \"count\": 3, \"big\": 18446744073709551615}")?;
        let Content::Map(members) = captured else {
            eyre::bail!("a JSON object captures as a map");
        };
        let read = |wanted: &str| {
            members
                .iter()
                .find(|(key, _)| {
                    matches!(key, Content::Str(text) if *text == wanted)
                        || matches!(key, Content::String(text) if text == wanted)
                })
                .map(|(_, value)| value.clone())
                .ok_or_eyre("the member exists")
        };
        let amount = f64::deserialize(ContentDeserializer::<serde_json::Error>::new(read(
            "amount",
        )?))?;
        assert!((amount - 1.5).abs() < f64::EPSILON);
        let count = u64::deserialize(ContentDeserializer::<serde_json::Error>::new(read(
            "count",
        )?))?;
        assert_eq!(count, 3);
        let big = u64::deserialize(ContentDeserializer::<serde_json::Error>::new(read("big")?))?;
        assert_eq!(big, u64::MAX);
    }

    use super::{Content, ContentDeserializer};
    use std::fmt;

    use serde::de::{MapAccess, Visitor};
    use serde::{Deserialize, Deserializer};

    #[derive(Debug)]
    struct FirstMember;

    impl<'de> Deserialize<'de> for FirstMember {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserializer.deserialize_map(FirstMemberVisitor)
        }
    }

    struct FirstMemberVisitor;

    impl<'de> Visitor<'de> for FirstMemberVisitor {
        type Value = FirstMember;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map with at least one member")
        }

        fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let _: Option<(String, u64)> = access.next_entry()?;
            Ok(FirstMember)
        }
    }

    /// Replaying a buffered sequence into a fixed-arity target must reject the extra elements.
    ///
    /// The trailing-element bug class, and the reason it is specific to *buffered* replay: when the
    /// format reads a tuple directly, the format itself notices the leftovers — `serde_json` answers
    /// a three-element array read as a pair with a trailing-characters error. Replay makes progeny
    /// the format, and a `SeqAccess` that stops after the arity it was asked for and never looks
    /// again drops what is left **silently**, which is the one failure mode this project forbids.
    /// serde's own `SeqDeserializer` exposes `end()` for exactly this and `deserialize_any` calls
    /// it; a visitor handed the access directly skips it.
    #[test]
    fn a_buffered_sequence_replayed_into_a_shorter_tuple_is_refused() {
        let content = Content::Seq(vec![Content::U64(1), Content::U64(2), Content::U64(3)]);
        let deserializer = ContentDeserializer::<serde::de::value::Error>::new(content);
        let read = <(u64, u64)>::deserialize(deserializer);
        assert!(read.is_err(), "read {read:?} instead of refusing the third");
    }

    #[test]
    fn a_buffered_sequence_replayed_into_a_shorter_array_is_refused() {
        let content = Content::Seq(vec![Content::U64(1), Content::U64(2), Content::U64(3)]);
        let deserializer = ContentDeserializer::<serde::de::value::Error>::new(content);
        let read = <[u64; 2]>::deserialize(deserializer);
        assert!(read.is_err(), "read {read:?} instead of refusing the third");
    }

    /// The arity it was promised still works, so the check above is not just a refusal.
    #[test]
    fn a_buffered_sequence_of_the_right_length_is_read() {
        let content = Content::Seq(vec![Content::U64(1), Content::U64(2)]);
        let deserializer = ContentDeserializer::<serde::de::value::Error>::new(content);
        assert_eq!(<(u64, u64)>::deserialize(deserializer), Ok((1, 2)));
    }

    #[test]
    fn a_buffered_map_cannot_silently_drop_members_a_visitor_did_not_read() {
        let direct = serde_json::from_str::<FirstMember>(r#"{"first":1,"second":2}"#);
        assert!(direct.is_err(), "the format itself accepted {direct:?}");

        let content = Content::Map(vec![
            (Content::String("first".to_owned()), Content::U64(1)),
            (Content::String("second".to_owned()), Content::U64(2)),
        ]);
        let deserializer = ContentDeserializer::<serde::de::value::Error>::new(content);
        let replayed = FirstMember::deserialize(deserializer);
        assert!(
            replayed.is_err(),
            "replay accepted {replayed:?} and silently dropped the second member"
        );
    }

    /// A carried-tag enum, written the way the generator writes one: the variant types declare
    /// the tag member and keep it, deserializing dispatches on it without taking it out, and
    /// serializing is the variant as it is.
    mod carried {
        use super::super::{Content, dispatch};
        use serde::{Deserialize, Deserializer};

        #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        struct Card {
            kind: String,
            number: String,
        }

        #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        struct Cash {
            kind: String,
        }

        #[derive(Debug, PartialEq)]
        enum Payment {
            Card(Card),
            Cash(Cash),
        }

        impl<'de> Deserialize<'de> for Payment {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                const NAME: &str = "Payment";
                const TAG: &str = "kind";
                const VARIANTS: &[&str] = &["card", "cash"];
                let payload: Content<'de> = Deserialize::deserialize(deserializer)?;
                dispatch(
                    NAME,
                    TAG,
                    VARIANTS,
                    payload,
                    |variant, replay| match variant {
                        0usize => Deserialize::deserialize(replay).map(Self::Card),
                        _ => Deserialize::deserialize(replay).map(Self::Cash),
                    },
                )
            }
        }

        impl serde::Serialize for Payment {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                match self {
                    Self::Card(value) => serde::Serialize::serialize(value, serializer),
                    Self::Cash(value) => serde::Serialize::serialize(value, serializer),
                }
            }
        }

        fn read(wire: &str) -> Payment {
            match serde_json::from_str(wire) {
                Ok(payment) => payment,
                Err(err) => panic!("{wire} was refused: {err}"),
            }
        }

        fn refusal(wire: &str) -> String {
            match serde_json::from_str::<Payment>(wire) {
                Ok(payment) => panic!("{wire} was accepted as {payment:?}"),
                Err(err) => err.to_string(),
            }
        }

        #[test]
        fn a_payload_round_trips_with_its_tag_where_the_wire_had_it() {
            let wire = r#"{"kind":"card","number":"4"}"#;
            let payment = read(wire);
            assert_eq!(
                payment,
                Payment::Card(Card {
                    kind: "card".to_owned(),
                    number: "4".to_owned(),
                })
            );
            let written = match serde_json::to_string(&payment) {
                Ok(written) => written,
                Err(err) => panic!("{payment:?} did not serialize: {err}"),
            };
            assert_eq!(written, wire);
        }

        #[test]
        fn the_tag_decides_the_variant_even_when_another_would_accept_the_payload() {
            // `Card` ignores unknown members and every member of this payload it declares is
            // present, so structural matching would happily read it as a `Card`. The tag says
            // otherwise, and the tag is what dispatches.
            let payment = read(r#"{"kind":"cash","number":"4"}"#);
            assert_eq!(
                payment,
                Payment::Cash(Cash {
                    kind: "cash".to_owned(),
                })
            );
        }

        #[test]
        fn a_payload_without_the_tag_reports_the_member_as_missing() {
            let message = refusal(r#"{"number":"4"}"#);
            assert!(message.contains("kind"), "{message}");
        }

        #[test]
        fn a_tag_naming_no_variant_reports_the_expected_names() {
            let message = refusal(r#"{"kind":"cheque"}"#);
            assert!(
                message.contains("cheque") && message.contains("card") && message.contains("cash"),
                "{message}"
            );
        }

        #[test]
        fn a_payload_that_is_not_a_map_reports_the_enum_it_expected() {
            let message = refusal(r#""card""#);
            assert!(message.contains("tagged enum Payment"), "{message}");
        }

        #[test]
        fn a_payload_writing_the_tag_twice_is_refused() {
            // `serde_json` itself keeps the last of two duplicate members, so the duplicate has
            // to be refused from the buffer, where both survive.
            let message = refusal(r#"{"kind":"card","kind":"cash"}"#);
            assert!(message.contains("duplicate"), "{message}");
        }

        #[test]
        fn a_tag_that_is_not_a_string_is_refused_before_any_variant_reads_it() {
            let message = refusal(r#"{"kind":4}"#);
            assert!(message.contains("invalid type"), "{message}");
        }
    }
}
