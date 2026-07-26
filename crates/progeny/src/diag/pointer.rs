//! JSON Pointer ([RFC 6901]) addresses into the source document.
//!
//! [RFC 6901]: https://www.rfc-editor.org/rfc/rfc6901

use std::fmt;

/// The address of a node inside the source document.
///
/// Diagnostics point at documents, not at progeny's own data structures, so the whole
/// pipeline threads a pointer through parsing and hands it to every diagnostic it raises.
/// Tokens are stored unescaped; escaping happens once, in [`fmt::Display`].
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JsonPointer {
    tokens: Vec<String>,
}

impl JsonPointer {
    /// The pointer to the document root (`""`).
    #[must_use]
    pub fn root() -> Self {
        Self::default()
    }

    /// This pointer with `token` appended.
    #[must_use]
    pub fn child(&self, token: impl Into<String>) -> Self {
        let mut tokens = self.tokens.clone();
        tokens.push(token.into());
        Self { tokens }
    }

    /// The unescaped reference tokens, outermost first.
    #[must_use]
    pub fn tokens(&self) -> &[String] {
        &self.tokens
    }

    /// Whether this pointer addresses the document root.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Parse the RFC 6901 textual form, resolving `~1` to `/` and `~0` to `~`.
    ///
    /// A leading `/` is required for every non-empty pointer; anything else is not a
    /// pointer, and the caller — always a `$ref` reader — is expected to treat `None` as a
    /// reference it cannot interpret rather than to guess.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        if text.is_empty() {
            return Some(Self::root());
        }
        let body = text.strip_prefix('/')?;
        let tokens = body
            .split('/')
            .map(|token| token.replace("~1", "/").replace("~0", "~"))
            .collect();
        Some(Self { tokens })
    }

    pub(crate) fn push(&mut self, token: impl Into<String>) {
        self.tokens.push(token.into());
    }

    pub(crate) fn pop(&mut self) {
        self.tokens.pop();
    }
}

impl fmt::Display for JsonPointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for token in &self.tokens {
            // Order matters: escaping `~` first would double-escape the `~` that `/`
            // introduces.
            write!(f, "/{}", token.replace('~', "~0").replace('/', "~1"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::JsonPointer;

    #[test]
    fn root_renders_as_the_empty_string() {
        assert_eq!(JsonPointer::root().to_string(), "");
    }

    #[test]
    fn tokens_are_escaped_on_render() {
        let pointer = JsonPointer::root()
            .child("paths")
            .child("/pets/{petId}")
            .child("a~b");
        assert_eq!(pointer.to_string(), "/paths/~1pets~1{petId}/a~0b");
    }

    #[test]
    fn render_and_parse_round_trip() {
        for pointer in [
            JsonPointer::root(),
            JsonPointer::root().child("components").child("schemas"),
            JsonPointer::root().child("paths").child("/a~b/c"),
            JsonPointer::root().child(""),
        ] {
            let text = pointer.to_string();
            assert_eq!(JsonPointer::parse(&text), Some(pointer), "text: {text:?}");
        }
    }

    #[test]
    fn parse_rejects_text_that_is_not_a_pointer() {
        assert_eq!(JsonPointer::parse("components/schemas"), None);
        assert_eq!(JsonPointer::parse("#/components"), None);
    }
}
