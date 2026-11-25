# Coding guidelines

- Use `indoc::indoc!` and `indoc::formatdoc!` for every string value that
  contains multiple lines. Do not assemble a multiline value with escaped
  newlines, continuation escapes, or adjacent fragments.
- Never use `anyhow`.
- Use `color-eyre` only in tests or at the outermost boundary of a leaf binary
  or CLI. Keep it out of reusable library APIs and dependencies; when it is
  test-only, keep it in development dependencies.
- Make fallible tests return `eyre::Result<()>`, normally through
  `#[test_util::test]`. Propagate `Result` values with `?` and convert optional
  test fixtures with `OptionExt::ok_or_eyre()`; do not hide panicking extraction
  behind assertion helpers.
- Import the `color_eyre::eyre` module and write `eyre::Result` and
  `eyre::eyre!`, for example with `use color_eyre::eyre;` or
  `use color_eyre::eyre::{self, OptionExt as _};`. Never spell the fully
  qualified `color_eyre::eyre::Result` or `color_eyre::eyre::eyre!` paths.
  Never import eyre's `Result` as plain `Result<T>` because it shadows
  `std::result::Result<T>`; always write `eyre::Result<T>` in code.
- Define proper library error types with `thiserror`; preserve useful variants,
  sources, and context instead of erasing failures into opaque strings.
- Do not call `unwrap()` or `expect()` unless an invariant makes failure
  impossible and the panicking call is clearer than propagation or matching.
  Attach a narrowly scoped
  `#[expect(clippy::unwrap_used, reason = "…")]` or
  `#[expect(clippy::expect_used, reason = "…")]` that states the invariant.
- Every lint suppression must use `#[expect(lint, reason = "…")]`. Never use
  `#[allow(...)]`; fix the underlying issue when an expectation cannot be
  precise and self-validating.
- Code progeny generates is linted under its consumer's configuration. A
  suppression it emits is an `expect` only when progeny alone decides whether
  the lint fires, such as `deprecated` on a use of an item it marked deprecated,
  and an inner `#![allow(lint, reason = "…")]` when the consumer's
  configuration, features or toolchain decide, such as a Clippy threshold lint.

## Changelog

A pull request with a user-facing change (a new feature, a fix users can notice
in generated code, the CLI or the configuration, or a behavior or breaking
change) adds one bullet under `## Unreleased` in `CHANGELOG.md`: a bold one-line
statement, optionally followed by one short sentence naming the config key,
flag or generated item. CI, xtask gates, tests, refactors and dependency bumps
get no entry. A release moves the Unreleased entries under the new `## vX.Y.Z`
heading before the bump, which commits `CHANGELOG.md` through
`.bumpversion.toml`.
