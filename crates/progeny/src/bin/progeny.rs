//! A thin shell over the library, for checked-in-output workflows.
//!
//! It adds no logic: read the arguments, read the files, call `generate`, write the files, print
//! the diagnostics, apply the caller's strictness policy. Every decision that affects output lives
//! in the library; every decision about the filesystem lives here. That split is why the library
//! can take bytes and return strings.

use std::process::ExitCode;

use progeny::{Action, Config, Output};

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("progeny: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, String> {
    let arguments = Arguments::parse(std::env::args().skip(1))?;
    if arguments.help {
        println!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    // Worth its own flag because the intended workflow checks generated source in: the reviewer of
    // a diff needs to know which progeny produced it, and the file itself does not say.
    if arguments.version {
        println!("progeny {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }

    let Some(spec) = &arguments.spec else {
        return Err(indoc::formatdoc! {"
            no description given

            {USAGE}"
        });
    };

    let input =
        std::fs::read(spec).map_err(|error| format!("reading the description {spec}: {error}"))?;

    let config = match &arguments.config {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|error| format!("reading the configuration {path}: {error}"))?;
            toml::from_str::<Config>(&text)
                .map_err(|error| format!("reading the configuration {path}: {error}"))?
        }
        None => Config::default(),
    };

    let output = progeny::generate(&input, &config).map_err(|error| error.to_string())?;
    report(&output);

    if let Some(directory) = &arguments.out_dir {
        write(&output, directory)?;
    } else if !output.files.is_empty() {
        return Err("generated files but no --out-dir to write them to".to_owned());
    }

    let denied: Vec<_> = config.denied(&output.diagnostics).collect();
    if denied.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    eprintln!(
        "{}",
        indoc::formatdoc! {"

            {} diagnostics are denied by the configuration:",
            denied.len()
        }
    );
    for diagnostic in denied {
        eprintln!("  {diagnostic}");
    }
    Ok(ExitCode::FAILURE)
}

fn report(output: &Output) {
    if output.diagnostics.is_empty() {
        println!("progeny: nothing to report; the description needed no repair");
        return;
    }
    let mut repairs = 0usize;
    let mut degradations = 0usize;
    let mut warnings = 0usize;
    for diagnostic in &output.diagnostics {
        match diagnostic.action() {
            Action::Repair => repairs += 1,
            Action::Degrade => degradations += 1,
            Action::Warn => warnings += 1,
        }
        println!("{diagnostic}");
    }
    println!("progeny: {repairs} repaired, {degradations} degraded, {warnings} worth a look");
}

fn write(output: &Output, directory: &str) -> Result<(), String> {
    for (path, contents) in &output.files {
        let target = std::path::Path::new(directory).join(path.as_str());
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("creating {}: {error}", parent.display()))?;
        }
        std::fs::write(&target, contents)
            .map_err(|error| format!("writing {}: {error}", target.display()))?;
    }
    println!("progeny: wrote {} files to {directory}", output.files.len());
    Ok(())
}

const USAGE: &str = indoc::indoc! {"
    progeny — generate a Rust client and server from an OpenAPI description

    usage:
      progeny <description> [--config <progeny.toml>] [--out-dir <directory>]

    arguments:
      <description>        the OpenAPI document, JSON or YAML
      --config <path>      a progeny.toml
      --out-dir <path>     where to write the generated files
      -h, --help           this message
      -V, --version        the version that would generate the output

    Exits non-zero when a diagnostic is denied by the configuration, so a build can refuse to \
    proceed on, say, any degradation."
};

/// The parsed command line.
///
/// Hand-rolled rather than derived: the whole binary is a shell, and an argument parser would be
/// the largest thing in it.
#[derive(Debug, Default)]
struct Arguments {
    spec: Option<String>,
    config: Option<String>,
    out_dir: Option<String>,
    help: bool,
    version: bool,
}

impl Arguments {
    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut parsed = Self::default();
        let mut arguments = arguments.peekable();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "-h" | "--help" => parsed.help = true,
                "-V" | "--version" => parsed.version = true,
                "--config" => {
                    parsed.config = Some(
                        arguments
                            .next()
                            .ok_or_else(|| "--config needs a path".to_owned())?,
                    );
                }
                "--out-dir" => {
                    parsed.out_dir = Some(
                        arguments
                            .next()
                            .ok_or_else(|| "--out-dir needs a path".to_owned())?,
                    );
                }
                other if other.starts_with('-') => {
                    return Err(indoc::formatdoc! {"
                        unknown option {other}

                        {USAGE}"
                    });
                }
                other if parsed.spec.is_none() => parsed.spec = Some(other.to_owned()),
                other => {
                    return Err(indoc::formatdoc! {"
                        unexpected argument {other}

                        {USAGE}"
                    });
                }
            }
        }
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::Arguments;
    use color_eyre::eyre;

    fn parse(arguments: &[&str]) -> Result<Arguments, String> {
        Arguments::parse(arguments.iter().map(|argument| (*argument).to_owned()))
    }

    #[test_util::test]
    fn a_description_and_its_options_are_read() {
        let parsed = parse(&["api.yaml", "--config", "progeny.toml", "--out-dir", "out"])
            .map_err(eyre::Report::msg)?;
        assert_eq!(parsed.spec.as_deref(), Some("api.yaml"));
        assert_eq!(parsed.config.as_deref(), Some("progeny.toml"));
        assert_eq!(parsed.out_dir.as_deref(), Some("out"));
        assert!(!parsed.help);
    }

    #[test_util::test]
    fn an_option_missing_its_value_is_an_error_rather_than_a_default() {
        assert!(parse(&["api.yaml", "--config"]).is_err());
        assert!(parse(&["api.yaml", "--out-dir"]).is_err());
    }

    #[test_util::test]
    fn unknown_options_and_extra_arguments_are_refused() {
        assert!(parse(&["--wat"]).is_err());
        assert!(parse(&["a.yaml", "b.yaml"]).is_err());
    }

    #[test_util::test]
    fn help_needs_no_description() {
        assert!(parse(&["--help"]).map_err(eyre::Report::msg)?.help);
    }

    #[test_util::test]
    fn version_needs_no_description_either() {
        assert!(parse(&["--version"]).map_err(eyre::Report::msg)?.version);
        assert!(parse(&["-V"]).map_err(eyre::Report::msg)?.version);
    }
}
