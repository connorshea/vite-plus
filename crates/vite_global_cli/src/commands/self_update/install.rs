//! Installation logic for self-update.
//!
//! Handles tarball extraction, dependency installation, symlink swapping,
//! and version cleanup.

use std::{
    io::{Cursor, Read as _},
    path::Path,
};

use flate2::read::GzDecoder;
use tar::Archive;
use vite_path::{AbsolutePath, AbsolutePathBuf};

use crate::error::Error;

/// Validate that a path from a tarball entry is safe (no path traversal).
///
/// Returns `false` if the path contains `..` components or is absolute.
fn is_safe_tar_path(path: &Path) -> bool {
    !path.is_absolute() && !path.components().any(|c| matches!(c, std::path::Component::ParentDir))
}

/// Files/directories to extract from the main package tarball.
const MAIN_PACKAGE_ENTRIES: &[&str] =
    &["dist/", "templates/", "rules/", "AGENTS.md", "package.json"];

/// Extract the platform-specific package (binary + .node files).
///
/// From the platform tarball, extracts:
/// - The `vp` binary → `{version_dir}/bin/vp`
/// - Any `.node` files → `{version_dir}/dist/`
pub async fn extract_platform_package(
    tgz_data: &[u8],
    version_dir: &AbsolutePath,
) -> Result<(), Error> {
    let bin_dir = version_dir.join("bin");
    let dist_dir = version_dir.join("dist");
    tokio::fs::create_dir_all(&bin_dir).await?;
    tokio::fs::create_dir_all(&dist_dir).await?;

    let data = tgz_data.to_vec();
    let bin_dir_clone = bin_dir.clone();
    let dist_dir_clone = dist_dir.clone();

    tokio::task::spawn_blocking(move || {
        let cursor = Cursor::new(data);
        let decoder = GzDecoder::new(cursor);
        let mut archive = Archive::new(decoder);

        for entry_result in archive.entries()? {
            let mut entry = entry_result?;
            let path = entry.path()?.to_path_buf();

            // Strip the leading `package/` prefix that npm tarballs have
            let relative = path.strip_prefix("package").unwrap_or(&path).to_path_buf();

            // Reject paths with traversal components (security)
            if !is_safe_tar_path(&relative) {
                continue;
            }

            let file_name = relative.file_name().and_then(|n| n.to_str()).unwrap_or("");

            if file_name == "vp" || file_name == "vp.exe" {
                // Binary goes to bin/
                let target = bin_dir_clone.join(file_name);
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf)?;
                std::fs::write(&target, &buf)?;

                // Set executable permission on Unix
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
                }
            } else if file_name.ends_with(".node") {
                // .node NAPI files go to dist/
                let target = dist_dir_clone.join(file_name);
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf)?;
                std::fs::write(&target, &buf)?;
            }
        }

        Ok::<(), Error>(())
    })
    .await
    .map_err(|e| Error::SelfUpdate(format!("Task join error: {e}").into()))??;

    Ok(())
}

/// Extract the main package (JS bundles, templates, rules, package.json).
///
/// Copies specific directories and files from the tarball to the version directory.
pub async fn extract_main_package(
    tgz_data: &[u8],
    version_dir: &AbsolutePath,
) -> Result<(), Error> {
    let version_dir_owned = version_dir.as_path().to_path_buf();
    let data = tgz_data.to_vec();

    tokio::task::spawn_blocking(move || {
        let cursor = Cursor::new(data);
        let decoder = GzDecoder::new(cursor);
        let mut archive = Archive::new(decoder);

        for entry_result in archive.entries()? {
            let mut entry = entry_result?;
            let path = entry.path()?.to_path_buf();

            // Strip the leading `package/` prefix
            let relative = path.strip_prefix("package").unwrap_or(&path).to_path_buf();

            // Reject paths with traversal components (security)
            if !is_safe_tar_path(&relative) {
                continue;
            }

            let relative_str = relative.to_string_lossy();

            // Check if this entry matches our allowed list
            let should_extract = MAIN_PACKAGE_ENTRIES.iter().any(|allowed| {
                if allowed.ends_with('/') {
                    // Directory prefix match
                    relative_str.starts_with(allowed)
                } else {
                    // Exact file match
                    relative_str == *allowed
                }
            });

            if !should_extract {
                continue;
            }

            let target = version_dir_owned.join(&*relative_str);

            if entry.header().entry_type().is_dir() {
                std::fs::create_dir_all(&target)?;
            } else {
                // Ensure parent directory exists
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf)?;
                std::fs::write(&target, &buf)?;
            }
        }

        Ok::<(), Error>(())
    })
    .await
    .map_err(|e| Error::SelfUpdate(format!("Task join error: {e}").into()))??;

    Ok(())
}

/// Strip devDependencies and optionalDependencies from package.json.
pub async fn strip_dev_dependencies(version_dir: &AbsolutePath) -> Result<(), Error> {
    let package_json_path = version_dir.join("package.json");

    if !tokio::fs::try_exists(&package_json_path).await.unwrap_or(false) {
        return Ok(());
    }

    let content = tokio::fs::read_to_string(&package_json_path).await?;
    let mut json: serde_json::Value = serde_json::from_str(&content)?;

    if let Some(obj) = json.as_object_mut() {
        obj.remove("devDependencies");
        obj.remove("optionalDependencies");
    }

    let updated = serde_json::to_string_pretty(&json)?;
    tokio::fs::write(&package_json_path, format!("{updated}\n")).await?;

    Ok(())
}

/// Install production dependencies using the new version's binary.
///
/// Spawns: `{version_dir}/bin/vp install --silent` with `CI=true`.
pub async fn install_production_deps(version_dir: &AbsolutePath) -> Result<(), Error> {
    let vp_binary = version_dir.join("bin").join(if cfg!(windows) { "vp.exe" } else { "vp" });

    if !tokio::fs::try_exists(&vp_binary).await.unwrap_or(false) {
        return Err(Error::SelfUpdate(
            format!("New binary not found at {}", vp_binary.as_path().display()).into(),
        ));
    }

    tracing::debug!("Running vp install in {}", version_dir.as_path().display());

    let status = tokio::process::Command::new(vp_binary.as_path())
        .args(["install", "--silent"])
        .current_dir(version_dir)
        .env("CI", "true")
        .status()
        .await?;

    if !status.success() {
        return Err(Error::SelfUpdate(
            format!(
                "Failed to install production dependencies (exit code: {})",
                status.code().unwrap_or(-1)
            )
            .into(),
        ));
    }

    Ok(())
}

/// Save the current version before swapping, for rollback support.
///
/// Reads the `current` symlink target and writes the version to `.previous-version`.
pub async fn save_previous_version(install_dir: &AbsolutePath) -> Result<Option<String>, Error> {
    let current_link = install_dir.join("current");

    if !tokio::fs::try_exists(&current_link).await.unwrap_or(false) {
        return Ok(None);
    }

    let target = tokio::fs::read_link(&current_link).await?;
    let version = target.file_name().and_then(|n| n.to_str()).map(String::from);

    if let Some(ref v) = version {
        let prev_file = install_dir.join(".previous-version");
        tokio::fs::write(&prev_file, v).await?;
        tracing::debug!("Saved previous version: {}", v);
    }

    Ok(version)
}

/// Atomically swap the `current` symlink to point to a new version.
///
/// On Unix: creates a temp symlink then renames (atomic).
/// On Windows: removes junction and creates a new one.
pub async fn swap_current_link(install_dir: &AbsolutePath, version: &str) -> Result<(), Error> {
    let current_link = install_dir.join("current");
    let version_dir = install_dir.join(version);

    // Verify the version directory exists
    if !tokio::fs::try_exists(&version_dir).await.unwrap_or(false) {
        return Err(Error::SelfUpdate(
            format!("Version directory does not exist: {}", version_dir.as_path().display()).into(),
        ));
    }

    #[cfg(unix)]
    {
        // Atomic symlink swap: create temp link, then rename over current
        let temp_link = install_dir.join("current.new");

        // Remove temp link if it exists from a previous failed attempt
        let _ = tokio::fs::remove_file(&temp_link).await;

        tokio::fs::symlink(version, &temp_link).await?;
        tokio::fs::rename(&temp_link, &current_link).await?;
    }

    #[cfg(windows)]
    {
        // Windows: junction swap (not atomic)
        use std::process::Command;

        // Remove existing junction (use symlink_metadata to detect broken junctions too)
        if std::fs::symlink_metadata(current_link.as_path()).is_ok() {
            let status = Command::new("cmd")
                .args(["/c", "rmdir", &current_link.as_path().display().to_string()])
                .status()?;
            if !status.success() {
                return Err(Error::SelfUpdate("Failed to remove existing junction".into()));
            }
        }

        // Create new junction
        let status = Command::new("cmd")
            .args([
                "/c",
                "mklink",
                "/J",
                &current_link.as_path().display().to_string(),
                &version_dir.as_path().display().to_string(),
            ])
            .status()?;
        if !status.success() {
            return Err(Error::SelfUpdate("Failed to create junction".into()));
        }
    }

    tracing::debug!("Swapped current → {}", version);
    Ok(())
}

/// Refresh shims by running `vp env setup --refresh` with the new binary.
pub async fn refresh_shims(install_dir: &AbsolutePath) -> Result<(), Error> {
    let vp_binary =
        install_dir.join("current").join("bin").join(if cfg!(windows) { "vp.exe" } else { "vp" });

    if !tokio::fs::try_exists(&vp_binary).await.unwrap_or(false) {
        tracing::warn!(
            "New binary not found at {}, skipping shim refresh",
            vp_binary.as_path().display()
        );
        return Ok(());
    }

    tracing::debug!("Refreshing shims...");

    let status = tokio::process::Command::new(vp_binary.as_path())
        .args(["env", "setup", "--refresh"])
        .status()
        .await?;

    if !status.success() {
        tracing::warn!(
            "Shim refresh exited with code {}, continuing anyway",
            status.code().unwrap_or(-1)
        );
    }

    Ok(())
}

/// Clean up old version directories, keeping at most `max_keep` versions.
///
/// Sorts by semver (newest first) and removes the oldest beyond the limit.
pub async fn cleanup_old_versions(
    install_dir: &AbsolutePath,
    max_keep: usize,
) -> Result<(), Error> {
    let mut versions: Vec<(node_semver::Version, AbsolutePathBuf)> = Vec::new();

    let mut entries = tokio::fs::read_dir(install_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Only consider entries that parse as semver
        if let Ok(ver) = node_semver::Version::parse(&name_str) {
            let path = AbsolutePathBuf::new(entry.path()).ok_or_else(|| {
                Error::SelfUpdate(
                    format!("Invalid absolute path: {}", entry.path().display()).into(),
                )
            })?;
            versions.push((ver, path));
        }
    }

    // Sort newest first
    versions.sort_by(|a, b| b.0.cmp(&a.0));

    // Remove versions beyond the keep limit
    for (_ver, path) in versions.into_iter().skip(max_keep) {
        tracing::debug!("Cleaning up old version: {}", path.as_path().display());
        if let Err(e) = tokio::fs::remove_dir_all(&path).await {
            tracing::warn!("Failed to remove {}: {}", path.as_path().display(), e);
        }
    }

    Ok(())
}

/// Read the previous version from `.previous-version` file.
pub async fn read_previous_version(install_dir: &AbsolutePath) -> Result<Option<String>, Error> {
    let prev_file = install_dir.join(".previous-version");

    if !tokio::fs::try_exists(&prev_file).await.unwrap_or(false) {
        return Ok(None);
    }

    let content = tokio::fs::read_to_string(&prev_file).await?;
    let version = content.trim().to_string();

    if version.is_empty() { Ok(None) } else { Ok(Some(version)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_safe_tar_path_normal() {
        assert!(is_safe_tar_path(Path::new("dist/index.js")));
        assert!(is_safe_tar_path(Path::new("bin/vp")));
        assert!(is_safe_tar_path(Path::new("package.json")));
        assert!(is_safe_tar_path(Path::new("templates/react/index.ts")));
    }

    #[test]
    fn test_is_safe_tar_path_traversal() {
        assert!(!is_safe_tar_path(Path::new("../etc/passwd")));
        assert!(!is_safe_tar_path(Path::new("dist/../../etc/passwd")));
        assert!(!is_safe_tar_path(Path::new("..")));
    }

    #[test]
    fn test_is_safe_tar_path_absolute() {
        assert!(!is_safe_tar_path(Path::new("/etc/passwd")));
        assert!(!is_safe_tar_path(Path::new("/usr/bin/vp")));
    }
}
