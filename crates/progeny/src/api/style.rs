//! The parameter style table, as a fallible classifier.
//!
//! OpenAPI defines serialization for a *subset* of (location, style, explode, shape)
//! combinations and leaves the rest undefined. A generator that picked something reasonable for
//! the undefined ones would be inventing wire format, which is the forbidden failure mode aimed at
//! the request line, so this refuses instead and the tolerance policy decides what that costs:
//! an optional parameter is omitted, a required one takes its operation with it.
//!
//! *Enforcement:* [`StyleContract`] has no public constructor. A renderer cannot be handed a
//! (location, style) pair that was never classified, because there is no way to build one.

use crate::diag::{Action, BreakageClass, Ctx, Diagnostic, JsonPointer};
use crate::doc::Parameter;

/// Where a parameter rides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Location {
    Path,
    Query,
    Header,
    Cookie,
}

impl Location {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "path" => Some(Self::Path),
            "query" => Some(Self::Query),
            "header" => Some(Self::Header),
            "cookie" => Some(Self::Cookie),
            _ => None,
        }
    }

    /// The style a location uses when the document names none.
    fn default_style(self) -> Style {
        match self {
            Self::Query | Self::Cookie => Style::Form,
            Self::Path | Self::Header => Style::Simple,
        }
    }

    pub(crate) fn slug(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Query => "query",
            Self::Header => "header",
            Self::Cookie => "cookie",
        }
    }
}

/// A serialization style OpenAPI names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Style {
    Form,
    Simple,
    Label,
    Matrix,
    SpaceDelimited,
    PipeDelimited,
    DeepObject,
}

impl Style {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "form" => Some(Self::Form),
            "simple" => Some(Self::Simple),
            "label" => Some(Self::Label),
            "matrix" => Some(Self::Matrix),
            "spaceDelimited" => Some(Self::SpaceDelimited),
            "pipeDelimited" => Some(Self::PipeDelimited),
            "deepObject" => Some(Self::DeepObject),
            _ => None,
        }
    }

    /// Whether `explode` defaults on for this style.
    fn explodes_by_default(self) -> bool {
        self == Self::Form
    }
}

/// The three shapes a parameter value can have, as far as serialization cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ParamShape {
    Primitive,
    Array,
    Object,
}

/// A (location, style, explode, shape) combination OpenAPI defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct StyleContract {
    location: Location,
    style: Style,
    explode: bool,
    shape: ParamShape,
}

impl StyleContract {
    pub(crate) fn location(self) -> Location {
        self.location
    }

    pub(crate) fn style(self) -> Style {
        self.style
    }

    pub(crate) fn explode(self) -> bool {
        self.explode
    }
}

/// Why a parameter has no defined serialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Undefined {
    pub(crate) detail: String,
}

/// Classify one parameter, or say why it cannot be.
pub(crate) fn classify(
    parameter: &Parameter,
    shape: ParamShape,
) -> Result<StyleContract, Undefined> {
    if parameter
        .name
        .as_deref()
        .unwrap_or_default()
        .trim()
        .is_empty()
    {
        return Err(Undefined {
            detail: "the parameter declares no `name`, so nothing says what to call it on the wire"
                .to_owned(),
        });
    }
    let Some(written) = parameter.location.as_deref() else {
        return Err(Undefined {
            detail: "the parameter declares no `in`, so there is nowhere to put it".to_owned(),
        });
    };
    let Some(location) = Location::parse(written) else {
        return Err(Undefined {
            detail: format!(
                "`in: {written}` is not one of the four locations OpenAPI defines, so there is no \
                 rule for how the value reaches the server"
            ),
        });
    };

    let style = match parameter.style.as_deref() {
        None => location.default_style(),
        Some(written) => match Style::parse(written) {
            Some(style) => style,
            None => {
                return Err(Undefined {
                    detail: format!("`style: {written}` is not a style OpenAPI defines"),
                });
            }
        },
    };
    let explode = parameter
        .explode
        .unwrap_or_else(|| style.explodes_by_default());

    if !defined(location, style, shape) {
        return Err(Undefined {
            detail: format!(
                "OpenAPI leaves `style: {}` on {} parameters undefined for this value's shape",
                name(style),
                location.slug()
            ),
        });
    }
    Ok(StyleContract {
        location,
        style,
        explode,
        shape,
    })
}

/// Classify one member of an `application/x-www-form-urlencoded` body's `encoding`.
///
/// A form body is a query string in the body position, so its members answer to the query row of
/// the same table — which is why this asks that table rather than carrying rules of its own. The
/// value's shape is unknown here (the member's type is a fact about the body type, not about the
/// encoding entry), so the shape is not consulted: what an `encoding` can say is `style` and
/// `explode`, and a style the query location does not define at all is refused.
pub(crate) fn form_member(
    encoding: &crate::doc::Encoding,
    at: &JsonPointer,
    ctx: &mut Ctx,
) -> Option<(Style, bool)> {
    let style = match encoding.style.as_deref() {
        None => Style::Form,
        Some(written) => {
            let Some(style) = Style::parse(written) else {
                ctx.report(Diagnostic::new(
                    BreakageClass::QuerySerializationStyle,
                    Action::Degrade,
                    at.clone(),
                    format!(
                        "`style: {written}` is not a style OpenAPI defines; the member is encoded \
                         as exploded `form`, which is what a form body defaults to"
                    ),
                ));
                return None;
            };
            style
        }
    };
    // Every query style is defined for *some* shape; one defined for none would have no reading at
    // all in a form body either.
    if !matches!(
        style,
        Style::Form | Style::SpaceDelimited | Style::PipeDelimited | Style::DeepObject
    ) {
        ctx.report(Diagnostic::new(
            BreakageClass::QuerySerializationStyle,
            Action::Degrade,
            at.clone(),
            format!(
                "OpenAPI defines no `style: {}` for a form body's members; the member is encoded \
                 as exploded `form`",
                name(style)
            ),
        ));
        return None;
    }
    Some((
        style,
        encoding
            .explode
            .unwrap_or_else(|| style.explodes_by_default()),
    ))
}

/// The table itself: which (location, style, shape) triples OpenAPI defines.
///
/// One arm per location rather than a lookup structure, so that a combination nobody thought about
/// is a missing arm the compiler complains about rather than a lookup that returns nothing and is
/// read as "undefined".
fn defined(location: Location, style: Style, shape: ParamShape) -> bool {
    match location {
        // `label` and `matrix` are defined for every shape; `simple` is not defined for objects
        // in a way that round-trips, but it is universally used for them and the encoding is
        // unambiguous, so it is admitted.
        Location::Path => matches!(style, Style::Simple | Style::Label | Style::Matrix),
        Location::Query => match style {
            Style::Form => true,
            // Both are defined only for arrays: there is no rule for joining an object's members
            // with a space.
            Style::SpaceDelimited | Style::PipeDelimited => shape == ParamShape::Array,
            // Defined for objects alone, and the one style whose array form the specification
            // explicitly declines to define.
            Style::DeepObject => shape == ParamShape::Object,
            Style::Simple | Style::Label | Style::Matrix => false,
        },
        // A header carries no brackets and no repetition rules, so only the flat style applies.
        Location::Header => style == Style::Simple,
        // A cookie is a form-encoded pair inside one header. Exploding it would mean writing
        // several `Cookie` headers for one parameter, which no specification rule covers.
        Location::Cookie => style == Style::Form,
    }
}

fn name(style: Style) -> &'static str {
    match style {
        Style::Form => "form",
        Style::Simple => "simple",
        Style::Label => "label",
        Style::Matrix => "matrix",
        Style::SpaceDelimited => "spaceDelimited",
        Style::PipeDelimited => "pipeDelimited",
        Style::DeepObject => "deepObject",
    }
}

#[cfg(test)]
mod tests {
    use super::{Location, ParamShape, Style, classify};
    use crate::doc::Parameter;

    fn parameter(location: &str, style: Option<&str>, explode: Option<bool>) -> Parameter {
        Parameter {
            name: Some("thing".to_owned()),
            location: Some(location.to_owned()),
            style: style.map(ToOwned::to_owned),
            explode,
            ..Parameter::default()
        }
    }

    #[test]
    fn a_location_supplies_the_style_the_document_left_out() {
        let query = classify(&parameter("query", None, None), ParamShape::Primitive).unwrap();
        assert_eq!(query.style(), Style::Form);
        // `form` is the one style that explodes unless told otherwise.
        assert!(query.explode());

        let path = classify(&parameter("path", None, None), ParamShape::Primitive).unwrap();
        assert_eq!(path.style(), Style::Simple);
        assert!(!path.explode());
        assert_eq!(path.location(), Location::Path);
    }

    #[test]
    fn a_combination_the_specification_leaves_undefined_is_refused_rather_than_guessed() {
        // deepObject over an array: the one combination the specification names and declines.
        let error = classify(
            &parameter("query", Some("deepObject"), None),
            ParamShape::Array,
        )
        .unwrap_err();
        assert!(error.detail.contains("deepObject"), "{error:?}");

        // spaceDelimited over an object: there is no rule for joining members with a space.
        assert!(
            classify(
                &parameter("query", Some("spaceDelimited"), None),
                ParamShape::Object
            )
            .is_err()
        );

        // A path parameter cannot use a query style.
        assert!(
            classify(
                &parameter("path", Some("form"), None),
                ParamShape::Primitive
            )
            .is_err()
        );
    }

    #[test]
    fn a_parameter_with_no_name_has_nothing_to_be_called_on_the_wire() {
        let mut nameless = parameter("query", None, None);
        nameless.name = None;
        let error = classify(&nameless, ParamShape::Primitive).unwrap_err();
        assert!(error.detail.contains("no `name`"), "{error:?}");
    }

    #[test]
    fn a_location_openapi_does_not_define_is_refused_by_name() {
        // Swagger 2.0's `in: body`, which real 3.x documents still carry.
        let error = classify(&parameter("body", None, None), ParamShape::Object).unwrap_err();
        assert!(error.detail.contains("in: body"), "{error:?}");
        assert!(error.detail.contains("four locations"), "{error:?}");
    }

    #[test]
    fn every_location_defines_at_least_its_own_default_style() {
        for location in ["path", "query", "header", "cookie"] {
            assert!(
                classify(&parameter(location, None, None), ParamShape::Primitive).is_ok(),
                "{location}"
            );
        }
    }
}
