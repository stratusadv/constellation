//! The release binary: compiling it, and placing it where agents launch it.
//!
//! Both tasks are one problem in disguise. A running MCP server holds its
//! executable open for the life of an editor session, and Windows refuses to
//! overwrite an open file, failing cargo's link step with "Access is denied".
//! Renaming an open file is permitted, so every write here displaces the
//! previous copy first and collects the displaced ones once nothing holds them.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{Result, workspace_root};

/// The suffix every displaced copy carries, and the marker a sweep matches on.
const DISPLACED_SUFFIX: &str = ".old";

/// The variable cargo-xwin reads to confirm the Microsoft CRT and Windows SDK
/// license was accepted.
const LICENSE_VARIABLE: &str = "XWIN_ACCEPT_LICENSE";

/// The Windows target a cross build produces, matching the `.cargo/config.toml`
/// entry that links it against the static CRT.
const TARGET_WINDOWS: &str = "x86_64-pc-windows-msvc";

/// A cap on how many displaced copies one sweep removes. Far above the handful
/// a real working tree accumulates, so reaching it means the naming scheme is
/// matching something it should not.
const DISPLACED_SWEEP_MAX: u32 = 1_024;

/// The `build` task: one target, the host's when the caller names none.
pub fn build_task(target: Option<&str>) -> Result {
    let target = match target {
        Some(target) => target.to_string(),
        None => host_target()?,
    };

    let artifact = build(&target)?;

    println!("built {}", artifact.display());

    Ok(())
}

/// The two deliverables from one checkout: the host binary and the Windows one.
///
/// A Windows host builds once, because its host binary is already the Windows
/// binary. Anywhere else the second build cross-compiles through cargo-xwin.
pub fn dist() -> Result {
    let host = host_target()?;
    let host_artifact = build(&host)?;

    println!("built {}", host_artifact.display());

    if target_is_windows(&host) {
        return Ok(());
    }

    let windows_artifact = build(TARGET_WINDOWS)?;

    println!("built {}", windows_artifact.display());
    println!("note: the icon is embedded only by a Windows host, so this one carries none");

    Ok(())
}

/// The release binary for one target, compiled into `target/<triple>/release`.
///
/// Every build names its triple, the host's included. The checkout sits on a
/// drive Linux and Windows both mount, and an unqualified path would have each
/// platform overwrite the binary the other just built.
///
/// The previous copy is displaced before cargo runs and put back if the build
/// fails, so a failed build never leaves the tree without a working binary.
pub fn build(target: &str) -> Result<PathBuf> {
    assert!(!target.is_empty(), "a build always names a target");

    let root = workspace_root()?;
    let artifact = release_artifact(&root, target);

    let displaced = displace(&artifact)?;

    let status = cargo_command(target)?
        .args(["build", "--release", "--target", target])
        .current_dir(&root)
        .status()?;

    if !status.success() {
        restore(displaced.as_deref(), &artifact)?;

        return Err(format!("cargo build --release for {target} failed: {status}").into());
    }

    let directory = artifact.parent().ok_or("the release artifact has no directory")?;
    sweep_displaced(directory, &binary_name(target))?;

    assert!(artifact.is_file(), "a successful build leaves the artifact in place");

    Ok(artifact)
}

/// The freshly built release binary, copied into a directory cargo does not
/// rewrite, so an agent launching it is unaffected by the next build.
pub fn install(destination: Option<&str>) -> Result {
    let host = host_target()?;
    let artifact = build(&host)?;

    let directory = match destination {
        Some(path) => PathBuf::from(path),
        None => default_install_directory()?,
    };

    fs::create_dir_all(&directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;

    let name = binary_name(&host);
    let installed = directory.join(&name);
    let _ = displace(&installed)?;

    // Both paths are named on failure. This copy is where an install goes
    // wrong, and its errno alone ("No such file or directory") says nothing
    // about which of the two files was missing or why.
    let bytes = fs::copy(&artifact, &installed).map_err(|error| {
        format!("could not copy {} to {}: {error}", artifact.display(), installed.display())
    })?;

    assert!(bytes > 0, "an installed binary is never empty");

    sweep_displaced(&directory, &name)?;

    println!("installed {}", installed.display());

    Ok(())
}

/// The path the release profile writes the CLI binary to, for one target.
pub(crate) fn release_artifact(root: &Path, target: &str) -> PathBuf {
    let name = binary_name(target);
    let artifact = root.join("target").join(target).join("release").join(&name);

    assert!(artifact.ends_with(&name), "the artifact path names the binary");

    artifact
}

/// The binary's file name for a target. The extension follows the target rather
/// than the host, so a cross build on Linux still produces constellation.exe.
pub(crate) fn binary_name(target: &str) -> String {
    if target_is_windows(target) {
        return "constellation.exe".to_string();
    }

    "constellation".to_string()
}

/// Whether a target triple names Windows.
fn target_is_windows(target: &str) -> bool {
    target.contains("-windows-")
}

/// The triple rustc reports for the host, so every build names its target
/// explicitly instead of leaning on cargo's implicit default.
pub(crate) fn host_target() -> Result<String> {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let output = Command::new(rustc).arg("-vV").output()?;

    if !output.status.success() {
        return Err(format!("rustc -vV failed: {}", output.status).into());
    }

    let report = String::from_utf8(output.stdout)?;

    for line in report.lines() {
        let Some(target) = line.strip_prefix("host: ") else {
            continue;
        };

        assert!(!target.is_empty(), "rustc always names a non-empty host");

        return Ok(target.to_string());
    }

    Err("rustc -vV named no host triple".into())
}

/// The cargo invocation for a target.
///
/// A Windows target on a Windows host is an ordinary build. Anywhere else it
/// goes through cargo-xwin, which supplies the MSVC toolchain the linker needs.
fn cargo_command(target: &str) -> Result<Command> {
    let mut command = Command::new(cargo());

    if target_is_windows(target) && !cfg!(windows) {
        license_accepted()?;

        command.arg("xwin");
    }

    Ok(command)
}

/// The Microsoft CRT and Windows SDK license confirmed as accepted.
///
/// cargo-xwin downloads both, and only the person building can agree to their
/// terms, so the task says what the variable means rather than setting it.
fn license_accepted() -> Result {
    if let Ok(accepted) = std::env::var(LICENSE_VARIABLE)
        && !accepted.is_empty()
    {
        return Ok(());
    }

    Err(format!(
        "cross-compiling to {TARGET_WINDOWS} downloads the Microsoft CRT and \
         Windows SDK through cargo-xwin. Set {LICENSE_VARIABLE}=1 to accept \
         their license, or build on Windows."
    )
    .into())
}

/// The cargo to shell out to: the one that launched this task when known, so a
/// non-default toolchain builds with itself rather than with whatever is first
/// on PATH.
fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

/// The directory an install lands in when the caller names none.
fn default_install_directory() -> Result<PathBuf> {
    if let Ok(configured) = std::env::var("CONSTELLATION_INSTALL_DIR") {
        return Ok(PathBuf::from(configured));
    }

    let home_variable = if cfg!(windows) { "USERPROFILE" } else { "HOME" };

    let home = std::env::var(home_variable)
        .map_err(|_| format!("{home_variable} is unset, so there is no default install dir"))?;

    Ok(PathBuf::from(home).join(".local").join("bin"))
}

/// The path an existing file was renamed to, or `None` when there was no file
/// to move aside.
///
/// The timestamp only has to make the name unique against the copies already
/// sitting in the directory, which a nanosecond clock does comfortably.
fn displace(path: &Path) -> Result<Option<PathBuf>> {
    // `symlink_metadata` rather than `exists`, which follows symlinks.
    //
    // A previous install can leave a symlink whose target has since gone: the
    // checkout moved to another drive, or the tree it pointed into was deleted.
    // `exists` reports such a link as absent, so it was left in place, and then
    // `fs::copy` followed it too and tried to create the file under a directory
    // that is no longer there. The install failed with a bare
    // "No such file or directory" naming neither the link nor its target.
    //
    // A dangling link is not an absent file. It is a directory entry that must
    // be moved aside like any other, which is what asking about the entry
    // itself does.
    if fs::symlink_metadata(path).is_err() {
        return Ok(None);
    }

    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();

    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{stamp}{DISPLACED_SUFFIX}"));

    let displaced = PathBuf::from(name);
    fs::rename(path, &displaced)?;

    assert!(fs::symlink_metadata(path).is_err(), "a displaced entry is moved, not copied");
    assert!(fs::symlink_metadata(&displaced).is_ok(), "a displaced entry keeps its place");

    Ok(Some(displaced))
}

/// The displaced copy put back, after a build that failed to produce a new one.
fn restore(displaced: Option<&Path>, artifact: &Path) -> Result {
    let Some(displaced) = displaced else {
        return Ok(());
    };

    if artifact.exists() {
        return Ok(());
    }

    fs::rename(displaced, artifact)?;

    assert!(artifact.exists(), "the previous binary is back in place");

    Ok(())
}

/// The displaced copies in `directory` removed.
///
/// A copy still held open by a live session refuses to delete; that is expected
/// rather than an error, and a later sweep collects it once the session exits.
fn sweep_displaced(directory: &Path, binary_name: &str) -> Result {
    assert!(!binary_name.is_empty(), "a sweep always names a binary");

    let mut examined: u32 = 0;

    for entry in fs::read_dir(directory)? {
        examined += 1;

        assert!(examined <= DISPLACED_SWEEP_MAX, "a sweep examines a bounded directory");

        let path = entry?.path();

        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };

        if name.starts_with(binary_name) && name.ends_with(DISPLACED_SUFFIX) {
            let _ = fs::remove_file(&path);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{DISPLACED_SUFFIX, displace};

    #[test]
    fn nothing_to_displace_when_the_path_is_free() {
        let directory = tempfile::tempdir().unwrap();

        let displaced = displace(&directory.path().join("constellation")).unwrap();

        assert!(displaced.is_none(), "an absent path has nothing to move aside");
    }

    #[test]
    fn an_ordinary_file_is_moved_aside() {
        let directory = tempfile::tempdir().unwrap();
        let installed = directory.path().join("constellation");

        fs::write(&installed, b"previous").unwrap();

        let displaced = displace(&installed).unwrap().expect("the file was moved aside");

        assert!(!installed.exists(), "the install path is free for the new binary");
        assert_eq!(fs::read(&displaced).unwrap(), b"previous", "the old binary is kept");

        assert!(
            displaced.to_string_lossy().ends_with(DISPLACED_SUFFIX),
            "so the sweep can collect it: {}",
            displaced.display(),
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_whose_target_is_gone_is_still_moved_aside() {
        let directory = tempfile::tempdir().unwrap();
        let installed = directory.path().join("constellation");

        // Exactly the state a checkout that moved to another drive leaves
        // behind: a link that still occupies the install path while resolving
        // to nothing. `Path::exists` calls it absent; leaving it in place made
        // the copy that follows fail against the missing target.
        std::os::unix::fs::symlink(directory.path().join("gone/constellation"), &installed)
            .unwrap();

        assert!(!installed.exists(), "the link resolves to nothing");

        let displaced = displace(&installed).unwrap().expect("a dangling link is still an entry");

        assert!(
            fs::symlink_metadata(&installed).is_err(),
            "the install path is free, so the copy that follows can create a real file",
        );

        assert!(
            fs::symlink_metadata(&displaced).is_ok(),
            "and the link itself was moved rather than deleted",
        );
    }
}
