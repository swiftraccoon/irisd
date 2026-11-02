//! Landlock sandbox: the daemon may read its configuration, read and write
//! its recording roots and state directory, connect to camera ports, and
//! bind its API port, and nothing else.
//!
//! The ruleset is applied before any thread is spawned, because Landlock
//! restricts the calling thread and threads created afterwards inherit it.

use std::path::PathBuf;

use anyhow::Context;
use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, NetPort, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus, path_beneath_rules,
};
use tracing::{info, warn};

/// Highest ABI the rules are written for; older kernels get a best-effort subset.
const TARGET_ABI: ABI = ABI::V7;

/// What the daemon needs.
#[derive(Clone, Debug, Default)]
pub struct SandboxSpec {
    /// Directories readable (configuration, `/etc`, `/proc`).
    pub read_only: Vec<PathBuf>,
    /// Directories readable and writable (recording roots, state).
    pub read_write: Vec<PathBuf>,
    /// TCP ports the daemon may connect to.
    pub connect_ports: Vec<u16>,
    /// TCP ports the daemon may bind.
    pub bind_ports: Vec<u16>,
}

/// Applies the sandbox to the calling thread and its future children.
pub fn apply(spec: &SandboxSpec) -> anyhow::Result<RulesetStatus> {
    let abi = TARGET_ABI;
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .context("declaring filesystem access")?
        .handle_access(AccessNet::from_all(abi))
        .context("declaring network access")?
        .create()
        .context("creating the Landlock ruleset")?;
    ruleset = ruleset
        .add_rules(path_beneath_rules(
            &spec.read_only,
            AccessFs::from_read(abi),
        ))
        .context("adding read-only rules")?;
    ruleset = ruleset
        .add_rules(path_beneath_rules(
            &spec.read_write,
            AccessFs::from_all(abi),
        ))
        .context("adding read-write rules")?;
    for &port in &spec.connect_ports {
        ruleset = ruleset
            .add_rule(NetPort::new(port, AccessNet::ConnectTcp))
            .with_context(|| format!("allowing TCP connect to port {port}"))?;
    }
    for &port in &spec.bind_ports {
        ruleset = ruleset
            .add_rule(NetPort::new(port, AccessNet::BindTcp))
            .with_context(|| format!("allowing TCP bind on port {port}"))?;
    }
    let status = ruleset
        .restrict_self()
        .context("enforcing the Landlock ruleset")?;
    match status.ruleset {
        RulesetStatus::FullyEnforced => info!("sandbox fully enforced"),
        RulesetStatus::PartiallyEnforced => {
            info!("sandbox partially enforced (kernel lacks some Landlock features)");
        }
        RulesetStatus::NotEnforced => warn!("sandbox not enforced: kernel lacks Landlock"),
    }
    Ok(status.ruleset)
}
