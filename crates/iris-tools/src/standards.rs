//! `standards`: the mechanical part of `STANDARDS.md` and of the rules in
//! `AGENTS.md`, as checks that fail.
//!
//! - `standards tree` reads every file git tracks or would track.
//! - `standards commits RANGE` reads the messages and changes of a range.
//! - `standards message FILE` reads the message of the commit being made,
//!   for a `commit-msg` hook.
//! - `standards hooks` writes that hook and a `pre-commit` hook into a
//!   clone.
//!
//! What a check cannot decide stays with a reviewer: whether research was
//! adequate, whether a document is true, whether a commit is one change.

mod command;
mod commits;
mod config;
mod finding;
mod installation;
mod layout;
mod markdown;
mod prose;
mod rust;
mod text;

pub use command::{Args, check_publication, run};
