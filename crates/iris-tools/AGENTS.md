# iris-tools — rules for changing this crate

Read the root `AGENTS.md` and `STANDARDS.md` first. This crate is the one binary for the
project's own tooling; the reference pack rules out Python and other interpreters for
project tools, so a new tool is a subcommand here.

## Invariants

- Camera tools (`rtsp-probe`, `onvif-call`) open one session, tear it down, and take
  credentials as arguments or from the environment, never from a file in the repository.
  Running them is bound by root rules 6 and 9.
- `soak-sample` is read-only towards the host and the service: `/proc`, `/sys`, the
  journal, `systemctl show`, `ss`, `chronyc`, and the API over loopback. A section that
  fails yields an `error` field, never a failed sample, so a timer keeps the series intact.
- The report tools parse the daemon's statistics lines by field name
  (`docs/research/07-architecture.md §9`); a field renamed in the daemon is renamed here
  in the same change.
- `replay-bench` is the harness behind every cost figure in `docs/research/12-ingest-cost.md`;
  a change to what it measures or how it windows samples re-measures the table.
- `gh-commit` publishes exactly the difference between `origin/<branch>` and the local
  `HEAD` and refuses a dirty working tree; it never force-updates a branch.

## Where things are

- `main.rs` — the clap subcommand table.
- `logs.rs`, `time.rs` — the statistics line parser and the RFC 3339 helpers shared by
  the reports.
- `stats.rs`, `report.rs`, `sample.rs`, `bench.rs`, `rtsp_probe.rs`, `onvif_call.rs`,
  `gh_commit.rs` — one subcommand each, with `Args` and `run`.

## Testing

Unit tests run without hardware. A change to a report is checked against a saved soak
directory or daemon log; a change to `replay-bench` against a recording through
`iris-replay`; the camera probes against a live camera within the root rules.
