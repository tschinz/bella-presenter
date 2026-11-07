//! Small OS-specific helpers, kept behind `cfg` here per the project layout.

use std::path::Path;

/// Open the given file's containing folder in the OS file manager, revealing the file
/// where supported: Finder on macOS, Explorer on Windows, the default file manager on
/// Linux/BSD (which opens the directory, as there is no portable "reveal").
pub fn reveal_in_file_manager(path: &Path) {
  #[cfg(target_os = "macos")]
  {
    let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
  }
  #[cfg(target_os = "windows")]
  {
    let _ = std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn();
  }
  #[cfg(all(unix, not(target_os = "macos")))]
  {
    let dir = path.parent().unwrap_or(path);
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
  }
}

/// Whether `modifiers` turn the wheel into zoom and a drag into panning. Ctrl and Cmd as before;
/// Option as well, since macOS' accessibility zoom may claim Ctrl+scroll and Option+scroll is the
/// zoom gesture of Adobe's tools.
pub fn zoom_modifier(modifiers: gpui_kit::Modifiers) -> bool {
  modifiers.control || modifiers.platform || modifiers.alt
}

/// The zoom and pan gestures as the shortcut list names them.
pub const ZOOM_SCROLL: &str = if cfg!(target_os = "macos") {
  "⌥ / ⌘ / Ctrl + scroll"
} else {
  "Ctrl / Alt + scroll"
};
pub const ZOOM_DRAG: &str = if cfg!(target_os = "macos") {
  "⌥ / ⌘ / Ctrl + drag"
} else {
  "Ctrl / Alt + drag"
};

/// The local file a `file://` URL names (percent-decoded): macOS hands "Open with" and double-clicked
/// files over as such URLs instead of command-line arguments.
pub fn path_from_url(url: &str) -> Option<std::path::PathBuf> {
  let rest = url.strip_prefix("file://")?;
  let rest = rest.strip_prefix("localhost").unwrap_or(rest);
  let bytes = rest.as_bytes();
  let mut decoded = Vec::with_capacity(bytes.len());
  let mut i = 0;
  while let Some(&b) = bytes.get(i) {
    let hex = bytes
      .get(i + 1..i + 3)
      .and_then(|h| std::str::from_utf8(h).ok())
      .and_then(|h| u8::from_str_radix(h, 16).ok());
    match (b, hex) {
      (b'%', Some(byte)) => {
        decoded.push(byte);
        i += 3;
      }
      _ => {
        decoded.push(b);
        i += 1;
      }
    }
  }
  String::from_utf8(decoded).ok().map(std::path::PathBuf::from)
}

/// A command for a helper tool that, on Windows, opens no console window.
pub fn quiet_command(program: &str) -> std::process::Command {
  let cmd = std::process::Command::new(program);
  #[cfg(windows)]
  let cmd = {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = cmd;
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
  };
  cmd
}

/// Whether the release asset `name` is the build for this platform (see `release.yml`).
pub fn is_update_asset(name: &str) -> bool {
  if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    name.ends_with("-macos-arm64.zip")
  } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    name.ends_with("_amd64.deb")
  } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
    name.ends_with("-windows-x64.zip")
  } else {
    false
  }
}

/// Installs the downloaded release asset `file` over this app and returns the program to start
/// afterwards. macOS replaces the running `.app` bundle, Linux installs the `.deb` (asking for the
/// admin password through `pkexec`), Windows moves the running files aside and unpacks the zip.
pub fn install_update(file: &Path) -> Result<std::path::PathBuf, String> {
  // Taken before installing: on Linux the old executable's path reads as "(deleted)" afterwards.
  let exe = std::env::current_exe().map_err(|e| e.to_string())?;
  #[cfg(target_os = "macos")]
  {
    let bundle = exe
      .ancestors()
      .find(|p| p.extension().is_some_and(|e| e == "app"))
      .ok_or("not running from an .app bundle")?
      .to_path_buf();
    let parent = bundle.parent().ok_or("the app bundle has no folder")?;
    // Unpack next to the bundle so the swap is a rename on the same volume.
    let staging = parent.join(".bella-presenter-update");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("cannot write to {}: {e}", parent.display()))?;
    let swapped = (|| {
      run(quiet_command("ditto").arg("-x").arg("-k").arg(file).arg(&staging))?;
      let new_app = std::fs::read_dir(&staging)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .ok_or("the update contains no .app")?;
      let old = staging.join("previous.app");
      std::fs::rename(&bundle, &old).map_err(|e| format!("cannot replace {}: {e}", bundle.display()))?;
      if let Err(e) = std::fs::rename(&new_app, &bundle) {
        let _ = std::fs::rename(&old, &bundle);
        return Err(format!("cannot replace {}: {e}", bundle.display()));
      }
      Ok(())
    })();
    let _ = std::fs::remove_dir_all(&staging);
    swapped.map(|()| bundle)
  }
  #[cfg(target_os = "linux")]
  {
    run(quiet_command("pkexec").arg("dpkg").arg("-i").arg(file))?;
    Ok(exe)
  }
  #[cfg(windows)]
  {
    let dir = exe.parent().ok_or("the program has no folder")?.to_path_buf();
    let staging = file.with_extension("unpacked");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    // Windows 10+ ships bsdtar, which reads zip files.
    run(quiet_command("tar").arg("-xf").arg(file).arg("-C").arg(&staging))?;
    let root = staging.join("bella-presenter");
    for entry in std::fs::read_dir(&root).map_err(|e| format!("{}: {e}", root.display()))?.filter_map(Result::ok) {
      let target = dir.join(entry.file_name());
      // A running exe or loaded DLL cannot be overwritten, but it can be renamed.
      let old = target.with_extension("old");
      let _ = std::fs::remove_file(&old);
      if target.exists() {
        std::fs::rename(&target, &old).map_err(|e| format!("cannot replace {}: {e}", target.display()))?;
      }
      std::fs::copy(entry.path(), &target).map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    }
    Ok(exe)
  }
  #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
  {
    let _ = (file, exe);
    Err("updates are not supported on this platform".to_string())
  }
}

/// Starts `program` (from [`install_update`]) as a new instance; the caller then quits.
pub fn relaunch(program: &Path) {
  #[cfg(target_os = "macos")]
  let started = std::process::Command::new("open").arg("-n").arg(program).spawn();
  #[cfg(not(target_os = "macos"))]
  let started = std::process::Command::new(program).spawn();
  if let Err(e) = started {
    tracing::error!("cannot restart after the update: {e}");
  }
}

/// Runs a helper tool and turns a failure into its error output.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn run(cmd: &mut std::process::Command) -> Result<(), String> {
  let out = cmd.output().map_err(|e| format!("cannot run {:?}: {e}", cmd.get_program()))?;
  if out.status.success() {
    Ok(())
  } else {
    Err(format!("{:?} failed: {}", cmd.get_program(), String::from_utf8_lossy(&out.stderr).trim()))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn file_urls_are_percent_decoded() {
    assert_eq!(
      path_from_url("file:///Users/me/My%20Talk.pdf"),
      Some(std::path::PathBuf::from("/Users/me/My Talk.pdf"))
    );
    assert_eq!(path_from_url("https://example.com/a.pdf"), None);
  }
}
