# Commits and releases

Use [Conventional Commits 1.0.0](../references/conventional-commits/content/v1.0.0/index.md)
and [Semantic Versioning 2.0.0](../references/semver/spec/v2.0.0.md).
The conventions below are project defaults layered on those specifications.

## Commit messages

Use this structure:

```text
type(optional-scope)!: concise description

Optional body explaining the reason and relevant behavior.

BREAKING CHANGE: describe the incompatible change and migration.
```

Scope, `!`, body, and footers are optional. Mark a breaking change with `!`
immediately before the colon or a `BREAKING CHANGE:` footer. Prefer both a visible
marker and migration details for changes that affect users.

Use lowercase types and scopes and an imperative description without a trailing
period as our house style. These casing and prose choices extend the spec.

| Type | Use |
| --- | --- |
| `feat` | User-visible capability or public API addition |
| `fix` | Correct incorrect behavior |
| `docs` | Documentation changes |
| `refactor` | Restructure code without a feature or bug fix |
| `perf` | Performance improvement |
| `test` | Test coverage or test infrastructure |
| `build` | Dependencies, compiler settings, or build tooling |
| `ci` | CI configuration |
| `style` | Formatting with no behavior change |
| `chore` | Other maintenance, including reference snapshot refreshes |
| `revert` | Revert an earlier change; identify its commit in the body or footer |

`feat` and `fix` have defined meanings in the spec; the other types are our
conventions and carry no automatic version bump unless they break compatibility.
For example, removing a supported build configuration can require a breaking
marker even when the type is `build`.

```text
fix(parser): reject truncated input
docs: describe the error recovery contract
chore(references): refresh Rust API guidelines
feat(config)!: require explicit storage paths

BREAKING CHANGE: replace the implicit working-directory path with a storage_path setting.
```

Keep each commit coherent. Split independent changes when practical. If a project
uses squash merging, ensure the final squash message communicates the complete
change and breaking status. Do not rewrite published history to adjust messages.

## Define compatibility before choosing a version

Document the public surface: exported Rust APIs and traits, enabled features,
observable guarantees, CLI arguments and exit codes, configuration, storage and
wire formats, and supported platforms and compiler versions as applicable.
An internal implementation detail becomes a compatibility concern when users
rely on a documented promise about it.

For stable releases (`1.0.0` and above):

| Change | Version bump | Example |
| --- | --- | --- |
| Compatible bug fix | Patch | `1.4.2` → `1.4.3` |
| Compatible public functionality or deprecation | Minor | `1.4.2` → `1.5.0` |
| Incompatible public change | Major | `1.4.2` → `2.0.0` |

Choose the largest required bump across all changes in the release. Reset lower
components on minor and major bumps. Commit messages are useful inputs; verify
the actual compatibility impact before releasing.

SemVer permits an unstable API during `0.y.z`. Our default for Rust crates is to
reserve `0.y.(z+1)` for compatible fixes, use `0.(y+1).0` for incompatible changes
and new functionality, and still mark breaking commits and explain migration.
This is an explicit project policy, not a stability guarantee from SemVer.
Projects can define a stricter pre-1.0 policy. Publish `1.0.0` when ready to make
the documented stable API commitment.

Pre-release versions such as `2.0.0-rc.1` sort before `2.0.0`. Build metadata such
as `2.0.0+build.7` does not affect precedence. Never change the contents of an
already published version; publish a different version for corrections.

## Rust release review

- Review exported types, trait bounds and implementations, error variants,
  feature flags, default behavior, and documented guarantees for compatibility.
  An apparently additive API change can still affect inference or exhaustiveness.
- Treat MSRV and supported-target changes according to the project's published
  compatibility policy; state them in release notes. A dependency update can
  change those requirements too.
- Run the project's documented checks on supported toolchains and relevant
  feature combinations. Check package contents before publishing a crate.
- Update crate versions, workspace dependency requirements, release notes, and
  migration instructions consistently. Product documentation describes supported
  behavior; release notes may describe changes relative to a dated release.

Follow the project's release authorization and publication workflow. Adopting a
commit convention does not itself configure release automation or publish crates.
