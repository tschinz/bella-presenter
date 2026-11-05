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

/// Files the OS asked this app to open while it runs, waiting for the UI to pick them up.
static OPENED_FILES: std::sync::Mutex<Vec<std::path::PathBuf>> = std::sync::Mutex::new(Vec::new());
/// The UI's context, woken when a file arrives so an idle app opens it at once.
static REPAINT: std::sync::OnceLock<egui::Context> = std::sync::OnceLock::new();

/// Starts listening for files the OS hands over through its own channel instead of the command
/// line: macOS sends "Open with" and double-clicks as an Apple Event, which winit ignores. Call
/// before the event loop starts, so the file a launch was asked for is not missed.
pub fn listen_for_opened_files() {
  #[cfg(target_os = "macos")]
  mac_open::listen();
}

/// Lets a newly opened file wake the UI through `ctx`.
pub fn wake_on_opened_files(ctx: &egui::Context) {
  let _ = REPAINT.set(ctx.clone());
}

/// The files the OS asked to open since the last call, oldest first.
pub fn take_opened_files() -> Vec<std::path::PathBuf> {
  std::mem::take(&mut *OPENED_FILES.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
}

#[cfg(target_os = "macos")]
fn push_opened_file(path: std::path::PathBuf) {
  OPENED_FILES.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(path);
  if let Some(ctx) = REPAINT.get() {
    ctx.request_repaint();
  }
}

#[cfg(target_os = "macos")]
mod mac_open {
  use objc2::rc::Retained;
  use objc2::runtime::{AnyObject, NSObject};
  use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
  use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventManager, NSNotification, NSNotificationCenter, NSObjectProtocol, ns_string};

  /// Four-char codes from the Apple Event headers: `'aevt'`, `'odoc'` and `'----'`.
  const CORE_EVENT_CLASS: u32 = u32::from_be_bytes(*b"aevt");
  const OPEN_DOCUMENTS: u32 = u32::from_be_bytes(*b"odoc");
  const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");

  define_class!(
    // SAFETY: NSObject has no subclassing requirements, and this class adds no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "BellaPresenterOpenHandler"]
    struct OpenHandler;

    // SAFETY: NSObjectProtocol has no extra requirements.
    unsafe impl NSObjectProtocol for OpenHandler {}

    impl OpenHandler {
      /// AppKit installs its own Apple Event handlers while launching, so ours goes in after
      /// that, still before the launch's "open documents" event is delivered.
      // SAFETY: signature matches the notification observer selector.
      #[unsafe(method(willFinishLaunching:))]
      fn will_finish_launching(&self, _note: &NSNotification) {
        // SAFETY: `self` implements the selector with the `(event, reply)` signature
        // NSAppleEventManager calls, and lives for the whole process (leaked in `listen`).
        unsafe {
          NSAppleEventManager::sharedAppleEventManager().setEventHandler_andSelector_forEventClass_andEventID(
            self,
            sel!(handleOpenDocuments:withReplyEvent:),
            CORE_EVENT_CLASS,
            OPEN_DOCUMENTS,
          );
        }
      }

      // SAFETY: signature matches what NSAppleEventManager calls an event handler with.
      #[unsafe(method(handleOpenDocuments:withReplyEvent:))]
      fn handle_open_documents(&self, event: &NSAppleEventDescriptor, _reply: &NSAppleEventDescriptor) {
        let Some(list) = event.paramDescriptorForKeyword(DIRECT_OBJECT) else { return };
        // Apple Event lists are 1-based.
        for i in 1..=list.numberOfItems() {
          if let Some(path) = list.descriptorAtIndex(i).and_then(|d| d.fileURLValue()).and_then(|u| u.to_file_path()) {
            super::push_opened_file(path);
          }
        }
      }
    }
  );

  pub fn listen() {
    let Some(mtm) = MainThreadMarker::new() else {
      tracing::warn!("open-file events need the main thread");
      return;
    };
    // SAFETY: plain NSObject subclass without ivars; `init` is its designated initializer.
    let handler: Retained<OpenHandler> = unsafe { msg_send![OpenHandler::alloc(mtm), init] };
    let observer: &AnyObject = &handler;
    // SAFETY: `handler` implements `willFinishLaunching:` taking one NSNotification; the
    // notification name is the value of AppKit's NSApplicationWillFinishLaunchingNotification.
    unsafe {
      NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
        observer,
        sel!(willFinishLaunching:),
        Some(ns_string!("NSApplicationWillFinishLaunchingNotification")),
        None,
      );
    }
    // Neither the notification centre nor the event manager keeps it alive: it lives as long
    // as the process.
    std::mem::forget(handler);
  }
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
