# iris-tools — rules for changing this crate

Read the root `AGENTS.md` and `STANDARDS.md` first. This crate is the one binary for the
project's own tooling; the reference pack rules out Python and other interpreters for
project tools, so a new tool is a subcommand here.

## Invariants

- Camera tools (`rtsp-probe`, `onvif-call`) open one session, tear it down, and read the
  password from the environment variable `--password-env` names (`secret::Password`, whose
  `Debug` is redacted), never from a positional argument or a file in the repository.
  `rtsp-probe` writes its metadata sample only to an absent path or a caller-owned
  single-link regular file, opened without following links or blocking on a FIFO. Running
  them is bound by root rules 6 and 9.
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
  Nothing is adopted locally unless GitHub reports a valid GitHub signature, exactly one
  parent (the remote head the mutation built on, recorded in the recovery state), and the
  requested message (`verify_published_commit`); recovery, and adoption of a remote head
  that already carries the local tree, require REST verification and a single parent
  (`verified_rest_parent`). A failed check keeps the recovery state and exits non-zero.
- `soak-sample` sends the bearer to every API request it makes, `/metrics` included; it
  creates a missing output directory with mode 0700, never changes the mode of one it finds,
  and refuses one that its group or others can write. `replay-bench` writes `perf.data`
  through a descriptor it created; no privileged command touches a path inside the
  benchmark root.
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
- `secret.rs` — `Password` and the `--password-env` value parser the camera tools share.

## Testing

Unit tests run without hardware. A change to a report is checked against a saved soak
directory or daemon log; a change to `replay-bench` against a recording through
`iris-replay`; the camera probes against a live camera within the root rules.
