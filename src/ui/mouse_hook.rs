//! A mouse event for this app's own window, from a `--view` hook
//! ("mouse:18000:down:left:700:480"): for checking what the desktop view
//! sends a machine without anyone's hand on the mouse
//! (scripts/check-desktop-buttons.sh).
//!
//! The event is put in the app's own queue. From there it goes the way a
//! real one does: AppKit, GPUI's hit test, the view's listeners. The
//! system's pointer doesn't move, and no other app hears of it.

use std::time::Duration;

/// What the mouse does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum What {
    Down,
    Up,
    Move,
    /// A move with the button held.
    Drag,
}

/// One step of a check: after `after`, `what` at (`x`, `y`) in the window,
/// counted from its top left. A place outside the window is fine (below 0,
/// or past its size): that's the mouse off the window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Step {
    pub after: Duration,
    pub what: What,
    /// With Control held: a Mac's own right click.
    pub control: bool,
    pub x: f64,
    pub y: f64,
}

impl Step {
    /// `<ms>:<down|up|move|drag>:<left|ctrl>:<x>:<y>`. The button is the
    /// left one, or "ctrl" for the left one with Control held. That's the
    /// only right click there is to make here: GPUI takes a button from the
    /// event's own number, and an event made this way has the left one's
    /// whatever its type.
    pub fn parse(arg: &str) -> Option<Step> {
        let mut parts = arg.split(':');
        let after = Duration::from_millis(parts.next()?.parse().ok()?);
        let what = match parts.next()? {
            "down" => What::Down,
            "up" => What::Up,
            "move" => What::Move,
            "drag" => What::Drag,
            _ => return None,
        };
        let control = match parts.next()? {
            "left" => false,
            "ctrl" => true,
            _ => return None,
        };
        let (x, y) = (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
        parts.next().is_none().then_some(Step { after, what, control, x, y })
    }

    /// The event's NSEventType: the left button's down, up and dragged, or
    /// the mouse moved.
    fn kind(&self) -> u64 {
        match self.what {
            What::Down => 1,
            What::Up => 2,
            What::Move => 5,
            What::Drag => 6,
        }
    }

    /// Its modifier flags: NSEventModifierFlagControl, or none.
    fn flags(&self) -> u64 {
        if self.control { 1 << 18 } else { 0 }
    }

    /// Puts the event in this app's queue, for its biggest window (the one
    /// with the pages). Call on the main thread.
    #[allow(unexpected_cfgs)] // objc's msg_send! checks a cfg this crate doesn't declare
    pub fn post(&self) {
        use objc::runtime::Object;
        use objc::{class, msg_send, sel, sel_impl};
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct Point {
            x: f64,
            y: f64,
        }
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct Rect {
            origin: Point,
            size: Point,
        }
        unsafe {
            let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
            let windows: *mut Object = msg_send![app, windows];
            let many: u64 = msg_send![windows, count];
            let mut main: *mut Object = std::ptr::null_mut();
            let mut biggest = 0.;
            for i in 0..many {
                let window: *mut Object = msg_send![windows, objectAtIndex: i];
                let frame: Rect = msg_send![window, frame];
                if frame.size.x * frame.size.y > biggest {
                    biggest = frame.size.x * frame.size.y;
                    main = window;
                }
            }
            if main.is_null() {
                return;
            }
            let content: *mut Object = msg_send![main, contentView];
            let frame: Rect = msg_send![content, frame];
            // A window's own coordinates start at its bottom left.
            let location = Point { x: self.x, y: frame.size.y - self.y };
            let number: isize = msg_send![main, windowNumber];
            let info: *mut Object = msg_send![class!(NSProcessInfo), processInfo];
            let now: f64 = msg_send![info, systemUptime];
            let none: *mut Object = std::ptr::null_mut();
            let event: *mut Object = msg_send![class!(NSEvent), mouseEventWithType: self.kind() location: location modifierFlags: self.flags() timestamp: now windowNumber: number context: none eventNumber: 0isize clickCount: 1isize pressure: 1.0f32];
            let _: () = msg_send![app, postEvent: event atStart: false];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_is_read_from_the_hooks_words() {
        let step = Step::parse("18400:down:ctrl:723:480").unwrap();
        assert_eq!(step, Step { after: Duration::from_millis(18_400), what: What::Down, control: true, x: 723., y: 480. });
        // A Control-click is the left button's event with Control on it.
        assert_eq!((step.kind(), step.flags()), (1, 1 << 18));
        // Off the window is a place too.
        let off = Step::parse("0:up:left:-100:480.5").unwrap();
        assert_eq!((off.what, off.control, off.x, off.y, off.kind(), off.flags()), (What::Up, false, -100., 480.5, 2, 0));
        assert_eq!(Step::parse("5:move:left:1:2").map(|s| s.kind()), Some(5));
        assert_eq!(Step::parse("5:drag:left:1:2").map(|s| s.kind()), Some(6));
    }

    #[test]
    fn what_isnt_a_step_is_none() {
        for not in ["", "down:left:1:2", "soon:down:left:1:2", "5:press:left:1:2", "5:down:right:1:2", "5:down:left:1", "5:down:left:x:2", "5:down:left:1:2:3"] {
            assert_eq!(Step::parse(not), None, "{not}");
        }
    }
}
