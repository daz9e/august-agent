//! `uninstall` with a fake home, binaries and `stop`: what it asks, packs and deletes.

use super::uninstall;
use std::path::Path;

struct Setup {
    user: tempfile::TempDir,
    bin: tempfile::TempDir,
}

impl Setup {
    fn new() -> Self {
        let (user, bin) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::create_dir_all(user.path().join(".august/logs")).unwrap();
        std::fs::write(user.path().join(".august/august.db"), "data").unwrap();
        for f in ["august", "august-ext-memory", "other-tool"] {
            std::fs::write(bin.path().join(f), "bin").unwrap();
        }
        Setup { user, bin }
    }

    fn home(&self) -> std::path::PathBuf {
        self.user.path().join(".august")
    }

    fn run(&self, answers: &str, stop: &str) -> (anyhow::Result<()>, String) {
        let mut out = Vec::new();
        let r = uninstall(&self.home(), self.bin.path(), self.user.path(), stop, &mut answers.as_bytes(), &mut out);
        (r, String::from_utf8(out).unwrap())
    }

    fn backups(&self) -> Vec<std::path::PathBuf> {
        let list = std::fs::read_dir(self.user.path()).unwrap().flatten().map(|e| e.path());
        list.filter(|p| p.to_string_lossy().contains("august-backup-")).collect()
    }
}

fn exists(p: &Path, name: &str) -> bool {
    p.join(name).exists()
}

#[test]
fn backup_then_everything_goes() {
    let s = Setup::new();
    let (r, out) = s.run("y\nyes\n", "true");
    r.unwrap();
    assert!(out.contains("Backup:"));
    let backups = s.backups();
    assert_eq!(backups.len(), 1);
    let list = std::process::Command::new("tar").arg("-tzf").arg(&backups[0]).output().unwrap();
    assert!(String::from_utf8_lossy(&list.stdout).contains(".august/august.db"));
    assert!(!s.home().exists());
    assert!(!exists(s.bin.path(), "august") && !exists(s.bin.path(), "august-ext-memory"));
    assert!(exists(s.bin.path(), "other-tool"), "only August's own binaries go");
}

#[test]
fn no_backup_when_declined() {
    let s = Setup::new();
    s.run("n\nyes\n", "true").0.unwrap();
    assert!(s.backups().is_empty());
    assert!(!s.home().exists());
}

#[test]
fn nothing_changes_without_the_final_yes() {
    let s = Setup::new();
    let (r, out) = s.run("y\nno\n", "true");
    r.unwrap();
    assert!(out.contains("Nothing changed."));
    assert!(s.backups().is_empty() && s.home().exists() && exists(s.bin.path(), "august"));
}

#[test]
fn a_failed_stop_removes_nothing() {
    let s = Setup::new();
    assert!(s.run("y\nyes\n", "false").0.is_err());
    assert!(s.backups().is_empty() && s.home().exists() && exists(s.bin.path(), "august"));
}

#[test]
fn refuses_a_home_that_is_the_users_home() {
    let s = Setup::new();
    let mut out = Vec::new();
    let r = uninstall(s.user.path(), s.bin.path(), s.user.path(), "true", &mut "y\nyes\n".as_bytes(), &mut out);
    assert!(r.is_err() && s.user.path().exists());
}
