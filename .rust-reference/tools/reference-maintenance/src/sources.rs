//! Upstream source locations and the content selected for the reference pack.

use std::collections::BTreeMap;

use crate::manifest::Source;

/// Return the supported sources without retrieval metadata.
#[must_use]
pub fn all() -> BTreeMap<String, Source> {
    let mut sources = BTreeMap::new();
    for (id, url, branch, license) in [
        (
            "rust-style-guide",
            "https://github.com/rust-lang/rust.git",
            "main",
            "MIT OR Apache-2.0; see COPYRIGHT and license files",
        ),
        (
            "rust-api-guidelines",
            "https://github.com/rust-lang/api-guidelines.git",
            "master",
            "MIT OR Apache-2.0",
        ),
        (
            "google-styleguide",
            "https://github.com/google/styleguide.git",
            "gh-pages",
            concat!(
                "Style guides: CC-BY-3.0; code samples and include/mdguide*.js: Apache-2.0; ",
                "google-c-style.el: GPL-1.0-or-later OR Artistic (see file header); ",
                "see README.md and file notices"
            ),
        ),
        (
            "conventional-commits",
            "https://github.com/conventional-commits/conventionalcommits.org.git",
            "master",
            "Repository LICENSE: MIT; published specification: CC-BY-3.0 (see site footer)",
        ),
        (
            "semver",
            "https://github.com/semver/semver.org.git",
            "gh-pages",
            "CC-BY-3.0; see specification attribution",
        ),
    ] {
        sources.insert(
            id.into(),
            Source {
                kind: "git".into(),
                url: url.into(),
                branch: Some(branch.into()),
                selection: Vec::new(),
                license: license.into(),
                transformations: Vec::new(),
                commit: None,
                retrieved_at: String::new(),
                files: BTreeMap::new(),
            },
        );
    }
    sources
        .get_mut("rust-style-guide")
        .expect("the Rust guide is included above")
        .selection = [
        "src/doc/style-guide",
        "COPYRIGHT",
        "LICENSE-APACHE",
        "LICENSE-MIT",
    ]
    .map(String::from)
    .into();
    sources.insert(
        "google-timeless-documentation".into(),
        Source {
            kind: "web".into(),
            url: "https://developers.google.com/style/timeless-documentation".into(),
            branch: None,
            selection: Vec::new(),
            license: "Text: CC-BY-4.0; code samples: Apache-2.0".into(),
            transformations: Vec::new(),
            commit: None,
            retrieved_at: String::new(),
            files: BTreeMap::new(),
        },
    );
    sources
}
