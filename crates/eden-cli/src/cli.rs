//! Declarative command surface of the `eden` executable.
//!
//! Every option and every action is declared once, here. Help text, usage
//! lines and usage errors are clap's rendering of this tree, and each family
//! reads typed arguments instead of rescanning one shared string vector, so an
//! option cannot drift between the families that accept it.
use crate::style;
use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{
    Arg, ArgMatches, Args, ColorChoice, CommandFactory, FromArgMatches, Parser, Subcommand,
};
use std::ffi::OsString;
use std::path::PathBuf;

/// Which content an attachment flag embeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentKind {
    /// UTF-8 text embedded as a text block.
    Text,
    /// An image embedded as base64 data.
    Image,
    /// A PDF embedded as base64 data.
    File,
}

/// The parsed command line together with the matches it came from.
#[derive(Debug)]
pub struct Parsed {
    /// Typed view of the command line.
    pub cli: Cli,
    /// Raw matches, kept so attachments can be restored to command-line order.
    pub matches: ArgMatches,
    /// The color choice the command was built with.
    pub color: ColorChoice,
}

#[derive(Debug, Parser)]
#[command(
    name = "eden",
    version,
    about = "A Rust coding agent built from replaceable native plugins",
    disable_help_subcommand = true,
    subcommand_negates_reqs = true,
    args_conflicts_with_subcommands = true,
    override_usage = "eden [OPTIONS] [PROMPT]\n       eden [OPTIONS] <COMMAND> [ARGS]...",
    next_display_order = 800,
    styles = style::styles(),
    after_help = "Run 'eden <command> --help' for more information on a command."
)]
pub struct Cli {
    /// Directory to run in
    #[arg(long, global = true, value_name = "DIR")]
    pub cwd: Option<PathBuf>,
    /// Directory holding settings, trust and installed packages
    #[arg(long, global = true, value_name = "DIR")]
    pub global_dir: Option<PathBuf>,
    /// Trust this project's resources for this run only
    #[arg(long, global = true, conflicts_with = "no_trust_project")]
    pub trust_project: bool,
    /// Ignore this project's resources
    #[arg(long, global = true)]
    pub no_trust_project: bool,
    /// Read a different composition file
    #[arg(long, global = true, value_name = "PATH")]
    pub composition: Option<PathBuf>,
    /// Load environment variables from an explicit file
    #[arg(long, global = true, value_name = "PATH")]
    pub env_file: Vec<PathBuf>,
    /// When to color human-readable output
    #[arg(
        long,
        global = true,
        value_name = "WHEN",
        default_value = "auto",
        value_parser = ["auto", "always", "never"]
    )]
    pub color: String,
    /// Do not print status, warnings or notes
    #[arg(long, short, global = true)]
    pub quiet: bool,
    /// Print more detail; repeatable
    #[arg(long, short, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// Continue a saved session instead of submitting a new prompt
    #[arg(long = "continue", requires = "session", conflicts_with = "prompt")]
    pub continue_session: bool,
    /// Open or create this session history file
    #[arg(long, visible_alias = "resume", value_name = "PATH")]
    pub session: Option<PathBuf>,
    /// Keep the run in memory and write no session file
    #[arg(long, conflicts_with = "session")]
    pub no_session: bool,
    /// Print the records of a history file instead of running a prompt
    #[arg(long, value_name = "PATH")]
    pub history: Option<PathBuf>,
    /// Stream ordered events as JSON lines on stdout
    #[arg(long)]
    pub json: bool,
    /// Print the settled result to stdout
    #[arg(long)]
    pub print: bool,
    /// Enable only the named coding tools
    #[arg(long, value_name = "NAMES", value_delimiter = ',')]
    pub tools: Vec<String>,
    /// Disable the named coding tools
    #[arg(long, value_name = "NAMES", value_delimiter = ',')]
    pub exclude_tools: Vec<String>,
    /// Add a directory of skills
    #[arg(long, value_name = "PATH")]
    pub skill_path: Vec<PathBuf>,
    /// Add a directory of templates
    #[arg(long, value_name = "PATH")]
    pub template_path: Vec<PathBuf>,
    /// Refuse tools that would modify the workspace
    #[arg(long)]
    pub read_only: bool,
    /// Do not discover project context files
    #[arg(long)]
    pub no_context: bool,
    /// Do not discover skills
    #[arg(long)]
    pub no_skills: bool,
    /// Do not discover templates
    #[arg(long)]
    pub no_templates: bool,
    /// Attach a UTF-8 text file to the prompt
    #[arg(long, value_name = "PATH")]
    pub attach: Vec<PathBuf>,
    /// Attach an image to the prompt
    #[arg(long, value_name = "PATH")]
    pub image: Vec<PathBuf>,
    /// Attach a PDF to the prompt
    #[arg(long, value_name = "PATH")]
    pub file: Vec<PathBuf>,
    /// Prompt to submit
    #[arg(value_name = "PROMPT")]
    pub prompt: Option<String>,
    #[command(subcommand)]
    pub family: Option<Family>,
}

#[derive(Debug, Subcommand)]
pub enum Family {
    /// Inspect or change saved project trust
    Trust {
        #[command(subcommand)]
        action: TrustAction,
    },
    /// Print the resources a session would load
    Resources {
        #[command(subcommand)]
        action: Option<ResourcesAction>,
    },
    /// List the commands installed packages contribute
    Commands,
    /// Run one contributed command
    Command {
        /// Contributed command name
        name: String,
        /// JSON arguments for the command
        #[arg(value_name = "JSON", default_value = "{}")]
        arguments: String,
    },
    /// Install, list, remove or resolve native packages
    Package {
        #[command(subcommand)]
        action: PackageAction,
    },
    /// Inspect or export a stored history file
    History {
        /// Stream ordered events as JSON lines
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: HistoryAction,
    },
    /// Inspect or change a stored session
    #[command(
        after_help = concat!(
            "Session commands accept --composition PATH. Copy actions preview by default; ",
            "--apply creates the new file. The default Responses provider requires explicit ",
            "model configuration and credentials.",
        )
    )]
    Session {
        /// Stream ordered events as JSON lines
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: SessionAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum TrustAction {
    /// Trust a project directory
    Allow {
        /// Project directory
        #[arg(value_name = "PATH", default_value = ".")]
        path: PathBuf,
    },
    /// Deny a project directory
    Deny {
        /// Project directory
        #[arg(value_name = "PATH", default_value = ".")]
        path: PathBuf,
    },
    /// Print the workspace a directory produces
    Inspect {
        /// Project directory
        #[arg(value_name = "PATH", default_value = ".")]
        path: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum ResourcesAction {
    /// Print the resource snapshot a session would load
    List,
}

#[derive(Debug, Subcommand)]
pub enum PackageAction {
    /// Install a package from a local directory, archive or --source-json
    Install {
        /// Local package directory or archive
        #[arg(value_name = "SOURCE", required_unless_present = "source_json")]
        source: Option<PathBuf>,
        /// Structured source description as JSON
        #[arg(long, value_name = "JSON")]
        source_json: Option<String>,
        /// Build the package from its own source manifest
        #[arg(long)]
        build: bool,
    },
    /// List installed packages
    List,
    /// Remove one installed package version
    Remove {
        /// Package name
        name: String,
        /// Package version
        version: String,
        /// Remove it even while something still references it
        #[arg(long)]
        force: bool,
    },
    /// Resolve a dependency request from JSON arguments
    Resolve {
        /// Resolution request as JSON
        #[arg(value_name = "JSON")]
        arguments: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum HistoryAction {
    /// Print every stored record as a JSON line
    Inspect {
        /// History file to read
        path: PathBuf,
    },
    /// Copy a history file into a new file
    Export {
        /// History file to read
        path: PathBuf,
        /// File to create
        destination: PathBuf,
    },
}

/// Arguments shared by every copy action of the session family.
#[derive(Debug, Args)]
pub struct CopyArgs {
    /// Existing history file
    pub source: PathBuf,
    /// New history file to create
    pub destination: PathBuf,
    /// Node id to copy from
    #[arg(long, value_name = "NODE")]
    pub at: Option<u64>,
    /// Leave installation-private records out of the copy
    #[arg(long)]
    pub public_only: bool,
    /// Write the new file; without it the plan is only previewed
    #[arg(long)]
    pub apply: bool,
}

#[derive(Debug, Subcommand)]
pub enum SessionAction {
    /// Print session identity, head and branch state
    Info {
        /// Session history file
        path: PathBuf,
    },
    /// Print every record as one node line
    Tree {
        /// Session history file
        path: PathBuf,
    },
    /// Rebind a session to the selected composition
    Switch {
        /// Session history file
        path: PathBuf,
    },
    /// Print the resources a session loads
    Resources {
        /// Session history file
        path: PathBuf,
    },
    /// Print queued submissions
    Queue {
        /// Session history file
        path: PathBuf,
    },
    /// Append a queued submission
    Enqueue {
        /// Session history file
        path: PathBuf,
        /// Text to queue
        text: String,
        /// Queue kind recorded with the submission
        #[arg(long, default_value = "follow_up")]
        kind: String,
    },
    /// Choose how steering and follow-up submissions are handled
    QueueMode {
        /// Session history file
        path: PathBuf,
        /// Mode for steering submissions
        #[arg(long, default_value = "one")]
        steering: String,
        /// Mode for follow-up submissions
        #[arg(long, default_value = "one")]
        follow_up: String,
    },
    /// Attach a recorded attachment to the head of the session
    IncludeAttachment {
        /// Session history file
        path: PathBuf,
        /// Node id that recorded the attachment
        #[arg(long, value_name = "NODE")]
        at: u64,
    },
    /// Compact the transcript so far
    Compact {
        /// Session history file
        path: PathBuf,
        /// Instructions for the summary request
        #[arg(long, value_name = "TEXT")]
        instructions: Option<String>,
    },
    /// Continue the session from its current head
    Continue {
        /// Session history file
        path: PathBuf,
    },
    /// Move the head to a new branch
    Branch {
        /// Session history file
        path: PathBuf,
        /// Node id the new branch departs from
        #[arg(long, value_name = "NODE")]
        at: u64,
        /// Name of the new branch
        #[arg(long, value_name = "NAME")]
        branch: String,
        /// Summarize the departed transcript
        #[arg(long)]
        summarize: bool,
    },
    /// Save session metadata
    Metadata {
        /// Session history file
        path: PathBuf,
        /// Session name
        #[arg(long, value_name = "NAME")]
        name: String,
        /// Metadata tag; repeat for more than one
        #[arg(long, value_name = "TAG")]
        tag: Vec<String>,
    },
    /// Preview or create an independent copy that keeps the whole tree
    Fork {
        #[command(flatten)]
        copy: CopyArgs,
    },
    /// Preview or create an independent copy that keeps the whole tree
    Clone {
        #[command(flatten)]
        copy: CopyArgs,
    },
    /// Preview or create a copy from an outside history file
    Import {
        #[command(flatten)]
        copy: CopyArgs,
    },
    /// Preview or create a copy upgraded to the current schema
    Upgrade {
        #[command(flatten)]
        copy: CopyArgs,
    },
    /// Preview or create a copy recovered from a damaged tail
    Recover {
        #[command(flatten)]
        copy: CopyArgs,
    },
    /// Preview or create a copy migrated to the current layout
    Migrate {
        #[command(flatten)]
        copy: CopyArgs,
    },
}

/// What the probe pass reads before the authoritative parse.
#[derive(Clone, Debug)]
pub struct Startup {
    /// The explicit environment file, if the command line names one.
    pub env_file: Option<PathBuf>,
    /// When to color clap's own help, version and usage errors.
    pub color: ColorChoice,
}

/// Read the arguments that have to take effect before the authoritative parse.
///
/// `--env-file` is deliberately not an ordinary option: its values are
/// installed before the parse, so a file that cannot be read is reported
/// instead of any later argument problem. `--color` has to be known before the
/// command is built, because clap renders its own help and usage errors with
/// the choice the command carries. The probe is the same command with errors
/// ignored, which is also what keeps "which option takes a value" as clap's
/// knowledge rather than a second hand-written list.
pub fn probe(args: &[OsString]) -> Result<Startup, clap::Error> {
    let matches = probe_command().try_get_matches_from(args)?;
    let mut files = matches
        .get_many::<PathBuf>("env_file")
        .into_iter()
        .flatten()
        .cloned();
    let env_file = files.next();
    if files.next().is_some() {
        return Err(Cli::command().error(
            ErrorKind::TooManyValues,
            "the argument '--env-file <PATH>' cannot be used multiple times",
        ));
    }
    let color = match matches.get_one::<String>("color").map(String::as_str) {
        Some("always") => ColorChoice::Always,
        Some("never") => ColorChoice::Never,
        _ => ColorChoice::Auto,
    };
    Ok(Startup { env_file, color })
}

/// Parse the process arguments, or print clap's help, version or usage error.
pub fn parse(args: &[OsString], color: ColorChoice) -> Parsed {
    let command = Cli::command().color(color);
    let matches = match command.try_get_matches_from(args) {
        Ok(matches) => matches,
        Err(error) => subcommand_hint(args, color, error).exit(),
    };
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    Parsed {
        cli,
        matches,
        color,
    }
}

/// The family names the root command accepts.
const FAMILIES: [&str; 7] = [
    "trust",
    "resources",
    "commands",
    "command",
    "package",
    "history",
    "session",
];

/// Offer the closest family when the first word is a near miss for one.
///
/// clap cannot suggest this on its own: the root also has a `PROMPT`
/// positional, so a misspelled family is consumed as that positional and the
/// failure surfaces as a conflicting second argument instead. The word is read
/// back from the probe pass, and the hint only replaces an error that is
/// already an error, so no accepted command changes behaviour.
fn subcommand_hint(args: &[OsString], color: ColorChoice, error: clap::Error) -> clap::Error {
    if error.kind() == ErrorKind::InvalidSubcommand {
        return error;
    }
    let Ok(matches) = probe_command().color(color).try_get_matches_from(args) else {
        return error;
    };
    let Some(word) = matches.get_one::<String>("prompt") else {
        return error;
    };
    let candidate = FAMILIES
        .iter()
        .copied()
        .filter(|family| *family != word.as_str())
        .map(|family| (distance(word, family), family))
        .filter(|(distance, _)| *distance <= 3)
        .min_by_key(|(distance, _)| *distance);
    let Some((_, closest)) = candidate else {
        return error;
    };
    // Build the error the way clap builds its own subcommand error: no message
    // of its own, only the contexts its formatter renders, so the suggestion
    // and the usage line come out in the usual shape.
    let mut command = Cli::command();
    let usage = command.render_usage();
    let mut hinted = clap::Error::new(ErrorKind::InvalidSubcommand).with_cmd(&command);
    hinted.insert(
        ContextKind::InvalidSubcommand,
        ContextValue::String(word.clone()),
    );
    hinted.insert(
        ContextKind::SuggestedSubcommand,
        ContextValue::Strings(vec![closest.to_owned()]),
    );
    hinted.insert(ContextKind::Usage, ContextValue::StyledStr(usage));
    hinted
}

/// Levenshtein distance, used only to decide whether a word is a near miss.
fn distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (row, character) in left.chars().enumerate() {
        let mut current = vec![row + 1];
        for (column, other) in right.iter().enumerate() {
            let substitute = previous[column] + usize::from(character != *other);
            current.push(
                substitute
                    .min(previous[column + 1] + 1)
                    .min(current[column] + 1),
            );
        }
        previous = current;
    }
    previous[right.len()]
}

/// The probe command: the real tree with every non-parse outcome neutralized.
///
/// `--help` and `--version` are display requests rather than parse errors
/// that `ignore_errors` could absorb, so the probe replaces those flags with
/// hidden accepted ones. An explicit environment file is validated before the
/// process may print help or a version.
fn probe_command() -> clap::Command {
    Cli::command()
        .ignore_errors(true)
        .disable_help_flag(true)
        .disable_version_flag(true)
        .arg(
            Arg::new("probe-help")
                .short('h')
                .long("help")
                .action(clap::ArgAction::SetTrue)
                .global(true)
                .hide(true),
        )
        .arg(
            Arg::new("probe-version")
                .short('V')
                .long("version")
                .action(clap::ArgAction::SetTrue)
                .global(true)
                .hide(true),
        )
}

/// Report a startup failure in clap's own usage-error shape and exit.
pub fn fail(message: impl Into<String>) -> ! {
    Cli::command()
        .error(ErrorKind::ValueValidation, message.into())
        .exit()
}

/// Attachments in the order they were written on the command line.
///
/// clap keeps one vector per flag, so the three flags are merged back by the
/// argument index clap recorded for each value.
pub fn attachments(parsed: &Parsed) -> Vec<(AttachmentKind, PathBuf)> {
    let groups = [
        (AttachmentKind::Text, "attach", &parsed.cli.attach),
        (AttachmentKind::Image, "image", &parsed.cli.image),
        (AttachmentKind::File, "file", &parsed.cli.file),
    ];
    let mut ordered = vec![];
    for (kind, id, values) in groups {
        let indices: Vec<usize> = parsed
            .matches
            .indices_of(id)
            .map(|indices| indices.collect())
            .unwrap_or_default();
        for (offset, value) in values.iter().enumerate() {
            let index = indices.get(offset).copied().unwrap_or(usize::MAX);
            ordered.push((index, kind, value.clone()));
        }
    }
    ordered.sort_by_key(|(index, _, _)| *index);
    ordered
        .into_iter()
        .map(|(_, kind, value)| (kind, value))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// clap reads the program name from the first element, as a process does.
    fn command_line(args: &[&str]) -> Vec<OsString> {
        let mut line = vec![OsString::from("eden")];
        line.extend(args.iter().map(OsString::from));
        line
    }

    fn env_file(args: &[&str]) -> Option<PathBuf> {
        startup(args).env_file
    }

    fn startup(args: &[&str]) -> Startup {
        probe(&command_line(args)).unwrap()
    }

    #[test]
    fn a_value_that_looks_like_an_option_is_not_the_environment_file() {
        assert_eq!(
            env_file(&["--env-file", "a.env", "--version"]),
            Some("a.env".into())
        );
        // `--env-file` here is the value of `--cwd`, which only clap's own
        // knowledge of which options take a value can tell.
        assert_eq!(env_file(&["--cwd", "--env-file", "--version"]), None);
        assert_eq!(env_file(&["--version"]), None);
    }

    #[test]
    fn a_repeated_environment_file_is_rejected() {
        assert!(probe(&command_line(&["--env-file", "a", "--env-file", "b"])).is_err());
    }

    #[test]
    fn a_missing_environment_file_value_is_not_a_parse_failure_for_the_probe() {
        assert_eq!(env_file(&["--env-file"]), None);
    }

    #[test]
    fn the_color_choice_comes_from_the_probe() {
        let choices = ["auto", "always", "never"].map(|when| {
            let color = startup(&["--color", when, "prompt"]).color;
            (when, color)
        });
        assert!(matches!(choices[0].1, ColorChoice::Auto), "{choices:?}");
        assert!(matches!(choices[1].1, ColorChoice::Always), "{choices:?}");
        assert!(matches!(choices[2].1, ColorChoice::Never), "{choices:?}");
    }

    #[test]
    fn distance_separates_a_near_miss_from_an_unrelated_word() {
        assert!(distance("resorces", "resources") <= 3);
        assert!(distance("prompt", "session") > 3);
        assert_eq!(distance("session", "session"), 0);
    }
}
