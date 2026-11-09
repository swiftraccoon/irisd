//! Landlock sandbox. The daemon may read its configuration directory, `/etc`,
//! and `/proc`; read, create, write, rename, truncate, and remove regular
//! files and directories beneath its recording roots and state directory;
//! connect over TCP to the camera and provisioning ports; and bind its API
//! port. Executing files, creating device nodes, sockets, FIFOs, or symbolic
//! links beneath the writable paths, and `ioctl` on devices are denied. UDP
//! (the WS-Discovery probe), pathname Unix sockets (`sd_notify`), and signals
//! are outside the ruleset; on a kernel with Landlock ABI 6 the daemon is also
//! scoped away from abstract Unix sockets and from signalling processes outside
//! its domain. The systemd unit's `IPAddressDeny=` and `IPAddressAllow=`
//! restrict peers.
//!
//! The ruleset is applied before any thread is spawned, because Landlock
//! restricts the calling thread and threads created afterwards inherit it.
//!
//! Filesystem access control (Landlock ABI 1) and TCP bind and connect control
//! (ABI 4) are required: when the kernel lacks either, [`apply`] fails and the
//! daemon starts only with `--no-sandbox`. Rights and scopes from later ABIs
//! up to [`TARGET_ABI`] are applied when the kernel has them; a kernel without
//! them leaves the ruleset partially enforced, with a warning naming the
//! missing rights. Network rules name TCP ports for any destination address.

use std::path::PathBuf;

use anyhow::Context;
use landlock::{
    ABI, Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, LandlockStatus, NetPort,
    Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, Scope, path_beneath_rules,
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
    /// The required rights are enforced; rights beyond the kernel's Landlock
    /// ABI are not.
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

/// Applies the sandbox to the calling thread and the threads it creates
/// afterwards.
///
/// # Errors
///
/// Fails when the kernel cannot enforce the required rights, and when a rule
/// cannot be created, added, or enforced.
pub fn apply(spec: &SandboxSpec) -> anyhow::Result<SandboxEnforcement> {
    decide(restrict(spec)?)
}

/// The rights granted beneath the recording roots and the state directory:
/// everything `abi` offers except executing files, creating device nodes,
/// sockets, FIFOs, and symbolic links, and `ioctl` on devices.
fn read_write_rights(abi: ABI) -> BitFlags<AccessFs> {
    AccessFs::from_all(abi)
        & !(AccessFs::Execute
            | AccessFs::MakeChar
            | AccessFs::MakeBlock
            | AccessFs::MakeSock
            | AccessFs::MakeFifo
            | AccessFs::MakeSym
            | AccessFs::IoctlDev)
}

/// The rights granted on the read-only paths: reading files and listing
/// directories, never executing.
fn read_only_rights(abi: ABI) -> BitFlags<AccessFs> {
    AccessFs::from_read(abi) & !AccessFs::Execute
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
        .scope(Scope::from_all(abi))
        .context("declaring scopes")?
        .create()
        .context("creating the Landlock ruleset")?;
    ruleset = ruleset
        .add_rules(path_beneath_rules(&spec.read_only, read_only_rights(abi)))
        .context("adding read-only rules")?;
    ruleset = ruleset
        .add_rules(path_beneath_rules(&spec.read_write, read_write_rights(abi)))
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
                "sandbox partially enforced: the kernel lacks later Landlock rights"
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

/// The requested rights and scopes a kernel offering `abi` cannot enforce.
fn missing_optional_rights(abi: Option<ABI>) -> String {
    let abi = abi.unwrap_or(ABI::Unsupported);
    let filesystem = AccessFs::from_all(TARGET_ABI) & !AccessFs::from_all(abi);
    let network = AccessNet::from_all(TARGET_ABI) & !AccessNet::from_all(abi);
    let scopes = Scope::from_all(TARGET_ABI) & !Scope::from_all(abi);
    format!("filesystem {filesystem:?}; network {network:?}; scopes {scopes:?}")
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write as _;

    use iris_catalog::CatalogHandle;
    use iris_core::id::CameraId;

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
        assert!(missing.contains("Signal"), "{missing}");
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
            !missing.contains("IoctlDev")
                && !missing.contains("Tcp")
                && !missing.contains("Signal"),
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
    fn granted_rights_leave_out_execution_and_special_files() {
        let writable = read_write_rights(TARGET_ABI);
        for needed in [
            AccessFs::ReadFile,
            AccessFs::WriteFile,
            AccessFs::ReadDir,
            AccessFs::MakeDir,
            AccessFs::MakeReg,
            AccessFs::RemoveFile,
            AccessFs::RemoveDir,
            AccessFs::Refer,
            AccessFs::Truncate,
        ] {
            assert!(writable.contains(needed), "{needed:?}");
        }
        for denied in [
            AccessFs::Execute,
            AccessFs::MakeSym,
            AccessFs::MakeFifo,
            AccessFs::MakeSock,
            AccessFs::MakeChar,
            AccessFs::MakeBlock,
            AccessFs::IoctlDev,
        ] {
            assert!(!writable.contains(denied), "{denied:?}");
        }
        let readable = read_only_rights(TARGET_ABI);
        assert!(readable.contains(AccessFs::ReadFile | AccessFs::ReadDir));
        assert!(!readable.contains(AccessFs::Execute));
        assert!(!readable.contains(AccessFs::WriteFile));
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

    /// Applies a sandbox whose writable tree is `dir` on a thread of its own
    /// and runs `check` inside it. Landlock restricts that thread and its
    /// children only. Skipped on a kernel without Landlock.
    fn sandboxed(dir: PathBuf, check: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .name("irisd-sandbox-test".into())
            .spawn(move || {
                let spec = SandboxSpec {
                    read_only: vec![PathBuf::from("/proc"), PathBuf::from("/etc")],
                    read_write: vec![dir],
                    connect_ports: Vec::new(),
                    bind_ports: Vec::new(),
                };
                match apply(&spec) {
                    Ok(_) => check(),
                    Err(error) if error.to_string() == REQUIRED_MESSAGE => {
                        // The test kernel offers no Landlock; nothing to verify.
                    }
                    Err(error) => panic!("{error:#}"),
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn a_sandboxed_thread_keeps_the_filesystem_rights_the_daemon_needs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let outside = tempfile::tempdir().unwrap();
        let elsewhere = outside.path().join("denied");
        sandboxed(root.clone(), move || {
            let nested = root.join("cam/main/2026-01-01");
            std::fs::create_dir_all(&nested).unwrap();
            let file = nested.join("a.mp4");
            let mut created = File::create(&file).unwrap();
            created.write_all(b"recorded").unwrap();
            created.set_len(4).unwrap();
            created.sync_all().unwrap();
            drop(created);
            std::fs::rename(&file, nested.join("b.mp4")).unwrap();
            assert_eq!(std::fs::read(nested.join("b.mp4")).unwrap(), b"reco");
            File::open(&nested).unwrap().sync_all().unwrap();
            std::fs::remove_file(nested.join("b.mp4")).unwrap();
            std::fs::remove_dir(&nested).unwrap();
            assert!(std::fs::read_to_string("/proc/self/status").is_ok());
            assert!(
                std::os::unix::fs::symlink("target", root.join("link")).is_err(),
                "symbolic links are denied beneath the writable tree"
            );
            assert!(
                File::create(&elsewhere).is_err(),
                "writes outside the allowed trees are denied"
            );
        });
    }

    #[test]
    fn a_sandboxed_thread_can_run_the_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        sandboxed(root.clone(), move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            runtime.block_on(async {
                let catalog = CatalogHandle::open(&root.join("irisd.db")).unwrap();
                let camera = CameraId::try_new("cam1").unwrap();
                catalog
                    .ensure_camera(&camera, "Sandboxed", None)
                    .await
                    .unwrap();
                assert_eq!(catalog.cameras().await.unwrap().len(), 1);
                catalog.shutdown();
            });
        });
    }
}
