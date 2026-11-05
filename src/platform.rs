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
