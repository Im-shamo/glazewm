use std::{
  sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, OnceLock,
  },
  thread::{self, JoinHandle},
  time::SystemTime,
};

use tokio::sync::mpsc;
use tracing::{error, info, warn};
use windows::Win32::{
  Devices::HumanInterfaceDevice::{
    HID_USAGE_GENERIC_MOUSE, HID_USAGE_PAGE_GENERIC, MOUSE_MOVE_ABSOLUTE,
    MOUSE_MOVE_RELATIVE,
  },
  Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
  UI::{
    Input::{
      GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT,
      RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RID_INPUT,
      RIM_TYPEMOUSE,
    },
    WindowsAndMessaging::{
      DefWindowProcW, DestroyWindow, GetCursorPos, DBT_DEVNODES_CHANGED,
      RI_MOUSE_LEFT_BUTTON_DOWN, RI_MOUSE_LEFT_BUTTON_UP,
      RI_MOUSE_RIGHT_BUTTON_DOWN, RI_MOUSE_RIGHT_BUTTON_UP,
      SPI_ICONVERTICALSPACING, SPI_SETWORKAREA, WM_DEVICECHANGE,
      WM_DISPLAYCHANGE, WM_INPUT, WM_SETTINGCHANGE,
    },
  },
};

use crate::{common::Point, user_config::KeybindingConfig};

use super::{
  KeyboardHook, MouseMoveEvent, Platform, PlatformEvent, WinEventHook,
};

/// Global instance of sender for platform events.
///
/// For use with window procedure.
static PLATFORM_EVENT_TX: OnceLock<mpsc::UnboundedSender<PlatformEvent>> =
  OnceLock::new();

/// Whether mouse hook is currently enabled.
///
/// For use with window procedure.
static ENABLE_MOUSE_EVENTS: AtomicBool = AtomicBool::new(false);

/// Whether left-click is currently pressed.
///
/// For use with window procedure.
static IS_L_MOUSE_DOWN: AtomicBool = AtomicBool::new(false);

/// Whether right-click is currently pressed.
///
/// For use with window procedure.
static IS_R_MOUSE_DOWN: AtomicBool = AtomicBool::new(false);

/// Timestamp of the last mouse event emission.
///
/// For use with window procedure.
static LAST_MOUSE_EVENT_TIME: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct EventWindow {
  keyboard_hook: Arc<KeyboardHook>,
  window_thread: Option<JoinHandle<anyhow::Result<()>>>,
}

impl EventWindow {
  /// Creates an instance of `EventWindow`. Spawns a hidden window and
  /// emits platform events.
  ///
  /// Uses global state (e.g. `PLATFORM_EVENT_TX`) and should thus only
  /// ever be instantiated once in the application's lifetime.
  pub fn new(
    event_tx: mpsc::UnboundedSender<PlatformEvent>,
    keybindings: Vec<KeybindingConfig>,
    enable_mouse_events: bool,
  ) -> anyhow::Result<Self> {
    let keyboard_hook = KeyboardHook::new(keybindings, event_tx.clone())?;
    let win_event_hook = WinEventHook::new(event_tx.clone())?;
    let keyboard_hook_clone = keyboard_hook.clone();

    // Add the sender for platform events to global state.
    PLATFORM_EVENT_TX.set(event_tx.clone()).map_err(|_| {
      anyhow::anyhow!("Platform event sender already set.")
    })?;

    ENABLE_MOUSE_EVENTS.store(enable_mouse_events, Ordering::Relaxed);

    let window_thread = thread::spawn(move || {
      // Start hooks for listening to platform events.
      keyboard_hook_clone.start()?;
      win_event_hook.start()?;

      // Create a hidden window with a message loop on the current thread.
      let handle =
        Platform::create_message_window(Some(event_window_proc))?;

      let rid = RAWINPUTDEVICE {
        usUsagePage: HID_USAGE_PAGE_GENERIC,
        usUsage: HID_USAGE_GENERIC_MOUSE,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: HWND(handle),
      };

      // Register our window to receive mouse events.
      unsafe {
        RegisterRawInputDevices(
          &[rid],
          std::mem::size_of::<RAWINPUTDEVICE>() as u32,
        )
      }?;

      Platform::run_message_loop();

      // Clean-up on message loop exit.
      unsafe { DestroyWindow(HWND(handle)) }?;
      keyboard_hook_clone.stop()?;
      win_event_hook.stop()?;

      Ok(())
    });

    Ok(Self {
      keyboard_hook,
      window_thread: Some(window_thread),
    })
  }

  pub fn update(
    &mut self,
    keybindings: Vec<KeybindingConfig>,
    enable_mouse_events: bool,
  ) {
    self.keyboard_hook.update(keybindings);
    ENABLE_MOUSE_EVENTS.store(enable_mouse_events, Ordering::Relaxed);
  }

  /// Destroys the event window and stops the message loop.
  pub fn destroy(&mut self) -> anyhow::Result<()> {
    info!("Shutting down event window.");

    // Wait for the spawned thread to finish.
    if let Some(window_thread) = self.window_thread.take() {
      Platform::kill_message_loop(&window_thread)?;

      window_thread
        .join()
        .map_err(|_| anyhow::anyhow!("Thread join failed."))??;
    }

    Ok(())
  }
}

impl Drop for EventWindow {
  fn drop(&mut self) {
    if let Err(err) = self.destroy() {
      error!("Failed to gracefully shut down event window: {}", err);
    }
  }
}

/// Window procedure for the event window.
///
/// Handles messages for the event window, and forwards display change
/// events through an MPSC channel for the WM to process.
pub extern "system" fn event_window_proc(
  handle: HWND,
  message: u32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  if let Some(event_tx) = PLATFORM_EVENT_TX.get() {
    return match message {
      WM_DISPLAYCHANGE | WM_SETTINGCHANGE | WM_DEVICECHANGE => {
        if let Err(err) =
          handle_display_change_msg(message, wparam, event_tx)
        {
          warn!("Failed to handle display change message: {}", err);
        }

        LRESULT(0)
      }
      WM_INPUT if ENABLE_MOUSE_EVENTS.load(Ordering::Relaxed) => {
        if let Err(err) = handle_input_msg(wparam, lparam, event_tx) {
          warn!("Failed to handle input message: {}", err);
        }

        LRESULT(0)
      }
      _ => unsafe { DefWindowProcW(handle, message, wparam, lparam) },
    };
  }

  LRESULT(0)
}

/// Handles display change messages and emits the corresponding platform
/// event through an MPSC channel.
fn handle_display_change_msg(
  message: u32,
  wparam: WPARAM,
  event_tx: &mpsc::UnboundedSender<PlatformEvent>,
) -> anyhow::Result<()> {
  let should_emit_event = match message {
    WM_SETTINGCHANGE => {
      wparam.0 as u32 == SPI_SETWORKAREA.0
        || wparam.0 as u32 == SPI_ICONVERTICALSPACING.0
    }
    WM_DEVICECHANGE => wparam.0 as u32 == DBT_DEVNODES_CHANGED,
    _ => true,
  };

  if should_emit_event {
    event_tx.send(PlatformEvent::DisplaySettingsChanged)?;
  }

  Ok(())
}

/// Handles raw input messages for mouse events and emits the corresponding
/// platform event through an MPSC channel.
fn handle_input_msg(
  _wparam: WPARAM,
  lparam: LPARAM,
  event_tx: &mpsc::UnboundedSender<PlatformEvent>,
) -> anyhow::Result<()> {
  let event_time = SystemTime::now()
    .duration_since(SystemTime::UNIX_EPOCH)
    .map(|dur| dur.as_millis() as u64)?;

  let last_event_time = LAST_MOUSE_EVENT_TIME.load(Ordering::Relaxed);

  // Throttle events so that there's a minimum of 50ms between each
  // emission.
  if event_time - last_event_time < 50 {
    return Ok(());
  }

  let mut raw_input: RAWINPUT = unsafe { std::mem::zeroed() };
  let mut raw_input_size = std::mem::size_of::<RAWINPUT>() as u32;

  let res_size = unsafe {
    GetRawInputData(
      HRAWINPUT(lparam.0),
      RID_INPUT,
      Some(&mut raw_input as *mut _ as _),
      &mut raw_input_size,
      std::mem::size_of::<RAWINPUTHEADER>() as u32,
    )
  };

  // Ignore if data is invalid or not a mouse event.
  if res_size == 0
    || raw_input_size == u32::MAX
    || raw_input.header.dwType != RIM_TYPEMOUSE.0
  {
    return Ok(());
  }

  let mouse_input = unsafe { raw_input.data.mouse };
  let state_flags = mouse_input.usFlags;
  let button_flags =
    unsafe { mouse_input.Anonymous.Anonymous.usButtonFlags };

  if has_flags(button_flags, RI_MOUSE_LEFT_BUTTON_DOWN) {
    IS_L_MOUSE_DOWN.store(true, Ordering::Relaxed);
  }

  if has_flags(button_flags, RI_MOUSE_LEFT_BUTTON_UP) {
    IS_L_MOUSE_DOWN.store(false, Ordering::Relaxed);
  }

  if has_flags(button_flags, RI_MOUSE_RIGHT_BUTTON_DOWN) {
    IS_R_MOUSE_DOWN.store(true, Ordering::Relaxed);
  }

  if has_flags(button_flags, RI_MOUSE_RIGHT_BUTTON_UP) {
    IS_R_MOUSE_DOWN.store(false, Ordering::Relaxed);
  }

  // Emit even if a mouse move has occured.
  if has_flags(state_flags, MOUSE_MOVE_RELATIVE)
    || has_flags(state_flags, MOUSE_MOVE_ABSOLUTE)
  {
    let is_mouse_down = IS_L_MOUSE_DOWN.load(Ordering::Relaxed)
      || IS_R_MOUSE_DOWN.load(Ordering::Relaxed);

    let mut point = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut point) }?;

    let event = MouseMoveEvent {
      point: Point {
        x: point.x,
        y: point.y,
      },
      is_mouse_down,
    };

    event_tx.send(PlatformEvent::MouseMove(event))?;

    LAST_MOUSE_EVENT_TIME.store(event_time, Ordering::Relaxed);
  }

  Ok(())
}

/// Checks whether `short` contains all the bits of `mask`.
#[inline]
fn has_flags(short: u16, mask: u32) -> bool {
  short & mask as u16 == mask as u16
}
