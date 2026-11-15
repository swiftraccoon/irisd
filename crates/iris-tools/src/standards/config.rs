//! `standards.toml`: the exceptions to the checks, each with its reason.

use std::collections::BTreeMap;

use anyhow::Context;
use serde::Deserialize;

use super::finding::{Finding, Rule};

/// The file's contents.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    /// Texts of published sources that documents cite and a pattern of
    /// `AGENTS.md` rule 9 would refuse: a mail address, a path.
    #[serde(default)]
    pub allowed: Vec<Allowed>,
    /// The accepted exceptions.
    #[serde(default, rename = "waiver")]
    pub waivers: Vec<Waiver>,
}

/// A text the installation patterns pass, and whose it is.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allowed {
    /// The text, as the files hold it.
    pub text: String,
    /// The published source it comes from.
    pub reason: String,
}

/// An accepted exception: `count` breaches of `rule` in `path`, and why they
/// stand.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Waiver {
    /// The rule's name.
    pub rule: String,
    /// The file, relative to the repository's root.
    pub path: String,
    /// How many breaches the file holds. More fail, and so do fewer: a waiver
    /// covers what a reviewer saw and nothing else.
    pub count: usize,
    /// Why they stand.
    pub reason: String,
}

/// The rules a waiver may cover: those that read meaning from words or
/// shapes and so can be wrong. A rule that reads a fact has no exceptions.
fn waivable(rule: Rule) -> bool {
    matches!(
        rule,
        Rule::Timeless
            | Rule::Narration
            | Rule::DeviceCount
            | Rule::GetPrefix
            | Rule::BoolParameter
            | Rule::BareTuple
    )
}

impl Config {
    /// Parses the file's text.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        toml::from_str(text).context("parsing standards.toml")
    }

    /// `findings` less those the waivers cover, plus a finding for every
    /// waiver that is malformed or that no longer matches its file.
    pub fn settle(&self, findings: &[Finding]) -> Vec<Finding> {
        const FILE: &str = "standards.toml";
        let mut counts: BTreeMap<(Rule, &str), usize> = BTreeMap::new();
        for finding in findings {
            *counts.entry((finding.rule, &finding.path)).or_default() += 1;
        }
        let mut covered: Vec<(Rule, &str)> = Vec::new();
        let mut extra = Vec::new();
        for allowed in &self.allowed {
            if allowed.reason.trim().is_empty() {
                let message = format!("the allowed text {:?} has no reason", allowed.text);
                extra.push(Finding::new(Rule::Installation, FILE, 0, message));
            }
        }
        for waiver in &self.waivers {
            let Waiver {
                rule,
                path,
                count,
                reason,
            } = waiver;
            let Some(rule) = Rule::from_name(rule).filter(|rule| waivable(*rule)) else {
                let message = format!("no waiver exists for the rule {rule:?} ({path})");
                extra.push(Finding::new(Rule::Leftover, FILE, 0, message));
                continue;
            };
            if reason.trim().is_empty() {
                let message = format!("the waiver of {} in {path} has no reason", rule.name());
                extra.push(Finding::new(rule, FILE, 0, message));
            }
            let found = counts.get(&(rule, path.as_str())).copied().unwrap_or(0);
            if found <= *count {
                covered.push((rule, path));
            }
            if found != *count {
                let message = format!("the waiver allows {count}, the file holds {found}");
                extra.push(Finding::new(rule, path, 0, message));
            }
        }
        let mut kept: Vec<Finding> = findings
            .iter()
            .filter(|finding| !covered.contains(&(finding.rule, finding.path.as_str())))
            .cloned()
            .collect();
        kept.append(&mut extra);
        kept.sort();
        kept
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(count: usize, rule: &str, reason: &str) -> Config {
        Config::parse(&format!(
            "[[waiver]]\nrule = \"{rule}\"\npath = \"a.md\"\ncount = {count}\nreason = \"{reason}\"\n"
        ))
        .unwrap()
    }

    fn two() -> Vec<Finding> {
        vec![
            Finding::new(Rule::Timeless, "a.md", 1, "new"),
            Finding::new(Rule::Timeless, "a.md", 2, "old"),
        ]
    }

    #[test]
    fn a_waiver_covers_exactly_its_count() {
        assert_eq!(config(2, "timeless", "quotes the rule").settle(&two()), []);
        // One more than was accepted: every breach is reported.
        assert_eq!(config(1, "timeless", "quotes").settle(&two()).len(), 3);
        // One fewer: the waiver is stale.
        let stale = config(3, "timeless", "quotes").settle(&two());
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].message, "the waiver allows 3, the file holds 2");
    }

    #[test]
    fn a_waiver_needs_a_reason_and_a_rule_that_takes_one() {
        assert_eq!(config(2, "timeless", " ").settle(&two()).len(), 1);
        assert_eq!(config(2, "hex-case", "taste").settle(&two()).len(), 3);
        assert_eq!(config(2, "nonsense", "taste").settle(&two()).len(), 3);
        assert!(Config::parse("unknown = 1").is_err());
        let unexplained =
            Config::parse("[[allowed]]\ntext = \"a@b.org\"\nreason = \"\"\n").unwrap();
        assert_eq!(unexplained.settle(&[]).len(), 1);
    }
}
