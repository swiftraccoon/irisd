# Local upstream references

Read these files directly; no website build or submodule initialization is needed.
[`manifest.json`](manifest.json) records the acquisition URL, selected paths,
retrieval time, exact Git commit where available, and every snapshot file's SHA-256.

| ID | Read locally | Upstream |
| --- | --- | --- |
| `rust-style-guide` | [Style guide](rust-style-guide/src/doc/style-guide/src/README.md), [chapter index](rust-style-guide/src/doc/style-guide/src/SUMMARY.md) | [rust-lang/rust subtree](https://github.com/rust-lang/rust/tree/main/src/doc/style-guide) |
| `rust-api-guidelines` | [Chapter index](rust-api-guidelines/src/SUMMARY.md), [checklist](rust-api-guidelines/src/checklist.md) | [rust-lang/api-guidelines](https://github.com/rust-lang/api-guidelines) |
| `google-styleguide` | [README](google-styleguide/README.md), [Rust source](google-styleguide/rustguide.md) | [google/styleguide](https://github.com/google/styleguide), [rendered Rust guide](https://google.github.io/styleguide/rustguide.html) |
| `google-timeless-documentation` | [Readable guide](google-timeless-documentation/timeless-documentation.md), [original HTML](google-timeless-documentation/source.html) | [Timeless documentation](https://developers.google.com/style/timeless-documentation) |
| `conventional-commits` | [1.0.0 specification](conventional-commits/content/v1.0.0/index.md), [README](conventional-commits/README.md) | [conventionalcommits.org repository](https://github.com/conventional-commits/conventionalcommits.org) |
| `semver` | [2.0.0 specification](semver/spec/v2.0.0.md), [README](semver/README.md) | [semver.org](https://semver.org/), [source repository](https://github.com/semver/semver.org) |

The Rust snapshot preserves the repository path `src/doc/style-guide/` and root
license notices. The Conventional Commits snapshot keeps the 1.0.0 specification
with its translations, the repository README, and the license; the SemVer snapshot
keeps the specification directory and the README; neither carries its site
generator, theme, or package manifests. The API guidelines and Google snapshots
retain their tracked repository trees. The Google Rust source supplies the
requested rendered guide's content. The timeless guide is a web snapshot, with
source attribution and a generated Markdown reading copy.

## Licenses and attribution

Upstream materials retain their own licenses; repository policy does not
relicense them. Keep each source directory, license files, and the manifest
together when redistributing these references.

- Rust source: [Apache-2.0](rust-style-guide/LICENSE-APACHE),
  [MIT](rust-style-guide/LICENSE-MIT), and [copyright notices](rust-style-guide/COPYRIGHT).
- Rust API Guidelines: [Apache-2.0](rust-api-guidelines/LICENSE-APACHE) or
  [MIT](rust-api-guidelines/LICENSE-MIT).
- Google styleguide: the guides are [CC BY 3.0](google-styleguide/LICENSE),
  and code samples are Apache-2.0, as stated in the
  [upstream README](google-styleguide/README.md#licensing).
  The bundled [Markdown helpers](google-styleguide/include/mdguide.js) carry
  Apache-2.0 notices. [google-c-style.el](google-styleguide/google-c-style.el)
  declares GNU GPL version 1 or later, or the Artistic License; retain its
  file-specific notice. The snapshot's license description records these
  exceptions; individual file notices remain authoritative.
- Google timeless documentation: page text is CC BY 4.0, and code samples are
  Apache-2.0 except where otherwise noted; attribution is included in the snapshot.
- Conventional Commits: [repository license](conventional-commits/LICENSE);
  the published specification carries a CC BY 3.0 notice in the site footer, as the
  manifest's license entry records.
- Semantic Versioning: CC BY 3.0, as stated in the
  [specification](semver/spec/v2.0.0.md).

## Maintenance

Do not edit the managed source directories by hand. Add interpretations or house
rules to `../docs/`. The reference root is a document library; optional Rust
maintenance tooling lives in `../tools/reference-maintenance/` as its own
workspace. From the pack root, refresh and verify with:

```sh
cargo run --locked --manifest-path tools/reference-maintenance/Cargo.toml -- sync
cargo run --locked --manifest-path tools/reference-maintenance/Cargo.toml -- sync --check
```

Review the source and manifest diffs after a refresh.
See [adoption and maintenance](../docs/adoption.md) for selected-source updates,
locked restores, cache behavior, and copying guidance.
