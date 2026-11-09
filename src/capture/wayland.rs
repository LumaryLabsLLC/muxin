//! Linux on Wayland: the keyboard-shortcuts-inhibit protocol. The compositor
//! is asked not to act on its shortcuts while Muxin's window has the
//! keyboard; the keys then come to the window like any other. Compositors
//! that have it (GNOME, KDE, Sway, Hyprland) may ask the user first, and keep
//! a way out of their own.
//!
//! GPUI doesn't speak this protocol, but gives its `wl_display` and the
//! window's `wl_surface`: the request is made from an event queue of our own
//! on that same connection.

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry::WlRegistry, wl_seat::WlSeat, wl_surface::WlSurface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1 as Manager;
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1 as Inhibitor;

/// Nothing to keep: what the compositor says back isn't needed.
struct Quiet;

impl Dispatch<WlRegistry, GlobalListContents> for Quiet {
    fn event(_: &mut Self, _: &WlRegistry, _: <WlRegistry as Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

delegate_noop!(Quiet: ignore WlSeat);
delegate_noop!(Quiet: Manager);
delegate_noop!(Quiet: ignore Inhibitor);

/// The compositor's shortcuts are held back while this lives.
pub struct Inhibit {
    conn: Connection,
    inhibitor: Inhibitor,
    _queue: EventQueue<Quiet>,
}

impl Inhibit {
    /// None when the compositor doesn't have the protocol.
    pub fn start(window: &gpui::Window) -> Option<Inhibit> {
        let RawDisplayHandle::Wayland(display) = HasDisplayHandle::display_handle(window).ok()?.as_raw() else { return None };
        let RawWindowHandle::Wayland(surface) = HasWindowHandle::window_handle(window).ok()?.as_raw() else { return None };
        // GPUI's connection: borrowed, never closed from here.
        let conn = Connection::from_backend(unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) });
        let (globals, queue) = registry_queue_init::<Quiet>(&conn).ok()?;
        let handle = queue.handle();
        let manager: Manager = globals.bind(&handle, 1..=1, ()).ok()?;
        let seat: WlSeat = globals.bind(&handle, 1..=1, ()).ok()?;
        let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.surface.as_ptr().cast()) }.ok()?;
        let surface = WlSurface::from_id(&conn, id).ok()?;
        let inhibitor = manager.inhibit_shortcuts(&surface, &seat, &handle, ());
        conn.flush().ok()?;
        Some(Inhibit { conn, inhibitor, _queue: queue })
    }
}

impl Drop for Inhibit {
    fn drop(&mut self) {
        self.inhibitor.destroy();
        let _ = self.conn.flush();
    }
}
