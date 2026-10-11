//! `uninstall`: `august uninstall` removes August from this machine: it stops the service (and
//! its autostart), offers a backup of the data as a `.tar.gz` in the home directory, and after
//! a typed confirmation deletes the data (`$AUGUST_HOME`) and the binaries (`august` and
//! `august-ext-*`). Only a terminal command: neither a tool nor a chat command, so the agent
//! cannot remove itself.

#[cfg(test)]
mod tests;

use anyhow::{Context, bail};
use august_ext::August;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() != Some("run") {
        let august = August::new();
        august.describe(
            "`august uninstall`: remove August, with an optional backup",
            "Stops August, optionally packs its data into ~/august-backup-<time>.tar.gz, then deletes the data and the binaries.",
        );
        let exe = std::env::current_exe()?.display().to_string();
        august.register_cli("uninstall", "Remove August and all its data, with an optional backup", &[exe, "run".into()]);
        august.run().await;
        return Ok(());
    }
    let user = PathBuf::from(std::env::var("HOME").context("HOME is not set")?);
    let home = std::env::var("AUGUST_HOME").map(PathBuf::from).unwrap_or_else(|_| user.join(".august"));
    let exe = std::env::current_exe()?;
    let bin = exe.parent().unwrap_or(Path::new(".")).to_path_buf();
    let stop = format!("'{}' stop", bin.join("august").display().to_string().replace('\'', r"'\''"));
    let stdin = std::io::stdin();
    uninstall(&home, &bin, &user, &stop, &mut stdin.lock(), &mut std::io::stdout())
}

fn ask(input: &mut impl BufRead, out: &mut impl Write, question: &str) -> anyhow::Result<String> {
    write!(out, "{question}")?;
    out.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(answer.trim().to_lowercase())
}

/// The binaries to delete from `bin`: `august` and `august-ext-*`; none in a checkout.
fn binaries(bin: &Path) -> Vec<PathBuf> {
    if bin.join("../../Cargo.toml").is_file() {
        return vec![];
    }
    let Ok(dir) = std::fs::read_dir(bin) else { return vec![] };
    let name = |p: &PathBuf| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    dir.flatten().map(|e| e.path()).filter(|p| name(p) == "august" || name(p).starts_with("august-ext-")).collect()
}

/// Asks, stops (`stop`: a shell command), backs up into `user` if wanted, deletes `home` and
/// the binaries of `bin`. Answering anything but "yes" to the last question changes nothing.
fn uninstall(home: &Path, bin: &Path, user: &Path, stop: &str, input: &mut impl BufRead, out: &mut impl Write) -> anyhow::Result<()> {
    if home == user || home.parent().is_none() || !home.is_dir() {
        bail!("{} is not an August data directory, nothing to remove", home.display());
    }
    let bins = binaries(bin);
    writeln!(out, "This removes August from this machine:\n  data:     {}", home.display())?;
    writeln!(out, "  binaries: {} files in {}", bins.len(), bin.display())?;
    let backup = matches!(ask(input, out, "Pack the data into a backup in your home directory first? [Y/n] ")?.as_str(), "" | "y" | "yes");
    if ask(input, out, "Delete everything for good? Type 'yes' to confirm: ")? != "yes" {
        writeln!(out, "Nothing changed.")?;
        return Ok(());
    }

    // Stopped first, so nothing writes to the data while it is packed and deleted.
    let stopped = std::process::Command::new("sh").args(["-c", stop]).status()?;
    if !stopped.success() {
        bail!("could not stop August ({stopped}), nothing was removed");
    }
    if backup {
        let file = user.join(format!("august-backup-{}.tar.gz", chrono::Local::now().format("%Y%m%d-%H%M%S")));
        let name = home.file_name().context("no name")?;
        let tar = std::process::Command::new("tar").arg("-czf").arg(&file).arg("-C").arg(home.parent().unwrap()).arg(name).status()?;
        if !tar.success() {
            let _ = std::fs::remove_file(&file);
            bail!("the backup failed ({tar}), nothing was removed");
        }
        #[cfg(unix)]
        std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o600))?; // it holds tokens
        writeln!(out, "Backup: {}", file.display())?;
    }
    std::fs::remove_dir_all(home)?;
    for b in &bins {
        std::fs::remove_file(b)?;
    }
    writeln!(out, "August is removed.")?;
    Ok(())
}
