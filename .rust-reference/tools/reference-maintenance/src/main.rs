use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use rust_project_reference::{export, reference_root, report, sync};

const USAGE: &str = "Rust project reference maintenance

Usage:
  rust-project-reference [--root PATH] sync [--check | --locked] [--force] [SOURCE ...]
  rust-project-reference [--root PATH] export PROJECT

sync      Fetch upstream sources; --check verifies offline; --locked restores recorded revisions.
export    Copy the complete pack into PROJECT/.rust-reference without overwriting project files.

The pack root defaults to the reference library containing tools/reference-maintenance.
Git, curl, and network access are required to fetch upstream content.";

fn main() {
    if let Err(error) = run() {
        eprintln!("Reference maintenance failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = env::args_os().skip(1).peekable();
    let mut root = reference_root().to_path_buf();
    if args.peek().is_some_and(|arg| arg == "--root") {
        args.next();
        root = PathBuf::from(args.next().context("--root requires a path")?);
    }
    let Some(command) = args.next() else {
        report(USAGE)?;
        return Ok(());
    };
    match command.to_str().context("Command must be UTF-8")? {
        "--help" | "-h" | "help" => report(USAGE)?,
        "sync" => {
            let mut options = sync::Options::default();
            let mut sources = Vec::new();
            for argument in args {
                let argument = argument
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("Source IDs and options must be UTF-8"))?;
                match argument.as_str() {
                    "--check" => options.check = true,
                    "--locked" => options.locked = true,
                    "--force" => options.force = true,
                    "--help" | "-h" => {
                        report(USAGE)?;
                        return Ok(());
                    }
                    flag if flag.starts_with('-') => bail!("Unknown option: {flag}"),
                    _ => sources.push(argument),
                }
            }
            ensure!(
                !(options.check && options.locked),
                "Choose --check or --locked"
            );
            sync::run(&root, &sources, options)?;
        }
        "export" => {
            let project = PathBuf::from(args.next().context("export requires a project path")?);
            ensure!(args.next().is_none(), "export accepts one project path");
            let export = export::run(&root, &project)?;
            report(format_args!(
                "Reference pack copied to {}",
                export.destination.display()
            ))?;
            report("Merge .rust-reference/templates/AGENTS.md into the project's AGENTS.md.")?;
            if !export.ignored_by_git.is_empty() {
                let examples: Vec<_> = export.ignored_by_git.iter().take(3).cloned().collect();
                eprintln!(
                    "Warning: the project's Git ignore rules exclude {} pack file(s), \
                     including {}. Adjust .gitignore before committing, or the committed \
                     pack will be incomplete.",
                    export.ignored_by_git.len(),
                    examples.join(", ")
                );
            }
        }
        other => bail!("Unknown command: {other}\n\n{USAGE}"),
    }
    Ok(())
}
