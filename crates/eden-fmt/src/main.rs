//! `eden-fmt` command line interface.

use eden_fmt::engine::{Mode, Options, collect, run};
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match execute(&arguments) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("eden-fmt: {message}");
            ExitCode::from(2)
        }
    }
}

fn execute(arguments: &[String]) -> Result<ExitCode, String> {
    let mut mode: Option<Mode> = None;
    let mut options = Options::default();
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut skips: Vec<PathBuf> = Vec::new();
    let mut at = 0;
    while at < arguments.len() {
        let argument = arguments[at].as_str();
        match argument {
            "check" => mode = Some(Mode::Check),
            "write" => mode = Some(Mode::Write),
            "stdin" => mode = Some(Mode::Stdin),
            "--edition" => {
                options.edition = value(arguments, &mut at)?;
            }
            "--style-edition" => {
                options.style_edition = value(arguments, &mut at)?;
            }
            "--max-width" => {
                options.max_width = value(arguments, &mut at)?
                    .parse()
                    .map_err(|_| "--max-width takes a number".to_string())?;
            }
            "--jobs" => {
                options.jobs = value(arguments, &mut at)?
                    .parse()
                    .map_err(|_| "--jobs takes a number".to_string())?;
            }
            "--rustfmt" => {
                options.rustfmt = value(arguments, &mut at)?;
            }
            "--no-verify" => options.verify = false,
            "--skip" => skips.push(PathBuf::from(value(arguments, &mut at)?)),
            other if other.starts_with("--") => return Err(format!("unknown option `{other}`")),
            other => paths.push(PathBuf::from(other)),
        }
        at += 1;
    }
    let mode = mode.ok_or_else(|| "expected `check`, `write` or `stdin`".to_string())?;
    if matches!(mode, Mode::Stdin) {
        let mut source = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut source)
            .map_err(|error| error.to_string())?;
        // A buffer fed by an editor gets the same rule as a file on disk, so a
        // save cannot quietly introduce a continuation.
        let violations = eden_fmt::engine::check_source(&source);
        if !violations.is_empty() {
            for violation in &violations {
                eprintln!("{}", style_line(None, violation));
            }
            eprintln!(
                "eden-fmt: {} backslash continuation(s) to replace",
                violations.len()
            );
            return Ok(ExitCode::from(1));
        }
        match eden_fmt::format_source(&source, &options) {
            Ok(formatted) => {
                print!("{formatted}");
                Ok(ExitCode::SUCCESS)
            }
            Err(error) => {
                eprintln!("eden-fmt: {error}");
                Ok(ExitCode::from(2))
            }
        }
    } else {
        if paths.is_empty() {
            paths.push(PathBuf::from("."));
        }
        let files = collect(&paths, &skips).map_err(|error| error.to_string())?;
        let outcome = run(&files, &mode, &options);
        for path in &outcome.changed {
            match mode {
                Mode::Write => println!("reformatted {}", path.display()),
                _ => println!("{}", path.display()),
            }
        }
        for (path, error) in &outcome.failed {
            eprintln!("{}: {error}", path.display());
        }
        if !outcome.failed.is_empty() {
            eprintln!(
                "eden-fmt: {} file(s) could not be formatted",
                outcome.failed.len()
            );
            return Ok(ExitCode::from(2));
        }
        if !outcome.style.is_empty() {
            for (path, violation) in &outcome.style {
                eprintln!("{}", style_line(Some(path), violation));
            }
            eprintln!(
                "eden-fmt: {} backslash continuation(s) to replace",
                outcome.style.len()
            );
            return Ok(ExitCode::from(1));
        }
        if matches!(mode, Mode::Write) {
            return Ok(ExitCode::SUCCESS);
        }
        if outcome.changed.is_empty() {
            return Ok(ExitCode::SUCCESS);
        }
        eprintln!(
            "eden-fmt: {} file(s) would be reformatted; run `pnpm rust:fmt`",
            outcome.changed.len()
        );
        Ok(ExitCode::from(1))
    }
}

/// One diagnostic line: the rule, the fix, and the literal that breaks it.
fn style_line(path: Option<&std::path::Path>, violation: &eden_fmt::engine::Violation) -> String {
    let hint = concat!(
        "string literal still uses a backslash continuation; ",
        "carry the text with `concat!(...)` or lay it out as a raw string",
    );
    match path {
        Some(path) => format!(
            "{}: {hint} ({})",
            violation.position(path),
            violation.opening
        ),
        None => format!("{hint} ({})", violation.opening),
    }
}

fn value(arguments: &[String], at: &mut usize) -> Result<String, String> {
    *at += 1;
    arguments
        .get(*at)
        .cloned()
        .ok_or_else(|| "a value is missing".to_string())
}
