//! Process-boundary regressions: hooks pause actual recovery, not a model of it.
use super::*;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

#[test]
fn recovery_process_child() {
    let Ok(root) = std::env::var("ZC_RECOVERY_ROOT") else {
        return;
    };
    let mut host = PluginHost::from_plugins_dir(Path::new(&root)).unwrap();
    if std::env::var("ZC_RECOVERY_ACTION").as_deref() == Ok("stage") {
        let tx = host
            .recovery_root
            .transaction("race", "installing")
            .unwrap();
        tx.dir.create_dir(recovery::PACKAGE).unwrap();
        tx.dir.write("package/bytes", b"active owner").unwrap();
        println!("STAGE:{}", tx.entry);
        std::io::stdout().flush().unwrap();
        recovery::pause("stage-owned");
        // Dropping the process/lease without cleanup simulates abandonment.
        return;
    }
    if std::env::var("ZC_RECOVERY_ACTION").as_deref() == Ok("healthy-late") {
        tests::write_tool_source(
            &Path::new(&root).join("race"),
            "race",
            b"\0asm healthy restored",
        );
    }
    let result = host.remove_with_report("race");
    println!("RESULT:{result:?}");
}

struct Paused {
    child: Child,
    output: BufReader<std::process::ChildStdout>,
    lines: String,
}
impl Paused {
    fn start(root: &Path, step: &str, action: &str) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "host::recovery_tests::recovery_process_child",
                "--nocapture",
            ])
            .env("ZC_RECOVERY_ROOT", root)
            .env("ZC_RECOVERY_BARRIER", step)
            .env("ZC_RECOVERY_ACTION", action)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut child = Self {
            child,
            output,
            lines: String::new(),
        };
        loop {
            let mut line = String::new();
            assert_ne!(
                child.output.read_line(&mut line).unwrap(),
                0,
                "child ended before barrier: {}",
                child.lines
            );
            child.lines.push_str(&line);
            if line.starts_with("BARRIER:") {
                break;
            }
        }
        child
    }
    fn advance(&mut self, step: &str) {
        writeln!(self.child.stdin.as_mut().unwrap(), "continue").unwrap();
        loop {
            let mut line = String::new();
            assert_ne!(
                self.output.read_line(&mut line).unwrap(),
                0,
                "child ended before second barrier: {}",
                self.lines
            );
            self.lines.push_str(&line);
            if line.trim() == format!("BARRIER:{step}") {
                break;
            }
        }
    }
    fn resume(mut self) -> String {
        writeln!(self.child.stdin.take().unwrap(), "continue").unwrap();
        self.output.read_to_string(&mut self.lines).unwrap();
        assert!(self.child.wait().unwrap().success(), "{}", self.lines);
        self.lines.clone()
    }
    fn crash(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}
impl Drop for Paused {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn broken(root: &Path) {
    std::fs::create_dir(root.join("race")).unwrap();
    std::fs::write(root.join("race/manifest.toml"), "name =").unwrap();
}
fn healthy(root: &Path) -> Vec<(String, Vec<u8>)> {
    tests::write_tool_source(&root.join("race"), "race", b"\0asm healthy replacement");
    tests::package_bytes(&root.join("race"))
}

#[test]
fn separate_remover_validates_replacement_before_claim_and_restores_it() {
    let root = tempfile::tempdir().unwrap();
    broken(root.path());
    let paused = Paused::start(root.path(), "before-claim", "remove");
    std::fs::rename(root.path().join("race"), root.path().join("old")).unwrap();
    std::fs::create_dir(root.path().join("race")).unwrap();
    let expected = healthy(root.path());
    let output = paused.resume();
    assert!(
        output.contains("Namespace") || output.contains("namespace"),
        "{output}"
    );
    assert_eq!(tests::package_bytes(&root.path().join("race")), expected);
    assert!(root.path().join("old/manifest.toml").exists());
}

#[test]
fn separate_remover_deletes_only_claimed_generation_after_final_replacement() {
    let root = tempfile::tempdir().unwrap();
    broken(root.path());
    let paused = Paused::start(root.path(), "before-delete", "remove");
    std::fs::create_dir(root.path().join("race")).unwrap();
    let expected = healthy(root.path());
    assert!(paused.resume().contains("RESULT:Ok"));
    assert_eq!(tests::package_bytes(&root.path().join("race")), expected);
}

#[test]
fn crash_after_claim_restores_then_recovers_on_retry() {
    let root = tempfile::tempdir().unwrap();
    broken(root.path());
    Paused::start(root.path(), "after-claim", "remove").crash();
    assert!(!root.path().join("race").exists());
    let mut host = PluginHost::from_plugins_dir(root.path()).unwrap();
    host.remove("race").unwrap();
    assert_eq!(
        tests::dir_entries(root.path()),
        [".zeroclaw-package-lock-v1"]
    );
    let source = tempfile::tempdir().unwrap();
    tests::write_tool_source(source.path(), "race", b"\0asm reinstall");
    assert_eq!(
        host.install(source.path().to_str().unwrap()).unwrap(),
        "race"
    );
}

#[test]
fn healthy_claim_restore_never_clobbers_and_crash_retry_retains_both() {
    let root = tempfile::tempdir().unwrap();
    broken(root.path());
    // A late healthy package is unknown to the child's loaded map.
    let paused = Paused::start(root.path(), "after-claim", "remove");
    let claim = tests::dir_entries(root.path())
        .into_iter()
        .find(|p| p.contains("recovering-v1"))
        .unwrap();
    tests::write_tool_source(
        &root.path().join(&claim).join("package"),
        "race",
        b"\0asm healthy claim",
    );
    std::fs::create_dir(root.path().join("race")).unwrap();
    let expected = healthy(root.path());
    let output = paused.resume();
    assert!(output.contains("RecoveryRetained"), "{output}");
    assert_eq!(tests::package_bytes(&root.path().join("race")), expected);
    let retained = tests::package_bytes(&root.path().join(&claim).join("package"));
    let mut host =
        PluginHost::from_plugins_dir_with_security(root.path(), SignatureMode::Strict, vec![])
            .unwrap();
    assert!(matches!(
        host.remove("race"),
        Err(PluginError::RecoveryRetained { .. })
    ));
    assert_eq!(
        tests::package_bytes(&root.path().join(&claim).join("package")),
        retained
    );
    assert_eq!(tests::package_bytes(&root.path().join("race")), expected);
    std::fs::rename(root.path().join("race"), root.path().join("second")).unwrap();
    assert!(matches!(
        host.remove("race"),
        Err(PluginError::UnadmittedPackage { .. })
    ));
    assert_eq!(tests::package_bytes(&root.path().join("race")), retained);
}

#[test]
fn real_active_stage_survives_and_abandoned_protocol_stage_is_cleaned() {
    let root = tempfile::tempdir().unwrap();
    broken(root.path());
    let stage = Paused::start(root.path(), "stage-owned", "stage");
    let name = stage
        .lines
        .lines()
        .find_map(|line| line.strip_prefix("STAGE:"))
        .unwrap()
        .to_string();
    let mut host = PluginHost::from_plugins_dir(root.path()).unwrap();
    let retained = host.remove_with_report("race").unwrap();
    assert!(retained.contains(&root.path().join(&name)));
    assert_eq!(
        std::fs::read(root.path().join(&name).join("package/bytes")).unwrap(),
        b"active owner"
    );
    stage.crash();
    broken(root.path());
    host.remove("race").unwrap();
    assert!(!root.path().join(name).exists());
}

#[cfg(unix)]
#[test]
fn paused_stage_cleanup_does_not_delete_replacement_generation() {
    let root = tempfile::tempdir().unwrap();
    broken(root.path());
    let owner = Paused::start(root.path(), "stage-owned", "stage");
    let stage = owner
        .lines
        .lines()
        .find_map(|line| line.strip_prefix("STAGE:"))
        .unwrap()
        .to_string();
    owner.crash();
    let remover = Paused::start(root.path(), "before-stage-delete", "remove");
    std::fs::rename(root.path().join(&stage), root.path().join("claimed-old")).unwrap();
    std::fs::create_dir(root.path().join(&stage)).unwrap();
    std::fs::write(
        root.path().join(&stage).join("new-live-bytes"),
        b"never delete",
    )
    .unwrap();
    let output = remover.resume();
    assert!(output.contains("RecoveryRetained"), "{output}");
    assert_eq!(
        std::fs::read(root.path().join(&stage).join("new-live-bytes")).unwrap(),
        b"never delete"
    );
}

#[cfg(unix)]
#[test]
fn retained_root_and_replaced_coordination_lock_refuse_namespace_changes() {
    for swap_root in [true, false] {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("plugins");
        std::fs::create_dir(&root).unwrap();
        broken(&root);
        let paused = Paused::start(&root, "before-claim", "remove");
        if swap_root {
            std::fs::rename(&root, parent.path().join("old-root")).unwrap();
            let other = parent.path().join("other");
            std::fs::create_dir(&other).unwrap();
            std::os::unix::fs::symlink(&other, &root).unwrap();
            std::fs::write(other.join("keep"), b"outside").unwrap();
        } else {
            std::fs::rename(
                root.join(".zeroclaw-package-lock-v1"),
                root.join("old-lock"),
            )
            .unwrap();
            std::fs::write(root.join(".zeroclaw-package-lock-v1"), b"").unwrap();
        }
        let output = paused.resume();
        assert!(output.contains("NamespaceChanged"), "{output}");
        if swap_root {
            assert_eq!(
                std::fs::read(parent.path().join("other/keep")).unwrap(),
                b"outside"
            );
        } else {
            assert_eq!(
                std::fs::read(root.join("race/manifest.toml")).unwrap(),
                b"name ="
            );
        }
    }
}

#[test]
fn crashed_healthy_restore_is_retried_without_deleting_its_bytes() {
    let root = tempfile::tempdir().unwrap();
    broken(root.path());
    Paused::start(root.path(), "before-restore", "healthy-late").crash();
    let claim = tests::dir_entries(root.path())
        .into_iter()
        .find(|p| p.contains("recovering-v1"))
        .unwrap();
    let expected = tests::package_bytes(&root.path().join(&claim).join("package"));
    let mut host = PluginHost::from_plugins_dir(root.path()).unwrap();
    assert!(matches!(
        host.remove("race"),
        Err(PluginError::UnadmittedPackage { .. })
    ));
    assert_eq!(tests::package_bytes(&root.path().join("race")), expected);
    assert!(!root.path().join(claim).exists());
}

#[test]
fn abandoned_empty_claim_and_legacy_stage_have_distinct_retry_outcomes() {
    let root = tempfile::tempdir().unwrap();
    broken(root.path());
    let legacy = root.path().join(".race.installing-4242");
    std::fs::create_dir(&legacy).unwrap();
    std::fs::write(legacy.join("keep"), b"legacy bytes").unwrap();
    Paused::start(root.path(), "claim-created", "remove").crash();
    let mut host = PluginHost::from_plugins_dir(root.path()).unwrap();
    assert_eq!(
        host.remove_with_report("race").unwrap().as_slice(),
        std::slice::from_ref(&legacy)
    );
    assert_eq!(std::fs::read(legacy.join("keep")).unwrap(), b"legacy bytes");
    assert!(matches!(host.remove("race"), Err(PluginError::NotFound(_))));
    assert!(legacy.exists());
}

#[test]
fn no_final_package_never_sweeps_even_provably_abandoned_stage() {
    let root = tempfile::tempdir().unwrap();
    let stage = Paused::start(root.path(), "stage-owned", "stage");
    let name = stage
        .lines
        .lines()
        .find_map(|line| line.strip_prefix("STAGE:"))
        .unwrap()
        .to_string();
    stage.crash();
    let mut host = PluginHost::from_plugins_dir(root.path()).unwrap();
    assert!(matches!(host.remove("race"), Err(PluginError::NotFound(_))));
    assert_eq!(
        std::fs::read(root.path().join(name).join("package/bytes")).unwrap(),
        b"active owner"
    );
}

#[cfg(windows)]
#[test]
fn windows_parent_capabilities_pin_renames_until_host_drop() {
    let parent = tempfile::tempdir().unwrap();
    let ancestor = parent.path().join("ancestor");
    std::fs::create_dir(&ancestor).unwrap();
    let root = ancestor.join("plugins");
    std::fs::create_dir(&root).unwrap();
    let host = PluginHost::from_plugins_dir(&root).unwrap();
    assert!(std::fs::rename(&ancestor, parent.path().join("moved")).is_err());
    drop(host);
    std::fs::rename(&ancestor, parent.path().join("moved")).unwrap();
}

#[test]
fn namespace_failure_is_never_a_structural_recovery_verdict() {
    assert!(!is_structural_admission_failure(
        &PluginError::NamespaceChanged("changed generation".into())
    ));
    assert!(!is_structural_admission_failure(
        &PluginError::RecoveryRetained {
            path: "synthetic".into(),
            reason: "refused".into()
        }
    ));
}

#[test]
fn claimed_package_completed_before_delete_is_restored_healthy() {
    let root = tempfile::tempdir().unwrap();
    broken(root.path());
    let paused = Paused::start(root.path(), "before-delete", "remove");
    let claim = tests::dir_entries(root.path())
        .into_iter()
        .find(|p| p.contains("recovering-v1"))
        .unwrap();
    let package = root.path().join(&claim).join("package");
    tests::write_tool_source(&package, "race", b"\0asm completed during stage IO");
    let expected = tests::package_bytes(&package);
    let output = paused.resume();
    assert!(output.contains("UnadmittedPackage"), "{output}");
    assert_eq!(tests::package_bytes(&root.path().join("race")), expected);
    assert!(!root.path().join(claim).exists());
}

#[test]
fn directory_to_file_substitution_and_occupied_restore_preserve_both_files() {
    let root = tempfile::tempdir().unwrap();
    broken(root.path());
    let mut paused = Paused::start(root.path(), "before-claim,after-claim", "remove");
    std::fs::rename(root.path().join("race"), root.path().join("old-directory")).unwrap();
    std::fs::write(root.path().join("race"), b"substituted file").unwrap();
    paused.advance("after-claim");
    std::fs::write(root.path().join("race"), b"concurrent occupant").unwrap();
    let output = paused.resume();
    assert!(output.contains("RecoveryRetained"), "{output}");
    assert_eq!(
        std::fs::read(root.path().join("race")).unwrap(),
        b"concurrent occupant"
    );
    let claim = tests::dir_entries(root.path())
        .into_iter()
        .find(|p| p.contains("recovering-v1"))
        .unwrap();
    assert_eq!(
        std::fs::read(root.path().join(claim).join("package")).unwrap(),
        b"substituted file"
    );
    assert_eq!(
        std::fs::read(root.path().join("old-directory/manifest.toml")).unwrap(),
        b"name ="
    );
}
