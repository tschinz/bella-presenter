//! The update check against the latest GitHub release, and installing it in place.
//!
//! Downloads go through the system `curl` (shipped with macOS, Windows 10+ and the Linux
//! desktops), as `install.sh` does, so the app carries no HTTP/TLS stack. Releases are not
//! signed: an update is trusted as much as the GitHub release (HTTPS only). Both functions block;
//! run them in the background.

use std::path::{Path, PathBuf};

use serde::Deserialize;

const LATEST: &str = "https://api.github.com/repos/tschinz/bella-presenter/releases/latest";

/// A newer release with a build for this platform.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
  pub version: String,
  pub notes: String,
  /// Download URL of this platform's asset.
  pub url: String,
  /// The asset's file name.
  pub file: String,
}

#[derive(Deserialize)]
struct GhRelease {
  tag_name: String,
  #[serde(default)]
  body: Option<String>,
  #[serde(default)]
  assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
  name: String,
  browser_download_url: String,
}

/// The latest release if it is newer than this build and has an asset for this platform.
pub fn check() -> Result<Option<Release>, String> {
  let json = curl(&["--max-time", "20", "-H", "Accept: application/vnd.github+json", LATEST])?;
  newer_release(&json, env!("CARGO_PKG_VERSION"))
}

/// Downloads `release` into a fresh temporary folder and installs it over this app. Returns the
/// program to start afterwards (see [`crate::platform::relaunch`]).
pub fn install(release: &Release) -> Result<PathBuf, String> {
  let dir = std::env::temp_dir().join(format!("bella-presenter-update-{}", release.version));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
  // Asset names come from the release: keep only the last path component.
  let name = Path::new(&release.file).file_name().ok_or("bad asset name")?;
  let file = dir.join(name);
  let target = file.to_string_lossy();
  curl(&["--max-time", "900", "-o", &target, &release.url])?;
  let result = crate::platform::install_update(&file);
  let _ = std::fs::remove_dir_all(&dir);
  result
}

/// Runs `curl` over HTTPS only, failing on HTTP errors, and returns its output.
fn curl(args: &[&str]) -> Result<String, String> {
  let out = crate::platform::quiet_command("curl")
    .args(["--proto", "=https", "--tlsv1.2", "-fsSL"])
    .args(args)
    .output()
    .map_err(|e| format!("cannot run curl: {e}"))?;
  if !out.status.success() {
    return Err(format!("curl: {}", String::from_utf8_lossy(&out.stderr).trim()));
  }
  String::from_utf8(out.stdout).map_err(|e| e.to_string())
}

/// The release in the GitHub API answer `json`, if it is newer than `current` and has an asset
/// for this platform.
fn newer_release(json: &str, current: &str) -> Result<Option<Release>, String> {
  let release: GhRelease = serde_json::from_str(json).map_err(|e| format!("unexpected release info: {e}"))?;
  let version = release.tag_name.trim_start_matches('v').to_string();
  if !is_newer(&version, current) {
    return Ok(None);
  }
  let asset = release.assets.into_iter().find(|a| crate::platform::is_update_asset(&a.name));
  Ok(asset.map(|a| Release {
    version,
    notes: release.body.unwrap_or_default(),
    url: a.browser_download_url,
    file: a.name,
  }))
}

/// Whether version `candidate` is newer than `current` (`major.minor.patch`; a pre-release
/// suffix such as `-rc1` is ignored). Unreadable versions are never newer.
fn is_newer(candidate: &str, current: &str) -> bool {
  match (parse_version(candidate), parse_version(current)) {
    (Some(a), Some(b)) => a > b,
    _ => false,
  }
}

fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
  let core = v.trim_start_matches('v').split(['-', '+']).next()?;
  let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
  Some((parts.next()??, parts.next().flatten().unwrap_or(0), parts.next().flatten().unwrap_or(0)))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn compares_versions() {
    assert!(is_newer("0.4.0", "0.3.9"));
    assert!(is_newer("v1.0.0", "0.9.12"));
    assert!(is_newer("0.3.10", "0.3.9"));
    assert!(!is_newer("0.3.0", "0.3.0"));
    assert!(!is_newer("0.2.9", "0.3.0"));
    assert!(!is_newer("0.4.0-rc1", "0.4.0"));
    assert!(!is_newer("latest", "0.3.0"));
  }

  #[test]
  fn picks_this_platforms_asset_from_a_newer_release() {
    let json = r#"{"tag_name":"v9.0.0","body":"Notes","assets":[
      {"name":"bella-presenter-macos-arm64.zip","browser_download_url":"https://x/mac.zip"},
      {"name":"bella-presenter_9.0.0_amd64.deb","browser_download_url":"https://x/linux.deb"},
      {"name":"bella-presenter-windows-x64.zip","browser_download_url":"https://x/win.zip"}]}"#;
    let found = newer_release(json, "0.3.0").expect("valid json");
    let supported = cfg!(any(
      all(target_os = "macos", target_arch = "aarch64"),
      all(target_os = "linux", target_arch = "x86_64"),
      all(target_os = "windows", target_arch = "x86_64")
    ));
    assert_eq!(found.is_some(), supported, "{found:?}");
    if let Some(r) = found {
      assert_eq!((r.version.as_str(), r.notes.as_str()), ("9.0.0", "Notes"));
      assert!(crate::platform::is_update_asset(&r.file));
    }
    assert_eq!(newer_release(json, "9.0.0").expect("valid json"), None);
  }
}
