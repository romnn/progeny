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

#![allow(
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
            Content::Seq(items) => {
                visitor.visit_seq(de::value::SeqDeserializer::new(items.into_iter().map(
                    |item| -> ContentDeserializer<'de, E> { ContentDeserializer::new(item) },
                )))
            }
            Content::Map(members) => visitor.visit_map(MapReplay {
                members: members.into_iter(),
                value: None,
                error: std::marker::PhantomData,
            }),
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

/// Replays a buffered map.
struct MapReplay<'de, E> {
    members: std::vec::IntoIter<(Content<'de>, Content<'de>)>,
    value: Option<Content<'de>>,
    error: std::marker::PhantomData<E>,
}

impl<'de, E> MapAccess<'de> for MapReplay<'de, E>
where
    E: de::Error,
{
    type Error = E;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, E>
    where
        K: DeserializeSeed<'de>,
    {
        match self.members.next() {
            Some((key, value)) => {
                self.value = Some(value);
                seed.deserialize(ContentDeserializer::new(key)).map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, E>
    where
        V: DeserializeSeed<'de>,
    {
        match self.value.take() {
            Some(value) => seed.deserialize(ContentDeserializer::new(value)),
            None => Err(de::Error::custom("value is missing")),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.members.len())
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

    /// Read a declared member, or fall back to its declared default.
    ///
    /// What `#[serde(default = "…")]` does, and the reason it is a separate function: a default is a
    /// statement about the absent case, and a buffered implementation that silently answered "not
    /// there" would disagree with the derive about it.
    pub(crate) fn take_or<T, E>(&mut self, name: &'static str, fallback: fn() -> T) -> Result<T, E>
    where
        T: Deserialize<'de>,
        E: de::Error,
    {
        match self.position(name) {
            Some(content) => T::deserialize(ContentDeserializer::new(content)),
            None => Ok(fallback()),
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
