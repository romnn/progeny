//! Three-state presence for properties that may be absent or explicitly null.

/// An optional and nullable property without collapsing absent and null.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Presence<T> {
    /// The containing object does not carry this property.
    #[default]
    Omitted,
    /// The containing object carries this property as `null`.
    Null,
    /// The containing object carries a non-null value.
    Value(T),
}

impl<T> Presence<T> {
    /// Whether the containing object should omit this property when serialized.
    #[must_use]
    pub const fn is_omitted(&self) -> bool {
        matches!(self, Self::Omitted)
    }
}

impl<T> serde::Serialize for Presence<T>
where
    T: serde::Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            // The containing struct skips `Omitted` before this implementation is called.
            // Serializing the value on its own has no member to omit, so null is the only
            // representation that does not invent a value.
            Self::Omitted | Self::Null => serializer.serialize_none(),
            Self::Value(value) => serde::Serialize::serialize(value, serializer),
        }
    }
}

impl<'de, T> serde::Deserialize<'de> for Presence<T>
where
    T: serde::Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_option(PresenceVisitor(std::marker::PhantomData))
    }
}

struct PresenceVisitor<T>(std::marker::PhantomData<T>);

impl<'de, T> serde::de::Visitor<'de> for PresenceVisitor<T>
where
    T: serde::Deserialize<'de>,
{
    type Value = Presence<T>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("null or a value")
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Presence::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Presence::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        serde::Deserialize::deserialize(deserializer).map(Presence::Value)
    }
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;

    use super::Presence;

    #[test_util::test]
    fn null_and_value_round_trip_without_conflating_the_variants() {
        assert!(Presence::<String>::Omitted.is_omitted());
        assert!(!Presence::<String>::Null.is_omitted());

        let null = serde_json::from_str::<Presence<String>>("null")?;
        assert_eq!(null, Presence::Null);

        let value = serde_json::from_str::<Presence<String>>(r#""value""#)?;
        assert_eq!(value, Presence::Value("value".to_owned()));
    }
}
