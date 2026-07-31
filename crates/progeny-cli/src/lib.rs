//! A thin shell over the library, for checked-in-output workflows.
//!
//! It adds no logic: read the arguments, read the files, call `generate`, write the files, print
//! the diagnostics, apply the caller's strictness policy. Every decision that affects output lives
//! in the library; every decision about the filesystem lives here. That split is why the library
//! can take bytes and return strings — and why it is a separate package, so that "the library
//! performs no I/O" is a property of its dependency graph rather than a rule a linter has to
//! carve an exception into.
//!
//! Emitting a crate or a whole workspace is *not* what distinguishes this front end from the
//! library: `progeny::Config`'s packaging choice is a library knob, and a build script can ask for
//! a workspace and write it out itself. The only thing that lives here is the filesystem.
//!
//! This crate exists to be shared by the two shipped binaries — `progeny` and `cargo-progeny` —
//! which differ only in how they are invoked. Nothing else is expected to depend on it.

use std::process::ExitCode;

use progeny::{Action, Config, Output};

/// How the front end was reached, which is the only thing the two shipped binaries disagree about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invocation {
    /// The standalone `progeny` binary.
    Standalone,
    /// `cargo progeny`, which cargo execs as `cargo-progeny progeny <arguments>`.
    CargoSubcommand,
}

impl Invocation {
    /// What a reader of the usage text would type to get here.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Standalone => "progeny",
            Self::CargoSubcommand => "cargo progeny",
        }
    }

    /// The usage text, naming this invocation.
    #[must_use]
    pub fn usage(self) -> String {
        indoc::formatdoc! {"
            {name} — generate a Rust client and server from an OpenAPI description

            usage:
              {name} <description> [--config <progeny.toml>] [--out-dir <directory>]

            arguments:
              <description>        the OpenAPI document, JSON or YAML
              --config <path>      a progeny.toml
              --out-dir <path>     where to write the generated files
              -h, --help           this message
              -V, --version        the version that would generate the output

            Exits non-zero when a diagnostic is denied by the configuration, so a build can refuse \
            to proceed on, say, any degradation.",
            name = self.name()
        }
    }
}

/// Run the front end over a process argument list, program name included.
///
/// Takes the whole of [`std::env::args`] rather than a pre-trimmed tail, because which leading
/// elements are the program's own name is exactly what [`Invocation`] knows and a caller
/// should not have to.
#[must_use]
pub fn main(invocation: Invocation, argv: impl Iterator<Item = String>) -> ExitCode {
    match run(invocation, argv) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("progeny: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(invocation: Invocation, argv: impl Iterator<Item = String>) -> Result<ExitCode, String> {
    let arguments = Arguments::parse(invocation, argv)?;
    if arguments.help {
        println!("{}", invocation.usage());
        return Ok(ExitCode::SUCCESS);
    }
    // Worth its own flag because the intended workflow checks generated source in: the reviewer of
    // a diff needs to know which progeny produced it, and the file itself does not say. It reports
    // the *library's* version rather than this package's, which are the same number only for as
    // long as the two are published together.
    if arguments.version {
        println!("progeny {}", progeny::VERSION);
        return Ok(ExitCode::SUCCESS);
    }

    let Some(spec) = &arguments.spec else {
        return Err(indoc::formatdoc! {"
            no description given

            {}",
            invocation.usage()
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

/// The parsed command line.
///
/// Hand-rolled rather than derived: the whole binary is a shell, and an argument parser would be
/// the largest thing in it.
#[derive(Debug, Default)]
pub struct Arguments {
    /// The OpenAPI document to read.
    pub spec: Option<String>,
    /// A `progeny.toml` to configure generation with.
    pub config: Option<String>,
    /// Where to write the generated files.
    pub out_dir: Option<String>,
    /// Print the usage text and stop.
    pub help: bool,
    /// Print the generator's version and stop.
    pub version: bool,
}

impl Arguments {
    /// Parse a process argument list, program name included.
    ///
    /// # Errors
    ///
    /// Returns the usage text when an option is unknown, is missing its value, or a second
    /// positional argument appears.
    pub fn parse(
        invocation: Invocation,
        argv: impl Iterator<Item = String>,
    ) -> Result<Self, String> {
        let mut arguments = argv.skip(1).peekable();
        // cargo execs `cargo progeny <arguments>` as `cargo-progeny progeny <arguments>`, so the
        // subcommand name arrives again as the first argument. Running the same binary directly
        // does not repeat it, which is why this peeks rather than skipping unconditionally.
        if invocation == Invocation::CargoSubcommand
            && arguments.peek().is_some_and(|first| first == "progeny")
        {
            arguments.next();
        }

        let mut parsed = Self::default();
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

                        {}",
                        invocation.usage()
                    });
                }
                other if parsed.spec.is_none() => parsed.spec = Some(other.to_owned()),
                other => {
                    return Err(indoc::formatdoc! {"
                        unexpected argument {other}

                        {}",
                        invocation.usage()
                    });
                }
            }
        }
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::{Arguments, Invocation};
    use color_eyre::eyre;

    fn parse(arguments: &[&str]) -> Result<Arguments, String> {
        Arguments::parse(
            Invocation::Standalone,
            std::iter::once("progeny".to_owned())
                .chain(arguments.iter().map(|argument| (*argument).to_owned())),
        )
    }

    fn parse_subcommand(arguments: &[&str]) -> Result<Arguments, String> {
        Arguments::parse(
            Invocation::CargoSubcommand,
            std::iter::once("cargo-progeny".to_owned())
                .chain(arguments.iter().map(|argument| (*argument).to_owned())),
        )
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

    /// The subcommand name cargo repeats is not a description.
    ///
    /// `cargo progeny api.yaml` reaches the binary as `cargo-progeny progeny api.yaml`. Without
    /// the strip, `progeny` would be taken as the positional and `api.yaml` would be rejected as
    /// a second one — so the subcommand would refuse every invocation that had a document in it.
    #[test_util::test]
    fn the_repeated_subcommand_name_is_not_taken_as_the_description() {
        let parsed = parse_subcommand(&["progeny", "api.yaml"]).map_err(eyre::Report::msg)?;
        assert_eq!(parsed.spec.as_deref(), Some("api.yaml"));
    }

    /// Running the binary under its own name does not repeat it, so the strip has to be
    /// conditional or `cargo-progeny api.yaml` would lose its only argument.
    #[test_util::test]
    fn a_direct_invocation_keeps_its_first_argument() {
        let parsed = parse_subcommand(&["api.yaml"]).map_err(eyre::Report::msg)?;
        assert_eq!(parsed.spec.as_deref(), Some("api.yaml"));
    }

    #[test_util::test]
    fn the_usage_text_names_the_invocation_a_reader_would_type() {
        assert!(
            Invocation::Standalone
                .usage()
                .contains("\n  progeny <description>")
        );
        assert!(
            Invocation::CargoSubcommand
                .usage()
                .contains("\n  cargo progeny <description>")
        );
    }
}
