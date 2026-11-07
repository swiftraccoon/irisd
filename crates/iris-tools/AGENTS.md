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
  in the same change. Event-feed counters are independent per `(camera, source)` pair.
- Catalog summaries retain queue count/byte peaks, event-state/pruning fields, overload
  rejections, and reconciliation-required state so bounded-admission failures remain visible.
- `replay-bench` is the harness behind every cost figure in `docs/research/12-ingest-cost.md`;
  rate calculations use counter deltas over a shared post-warm-up window for payload,
  CPU, and wake-ups. Changes to these calculations require remeasurement before making
  performance claims; saved results retain the provenance of their original harness.
- `gh-commit` publishes exactly the difference between `origin/<branch>` and the local
  `HEAD` and refuses a dirty working tree; it never force-updates a branch. Additions read
  committed blobs, and modes other than `100644` are rejected because the API cannot
  preserve executable files, symlinks, or submodules. Never read a symlink's worktree target.
- Startup messages for unavailable roots, the sandbox decision, and catalog open precede
  reconciliation and belong to the starting soak run. Their ordered stages distinguish
  repeated startup attempts; repeated unavailable-root messages can belong to one attempt.
  Reconciliation after statistics or another reconciliation also starts a separate run,
  even without a `stopped` line. Keep recovery and cumulative counters separate per run;
  an unclean stop's unobserved interval is not an exact downtime measurement. Arbitrary
  warnings cannot establish a restart, and `catalog open` is not a catalog statistics row.

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
