//! Landlock sandbox: the daemon may read its configuration, read and write
//! its recording roots and state directory, connect to camera ports, and
//! bind its API port, and nothing else.
//!
//! The ruleset is applied before any thread is spawned, because Landlock
//! restricts the calling thread and threads created afterwards inherit it.
//!
//! Filesystem access control (Landlock ABI 1) and TCP bind and connect control
//! (ABI 4) are required: when the kernel lacks either, [`apply`] fails and the
//! daemon starts only with `--no-sandbox`. Rights from later ABIs up to
//! [`TARGET_ABI`] are applied when the kernel has them; a kernel without them
//! leaves the ruleset partially enforced, with a warning naming the missing
//! rights. Network rules name TCP ports for any destination address; the
//! systemd unit's `IPAddressDeny=` and `IPAddressAllow=` restrict peers.

use std::path::PathBuf;

use anyhow::Context;
use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, LandlockStatus, NetPort, Ruleset,
    RulesetAttr, RulesetCreatedAttr, RulesetStatus, path_beneath_rules,
};
use tracing::{info, warn};

/// Highest ABI the rules are written for; older kernels get the required
/// rights plus whatever later rights they support.
const TARGET_ABI: ABI = ABI::V7;
/// ABI whose filesystem rights the daemon refuses to run without.
const REQUIRED_FS_ABI: ABI = ABI::V1;
/// ABI whose TCP bind and connect rights the daemon refuses to run without.
const REQUIRED_NET_ABI: ABI = ABI::V4;
/// The error when the kernel cannot enforce the required rights.
const REQUIRED_MESSAGE: &str = "Landlock ABI >= 4 required; pass --no-sandbox to run unsandboxed";

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

/// How much of the sandbox is in force, reported as `sandbox_enforced` in the
/// `process` statistics line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxEnforcement {
    /// `--no-sandbox`: no Landlock ruleset.
    Disabled,
    /// The required rights are enforced; rights newer than the kernel's
    /// Landlock ABI are not.
    Partial,
    /// Every requested right is enforced.
    Full,
}

impl SandboxEnforcement {
    /// The statistics value: 0 disabled, 1 partially enforced, 2 fully enforced.
    pub fn level(self) -> u8 {
        match self {
            Self::Disabled => 0,
            Self::Partial => 1,
            Self::Full => 2,
        }
    }
}

/// What the kernel did with the requested ruleset.
#[derive(Debug)]
enum KernelOutcome {
    /// A required right is unavailable: Landlock is absent, disabled at boot,
    /// or older than ABI 4.
    RequiredUnsupported(String),
    /// `restrict_self` returned `status`; `abi` is the ABI the kernel offered.
    Restricted {
        status: RulesetStatus,
        abi: Option<ABI>,
    },
}

/// Applies the sandbox to the calling thread and its future children.
///
/// # Errors
///
/// Fails when the kernel cannot enforce the required rights, and when a rule
/// cannot be created, added, or enforced.
pub fn apply(spec: &SandboxSpec) -> anyhow::Result<SandboxEnforcement> {
    decide(restrict(spec)?)
}

fn restrict(spec: &SandboxSpec) -> anyhow::Result<KernelOutcome> {
    let required = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(REQUIRED_FS_ABI))
        .and_then(|ruleset| ruleset.handle_access(AccessNet::from_all(REQUIRED_NET_ABI)));
    let ruleset = match required {
        Ok(ruleset) => ruleset,
        Err(error) => return Ok(KernelOutcome::RequiredUnsupported(error.to_string())),
    };
    let abi = TARGET_ABI;
    let mut ruleset = ruleset
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
    let abi = match status.landlock {
        LandlockStatus::Available { effective_abi, .. } => Some(effective_abi),
        LandlockStatus::NotEnabled | LandlockStatus::NotImplemented => None,
    };
    Ok(KernelOutcome::Restricted {
        status: status.ruleset,
        abi,
    })
}

/// Turns the kernel's answer into the daemon's decision: run fully or partially
/// sandboxed, or refuse to start.
fn decide(outcome: KernelOutcome) -> anyhow::Result<SandboxEnforcement> {
    match outcome {
        KernelOutcome::RequiredUnsupported(reason) => {
            Err(anyhow::anyhow!(reason).context(REQUIRED_MESSAGE))
        }
        KernelOutcome::Restricted {
            status: RulesetStatus::NotEnforced,
            ..
        } => Err(
            anyhow::anyhow!("the kernel did not enforce the Landlock ruleset")
                .context(REQUIRED_MESSAGE),
        ),
        KernelOutcome::Restricted {
            status: RulesetStatus::PartiallyEnforced,
            abi,
        } => {
            warn!(
                kernel_abi = %abi.unwrap_or(ABI::Unsupported),
                missing = %missing_optional_rights(abi),
                "sandbox partially enforced: the kernel lacks newer Landlock rights"
            );
            Ok(SandboxEnforcement::Partial)
        }
        KernelOutcome::Restricted {
            status: RulesetStatus::FullyEnforced,
            ..
        } => {
            info!("sandbox fully enforced");
            Ok(SandboxEnforcement::Full)
        }
    }
}

/// The requested rights a kernel offering `abi` cannot enforce.
fn missing_optional_rights(abi: Option<ABI>) -> String {
    let abi = abi.unwrap_or(ABI::Unsupported);
    let filesystem = AccessFs::from_all(TARGET_ABI) & !AccessFs::from_all(abi);
    let network = AccessNet::from_all(TARGET_ABI) & !AccessNet::from_all(abi);
    format!("filesystem {filesystem:?}; network {network:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kernel_without_the_required_rights_refuses_to_start() {
        for reason in ["Landlock is not supported", "ABI 3 lacks TCP rules"] {
            let error = decide(KernelOutcome::RequiredUnsupported(reason.to_owned())).unwrap_err();
            assert_eq!(error.to_string(), REQUIRED_MESSAGE);
            let chain = format!("{error:#}");
            assert!(chain.contains("--no-sandbox"), "{chain}");
            assert!(chain.contains(reason), "{chain}");
        }
    }

    #[test]
    fn an_unenforced_ruleset_refuses_to_start() {
        let error = decide(KernelOutcome::Restricted {
            status: RulesetStatus::NotEnforced,
            abi: None,
        })
        .unwrap_err();
        assert_eq!(error.to_string(), REQUIRED_MESSAGE);
    }

    #[test]
    fn missing_optional_rights_run_partially_enforced() {
        assert_eq!(
            decide(KernelOutcome::Restricted {
                status: RulesetStatus::PartiallyEnforced,
                abi: Some(ABI::V4),
            })
            .unwrap(),
            SandboxEnforcement::Partial
        );
        let missing = missing_optional_rights(Some(ABI::V4));
        assert!(missing.contains("IoctlDev"), "{missing}");
        assert!(!missing.contains("ConnectTcp"), "{missing}");
    }

    #[test]
    fn a_fully_enforced_ruleset_runs() {
        assert_eq!(
            decide(KernelOutcome::Restricted {
                status: RulesetStatus::FullyEnforced,
                abi: Some(TARGET_ABI),
            })
            .unwrap(),
            SandboxEnforcement::Full
        );
        let missing = missing_optional_rights(Some(TARGET_ABI));
        assert!(
            !missing.contains("IoctlDev") && !missing.contains("Tcp"),
            "{missing}"
        );
    }

    #[test]
    fn the_required_rights_are_part_of_every_request() {
        assert!(AccessFs::from_all(TARGET_ABI).contains(AccessFs::from_all(REQUIRED_FS_ABI)));
        assert!(AccessNet::from_all(TARGET_ABI).contains(AccessNet::from_all(REQUIRED_NET_ABI)));
        assert!(!AccessNet::from_all(REQUIRED_NET_ABI).is_empty());
    }

    #[test]
    fn enforcement_levels_are_ordered_for_the_statistics_line() {
        assert_eq!(
            [
                SandboxEnforcement::Disabled,
                SandboxEnforcement::Partial,
                SandboxEnforcement::Full
            ]
            .map(SandboxEnforcement::level),
            [0, 1, 2]
        );
    }
}
