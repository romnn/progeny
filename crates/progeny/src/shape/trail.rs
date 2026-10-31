//! The keyed shapes a walk over one JSON value is inside.

use super::ShapeKey;

/// The keyed shapes a walk has entered without descending into a child of the value it judges.
///
/// A shape can name itself without consuming any input: a `oneOf` whose one branch is the schema
/// itself, two aliases of each other, or a nullable wrapper around its own union.
/// A walk judging one value against such a shape meets the same key again with the same value,
/// and would recurse until the stack overflows.
/// A member or an element is a smaller value, so a walk descending into one starts a new trail,
/// and a recursive type meeting itself there is the ordinary case.
#[derive(Debug, Default)]
pub(crate) struct Trail {
    entered: Vec<ShapeKey>,
}

impl Trail {
    /// Runs `walk` inside `key`, or returns `None` when the walk is already inside it.
    ///
    /// The key is left again once `walk` returns, so two branches of one union that both name it
    /// each get to judge the value.
    pub(crate) fn enter<R>(
        &mut self,
        key: &ShapeKey,
        walk: impl FnOnce(&mut Self) -> R,
    ) -> Option<R> {
        if self.entered.contains(key) {
            return None;
        }
        self.entered.push(key.clone());
        let result = walk(self);
        self.entered.pop();
        Some(result)
    }

    /// The keys entered since the walk first entered `key`, `key` first, when it is inside it.
    ///
    /// The cycle a walk meeting `key` again has gone round with the same value.
    pub(crate) fn since(&self, key: &ShapeKey) -> Option<&[ShapeKey]> {
        let first = self.entered.iter().position(|entered| entered == key)?;
        self.entered.get(first..)
    }
}
