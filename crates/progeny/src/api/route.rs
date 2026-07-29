//! Path templates, and whether an operation can be called at all.
//!
//! A template is parsed into segments once, here, rather than re-split by each renderer: the
//! client fills the variables in to build a URL and the server registers them with a router, and
//! the two have to agree about where the variables are or they describe different routes.
//!
//! A segment is a *sequence* of literal text and variables rather than one or the other, because
//! real documents write `/Videos/{itemId}/stream.{container}` — `jellyfin` has six such paths, and
//! refusing them would delete working operations for no safety gained. Filling one is the same
//! rule as filling a whole segment, applied piece by piece.
//!
//! The check this performs is narrower than the registrability classifier
//! ([03](../../../plan/03-api-model.md)) will be at stage 7, and deliberately so. Whether two
//! routes *collide* is a question about a router, and there is no router yet; whether a template
//! can be *filled* is a question about the operation alone, and it is already load-bearing —
//! a `{petId}` that no path parameter declares leaves the client with a URL it cannot build.

use std::fmt;

/// A parsed path template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathTemplate {
    segments: Vec<Segment>,
    /// The template as written, for diagnostics and for the docs the client renders.
    literal: String,
}

impl PathTemplate {
    pub(crate) fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// The variables the template names, in order.
    pub(crate) fn variables(&self) -> impl Iterator<Item = &str> {
        self.segments
            .iter()
            .flat_map(|segment| &segment.0)
            .filter_map(|piece| match piece {
                Piece::Variable(name) => Some(name.as_str()),
                Piece::Literal(_) => None,
            })
    }
}

impl fmt::Display for PathTemplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.literal)
    }
}

/// One piece of a template between slashes, as the sequence it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Segment(Vec<Piece>);

impl Segment {
    pub(crate) fn pieces(&self) -> &[Piece] {
        &self.0
    }
}

/// Literal text, or a variable to fill in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Piece {
    Literal(String),
    Variable(String),
}

/// Why a template cannot be filled.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{detail}")]
pub(crate) struct Malformed {
    pub(crate) detail: String,
}

/// Parse a template, or say why it is not one.
pub(crate) fn parse(template: &str) -> Result<PathTemplate, Malformed> {
    if !template.starts_with('/') {
        return Err(Malformed {
            detail: format!("the path `{template}` does not start with `/`"),
        });
    }
    let mut segments = Vec::new();
    for raw in template.split('/').skip(1) {
        segments.push(Segment(pieces(raw, template)?));
    }

    let mut seen: Vec<&str> = Vec::new();
    for name in segments
        .iter()
        .flat_map(|segment| &segment.0)
        .filter_map(|piece| match piece {
            Piece::Variable(name) => Some(name.as_str()),
            Piece::Literal(_) => None,
        })
    {
        if seen.contains(&name) {
            return Err(Malformed {
                detail: format!(
                    "`{template}` names `{name}` twice, so one parameter would have to fill two \
                     positions"
                ),
            });
        }
        seen.push(name);
    }

    Ok(PathTemplate {
        segments,
        literal: template.to_owned(),
    })
}

/// One segment as its alternating literal and variable pieces.
fn pieces(raw: &str, template: &str) -> Result<Vec<Piece>, Malformed> {
    let mut out = Vec::new();
    let mut rest = raw;
    while let Some(open) = rest.find('{') {
        let (before, tail) = rest.split_at(open);
        if !before.is_empty() {
            out.push(Piece::Literal(before.to_owned()));
        }
        let Some(close) = tail.find('}') else {
            return Err(Malformed {
                detail: format!("the segment `{raw}` of `{template}` has unbalanced braces"),
            });
        };
        let name = tail.get(1..close).unwrap_or_default();
        if name.is_empty() {
            return Err(Malformed {
                detail: format!("the segment `{raw}` of `{template}` names no variable"),
            });
        }
        if name.contains('{') {
            return Err(Malformed {
                detail: format!("the segment `{raw}` of `{template}` has unbalanced braces"),
            });
        }
        out.push(Piece::Variable(name.to_owned()));
        rest = tail.get(close + 1..).unwrap_or_default();
    }
    if rest.contains('}') {
        return Err(Malformed {
            detail: format!("the segment `{raw}` of `{template}` has unbalanced braces"),
        });
    }
    if !rest.is_empty() || out.is_empty() {
        out.push(Piece::Literal(rest.to_owned()));
    }
    Ok(out)
}

/// Which of a template's variables no path parameter declares.
pub(crate) fn unbound<'a>(template: &'a PathTemplate, declared: &[String]) -> Vec<&'a str> {
    template
        .variables()
        .filter(|name| !declared.iter().any(|given| given == name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Piece, parse, unbound};
    use color_eyre::eyre::{self, OptionExt as _};

    fn shape(template: &str) -> eyre::Result<Vec<Vec<Piece>>> {
        Ok(parse(template)?
            .segments()
            .iter()
            .map(|segment| segment.pieces().to_vec())
            .collect())
    }

    fn literal(text: &str) -> Piece {
        Piece::Literal(text.to_owned())
    }

    fn variable(name: &str) -> Piece {
        Piece::Variable(name.to_owned())
    }

    #[test_util::test]
    fn a_template_becomes_segments() {
        assert_eq!(
            shape("/pets/{petId}/toys")?,
            [
                vec![literal("pets")],
                vec![variable("petId")],
                vec![literal("toys")],
            ]
        );
        let template = parse("/pets/{petId}/toys")?;
        assert_eq!(template.variables().collect::<Vec<_>>(), ["petId"]);
    }

    #[test_util::test]
    fn a_segment_may_mix_literal_text_and_variables() {
        // `jellyfin` writes six of these. Filling one is the same rule applied piece by piece, and
        // refusing them would delete working operations for no safety gained.
        assert_eq!(
            shape("/Videos/{itemId}/stream.{container}")?,
            [
                vec![literal("Videos")],
                vec![variable("itemId")],
                vec![literal("stream."), variable("container")],
            ]
        );
        assert_eq!(
            shape("/Videos/{itemId}/Trickplay/{width}/{index}.jpg")?,
            [
                vec![literal("Videos")],
                vec![variable("itemId")],
                vec![literal("Trickplay")],
                vec![variable("width")],
                vec![variable("index"), literal(".jpg")],
            ]
        );
        let template = parse("/Videos/{itemId}/stream.{container}")?;
        assert_eq!(
            template.variables().collect::<Vec<_>>(),
            ["itemId", "container"]
        );
    }

    #[test_util::test]
    fn the_root_path_is_a_template_with_one_empty_segment() {
        assert_eq!(shape("/")?, [vec![literal("")]]);
        assert_eq!(parse("/")?.variables().count(), 0);
    }

    #[test_util::test]
    fn a_template_progeny_cannot_fill_is_refused_rather_than_approximated() {
        for (template, expected) in [
            ("pets", "does not start with"),
            ("/pets/{petId", "unbalanced braces"),
            ("/pets/petId}", "unbalanced braces"),
            ("/pets/{}", "names no variable"),
            ("/pets/{{id}}", "unbalanced braces"),
            ("/pets/{id}/toys/{id}", "twice"),
        ] {
            let error = parse(template)
                .err()
                .ok_or_eyre("the test expects this operation to fail")?;
            assert!(error.detail.contains(expected), "{template}: {error:?}");
        }
    }

    #[test_util::test]
    fn a_variable_no_parameter_declares_is_reported_by_name() {
        let template = parse("/pets/{petId}/toys/{toyId}")?;
        assert_eq!(unbound(&template, &["petId".to_owned()]), ["toyId"]);
        assert!(unbound(&template, &["petId".to_owned(), "toyId".to_owned()]).is_empty());
    }
}
