//! System notifications: one when a session starts needing you, ends with an
//! error, or finishes its turn. Clicking it opens that session. It goes from
//! the notification center once the session is opened (or moves on), so what's
//! there is what still waits. The session in front of you doesn't notify, and
//! Settings turns them off (`[ui] notifications`).
//!
//! It follows the engine on its runtime, so it lasts with no window open (on
//! Linux the app's windows come and go; see `ui::run`). Each system has its own
//! way to show them: `imp`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::fleet::Engine;
use crate::fleet::model::Snapshot;
use crate::fleet::sessions::Mark;

static STARTED: AtomicBool = AtomicBool::new(false);
static ENABLED: AtomicBool = AtomicBool::new(true);
/// The session on screen in the window in front, if any.
static IN_FRONT: Mutex<Option<String>> = Mutex::new(None);

/// Turns them on or off (Settings): off takes away the ones showing.
pub(super) fn set_enabled(on: bool) {
    if !ENABLED.swap(on, Ordering::SeqCst) || on {
        return;
    }
    imp::withdraw_all();
}

/// The session you're looking at (the window is in front and shows it).
pub(super) fn set_in_front(session: Option<String>) {
    let mut front = IN_FRONT.lock().unwrap();
    if *front != session {
        *front = session;
    }
}

/// Once, whatever becomes of the windows.
pub(super) fn start(engine: Arc<Engine>, enabled: bool) {
    ENABLED.store(enabled, Ordering::SeqCst);
    // For trying them out: the demo notifies too, and what waits at launch is news.
    let testing = std::env::var_os("MUXIN_TEST_NOTIFICATIONS").is_some();
    if STARTED.swap(true, Ordering::SeqCst) || (engine.is_demo() && !testing) {
        return;
    }
    if !imp::prepare(engine.runtime()) {
        STARTED.store(false, Ordering::SeqCst);
        return;
    }
    let runtime = engine.runtime().clone();
    runtime.spawn(async move {
        let mut snapshots = engine.subscribe();
        // What's waiting already when the app starts was there before: no news.
        let mut seen = if testing { HashMap::new() } else { marks(&snapshots.borrow_and_update()) };
        while snapshots.changed().await.is_ok() {
            let snapshot = snapshots.borrow_and_update().clone();
            let (news, gone) = changes(&mut seen, &snapshot);
            for id in gone {
                imp::withdraw(&id);
            }
            if !ENABLED.load(Ordering::SeqCst) {
                continue;
            }
            let front = IN_FRONT.lock().unwrap().clone();
            for notice in news.into_iter().filter(|n| front.as_deref() != Some(n.session_id.as_str())) {
                imp::post(&notice);
            }
        }
    });
}

/// What a notification says, and which session it opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Notice {
    pub session_id: String,
    pub title: String,
    pub body: String,
}

fn waits(mark: Mark) -> bool {
    matches!(mark, Mark::NeedsYou | Mark::Failed | Mark::Done)
}

fn marks(snapshot: &Snapshot) -> HashMap<String, Mark> {
    snapshot.sessions.iter().map(|s| (s.id.clone(), s.mark())).collect()
}

/// What's new since `seen` (which it brings up to date): sessions that start
/// to wait for you, or wait for another reason, and those that stopped
/// waiting (opened, working again, or gone), whose notification goes.
fn changes(seen: &mut HashMap<String, Mark>, snapshot: &Snapshot) -> (Vec<Notice>, Vec<String>) {
    let now = marks(snapshot);
    let mut news = Vec::new();
    let mut gone = Vec::new();
    for s in &snapshot.sessions {
        let mark = s.mark();
        let before = seen.get(&s.id).copied();
        if waits(mark) && before != Some(mark) {
            let machine = snapshot.machines.iter().find(|m| m.id == s.machine_id).map_or(s.machine_id.as_str(), |m| m.name.as_str());
            news.push(notice(&s.id, &s.title, &s.agent_name, machine, mark));
        } else if !waits(mark) && before.is_some_and(waits) {
            gone.push(s.id.clone());
        }
    }
    gone.extend(seen.iter().filter(|(id, mark)| waits(**mark) && !now.contains_key(*id)).map(|(id, _)| id.clone()));
    *seen = now;
    (news, gone)
}

fn notice(id: &str, title: &str, agent: &str, machine: &str, mark: Mark) -> Notice {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let what = match mark {
        Mark::NeedsYou => "needs you",
        Mark::Failed => "stopped with an error",
        _ => "is done",
    };
    let name = if agent.is_empty() { "The session".to_string() } else { agent.to_string() };
    Notice {
        session_id: id.to_string(),
        title: if title.is_empty() { format!("{name} {what}") } else { title },
        body: format!("{name} on {machine} {what}."),
    }
}

/// Opens the session a notification was for.
#[cfg_attr(not(any(target_os = "macos", windows, target_os = "linux")), allow(dead_code))]
fn clicked(session_id: &str) {
    super::command(super::AppCommand::OpenSession(session_id.to_string()));
}

#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs)] // objc's msg_send! checks a cfg this crate doesn't declare
mod imp {
    //! The notification center (UserNotifications). It only works for an app
    //! in a bundle, so a bare build (`cargo run`) shows none.
    use std::sync::OnceLock;

    use block::{Block, ConcreteBlock};
    use cocoa::base::{BOOL, NO, YES, id, nil};
    use cocoa::foundation::{NSAutoreleasePool, NSString};
    use objc::declare::ClassDecl;
    use objc::runtime::{Class, Object, Sel};
    use objc::{class, msg_send, sel, sel_impl};

    use super::Notice;

    #[link(name = "UserNotifications", kind = "framework")]
    unsafe extern "C" {}

    /// UNAuthorizationOptionBadge | Sound | Alert.
    const ASK_FOR: u64 = 1 | 2 | 4;
    /// UNNotificationPresentationOptionSound | List | Banner: shown even while
    /// Muxin is in front (the session on screen never gets one).
    const PRESENT: u64 = 2 | 8 | 16;

    /// The center, once it's set up (only in a bundle).
    fn center() -> Option<id> {
        static CENTER: OnceLock<Option<usize>> = OnceLock::new();
        CENTER
            .get_or_init(|| unsafe {
                let bundle: id = msg_send![class!(NSBundle), mainBundle];
                let bundle_id: id = msg_send![bundle, bundleIdentifier];
                if bundle_id == nil {
                    tracing::info!("no notifications: not running from an app bundle");
                    return None;
                }
                let center: id = msg_send![class!(UNUserNotificationCenter), currentNotificationCenter];
                if center == nil {
                    return None;
                }
                let delegate: id = msg_send![delegate_class(), new];
                let _: () = msg_send![center, setDelegate: delegate];
                let asked = ConcreteBlock::new(|granted: BOOL, _error: id| {
                    if granted == NO {
                        tracing::info!("notifications are turned off for Muxin in System Settings");
                    }
                })
                .copy();
                let _: () = msg_send![center, requestAuthorizationWithOptions: ASK_FOR completionHandler: &*asked];
                Some(center as usize)
            })
            .map(|c| c as id)
    }

    /// Answers the center: a click opens the session, and one comes in while
    /// Muxin is in front shows anyway.
    fn delegate_class() -> &'static Class {
        static CLASS: OnceLock<usize> = OnceLock::new();
        let class = *CLASS.get_or_init(|| {
            let mut decl = ClassDecl::new("MuxinNotificationDelegate", class!(NSObject)).expect("one delegate class");
            extern "C" fn clicked(_: &Object, _: Sel, _center: id, response: id, done: id) {
                unsafe {
                    let notification: id = msg_send![response, notification];
                    let request: id = msg_send![notification, request];
                    let identifier: id = msg_send![request, identifier];
                    let action: id = msg_send![response, actionIdentifier];
                    // Opened (not swiped away).
                    let opened: BOOL = msg_send![action, isEqualToString: NSString::alloc(nil).init_str("com.apple.UNNotificationDefaultActionIdentifier").autorelease()];
                    if opened == YES
                        && let Some(session) = rust_string(identifier)
                    {
                        super::clicked(&session);
                    }
                    let done = &*(done as *const Block<(), ()>);
                    done.call(());
                }
            }
            extern "C" fn present(_: &Object, _: Sel, _center: id, _notification: id, done: id) {
                unsafe {
                    let done = &*(done as *const Block<(u64,), ()>);
                    done.call((PRESENT,));
                }
            }
            unsafe {
                decl.add_method(sel!(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:), clicked as extern "C" fn(&Object, Sel, id, id, id));
                decl.add_method(sel!(userNotificationCenter:willPresentNotification:withCompletionHandler:), present as extern "C" fn(&Object, Sel, id, id, id));
            }
            decl.register() as *const Class as usize
        });
        unsafe { &*(class as *const Class) }
    }

    unsafe fn rust_string(s: id) -> Option<String> {
        if s == nil {
            return None;
        }
        let utf8: *const std::os::raw::c_char = unsafe { msg_send![s, UTF8String] };
        (!utf8.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(utf8) }.to_string_lossy().into_owned())
    }

    fn ns(s: &str) -> id {
        unsafe { NSString::alloc(nil).init_str(s).autorelease() }
    }

    pub fn prepare(_: &tokio::runtime::Handle) -> bool {
        center().is_some()
    }

    pub fn post(n: &Notice) {
        let Some(center) = center() else { return };
        unsafe {
            let pool = NSAutoreleasePool::new(nil);
            let content: id = msg_send![class!(UNMutableNotificationContent), new];
            let _: () = msg_send![content, setTitle: ns(&n.title)];
            let _: () = msg_send![content, setBody: ns(&n.body)];
            let sound: id = msg_send![class!(UNNotificationSound), defaultSound];
            let _: () = msg_send![content, setSound: sound];
            // The session's id: a newer one for it replaces the older.
            let request: id = msg_send![class!(UNNotificationRequest), requestWithIdentifier: ns(&n.session_id) content: content trigger: nil];
            let _: () = msg_send![center, addNotificationRequest: request withCompletionHandler: nil];
            let _: () = msg_send![content, release];
            pool.drain();
        }
    }

    pub fn withdraw(session_id: &str) {
        let Some(center) = center() else { return };
        unsafe {
            let pool = NSAutoreleasePool::new(nil);
            let ids: id = msg_send![class!(NSArray), arrayWithObject: ns(session_id)];
            let _: () = msg_send![center, removeDeliveredNotificationsWithIdentifiers: ids];
            pool.drain();
        }
    }

    pub fn withdraw_all() {
        let Some(center) = center() else { return };
        unsafe {
            let _: () = msg_send![center, removeAllDeliveredNotifications];
        }
    }
}

#[cfg(windows)]
mod imp {
    //! Toasts, under an app id Muxin registers for the user (no administrator
    //! needed): Windows shows only notifications of apps it knows. They're
    //! made on a thread of their own, set up for WinRT.
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock, mpsc};

    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
    use windows::core::{HSTRING, IInspectable};

    use super::Notice;

    const APP_ID: &str = "LumaryLabs.Muxin";
    const GROUP: &str = "sessions";

    enum Msg {
        Post(Notice),
        Withdraw(String),
        WithdrawAll,
    }

    static TX: OnceLock<Mutex<mpsc::Sender<Msg>>> = OnceLock::new();

    pub fn prepare(_: &tokio::runtime::Handle) -> bool {
        if TX.get().is_some() {
            return true;
        }
        if let Err(e) = register() {
            tracing::info!("no notifications: {e}");
            return false;
        }
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new().name("notifications".into()).spawn(move || run(rx)).is_ok() && TX.set(Mutex::new(tx)).is_ok()
    }

    fn send(msg: Msg) {
        if let Some(tx) = TX.get() {
            let _ = tx.lock().unwrap().send(msg);
        }
    }

    pub fn post(n: &Notice) {
        send(Msg::Post(n.clone()));
    }

    pub fn withdraw(session_id: &str) {
        send(Msg::Withdraw(session_id.to_string()));
    }

    pub fn withdraw_all() {
        send(Msg::WithdrawAll);
    }

    /// The name Windows keeps a toast by (64 characters at most): the
    /// session's id, hashed.
    fn tag(session_id: &str) -> HSTRING {
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        session_id.hash(&mut hash);
        HSTRING::from(format!("{:016x}", hash.finish()))
    }

    fn run(rx: mpsc::Receiver<Msg>) {
        unsafe {
            let _ = windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED);
        }
        let notifier = match ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID)) {
            Ok(n) => n,
            Err(e) => {
                tracing::info!("no notifications: {e}");
                return;
            }
        };
        // Kept while they show: each has the handler that opens its session.
        let mut shown: HashMap<String, ToastNotification> = HashMap::new();
        for msg in rx {
            match msg {
                Msg::Post(n) => match toast(&n) {
                    Ok(t) => {
                        if let Some(old) = shown.remove(&n.session_id) {
                            let _ = notifier.Hide(&old);
                        }
                        if notifier.Show(&t).is_ok() {
                            shown.insert(n.session_id.clone(), t);
                        }
                    }
                    Err(e) => tracing::debug!("couldn't make a notification: {e}"),
                },
                Msg::Withdraw(id) => {
                    shown.remove(&id);
                    if let Ok(history) = ToastNotificationManager::History() {
                        let _ = history.RemoveGroupedTagWithId(&tag(&id), &HSTRING::from(GROUP), &HSTRING::from(APP_ID));
                    }
                }
                Msg::WithdrawAll => {
                    shown.clear();
                    if let Ok(history) = ToastNotificationManager::History() {
                        let _ = history.ClearWithId(&HSTRING::from(APP_ID));
                    }
                }
            }
        }
    }

    fn toast(n: &Notice) -> windows::core::Result<ToastNotification> {
        let xml = format!(r#"<toast><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual></toast>"#, escape(&n.title), escape(&n.body));
        let doc = XmlDocument::new()?;
        doc.LoadXml(&HSTRING::from(xml))?;
        let toast = ToastNotification::CreateToastNotification(&doc)?;
        toast.SetTag(&tag(&n.session_id))?;
        toast.SetGroup(&HSTRING::from(GROUP))?;
        let session = n.session_id.clone();
        toast.Activated(&TypedEventHandler::<ToastNotification, IInspectable>::new(move |_, _| {
            super::clicked(&session);
            Ok(())
        }))?;
        Ok(toast)
    }

    fn escape(text: &str) -> String {
        text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
    }

    /// Muxin's name and icon under its app id, in the user's part of the registry.
    fn register() -> Result<(), String> {
        use windows_sys::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegSetValueExW};
        let dir = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from).ok_or("LOCALAPPDATA isn't set")?.join("Muxin");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let icon = dir.join("notification-icon.png");
        if !icon.exists() {
            std::fs::write(&icon, include_bytes!("../../assets/brand/icon-128.png")).map_err(|e| e.to_string())?;
        }
        let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
        let path = wide(&format!(r"Software\Classes\AppUserModelId\{APP_ID}"));
        let mut key: HKEY = std::ptr::null_mut();
        let made = unsafe { RegCreateKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, std::ptr::null(), REG_OPTION_NON_VOLATILE, KEY_WRITE, std::ptr::null(), &mut key, std::ptr::null_mut()) };
        if made != 0 {
            return Err(format!("couldn't register the app for notifications (error {made})"));
        }
        for (name, value) in [("DisplayName", "Muxin".to_string()), ("IconUri", icon.to_string_lossy().into_owned())] {
            let value = wide(&value);
            unsafe {
                RegSetValueExW(key, wide(name).as_ptr(), 0, REG_SZ, value.as_ptr().cast(), (value.len() * 2) as u32);
            }
        }
        unsafe { RegCloseKey(key) };
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod imp {
    //! The desktop's notification server, over D-Bus: a click is its
    //! "default" action, which opens the session.
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    use futures::StreamExt;
    use tokio::sync::mpsc;

    use super::Notice;

    enum Msg {
        Post(Notice),
        Withdraw(String),
        WithdrawAll,
    }

    static TX: OnceLock<mpsc::UnboundedSender<Msg>> = OnceLock::new();

    pub fn prepare(runtime: &tokio::runtime::Handle) -> bool {
        if TX.get().is_some() {
            return true;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        runtime.spawn(run(rx));
        TX.set(tx).is_ok()
    }

    fn send(msg: Msg) {
        if let Some(tx) = TX.get() {
            let _ = tx.send(msg);
        }
    }

    pub fn post(n: &Notice) {
        send(Msg::Post(n.clone()));
    }

    pub fn withdraw(session_id: &str) {
        send(Msg::Withdraw(session_id.to_string()));
    }

    pub fn withdraw_all() {
        send(Msg::WithdrawAll);
    }

    async fn run(mut rx: mpsc::UnboundedReceiver<Msg>) {
        let proxy = match connect().await {
            Ok(p) => p,
            Err(e) => {
                tracing::info!("no notifications: {e}");
                return;
            }
        };
        // Which session each one shown is for, both ways.
        let sessions: Arc<Mutex<HashMap<u32, String>>> = Default::default();
        let mut ids: HashMap<String, u32> = HashMap::new();
        if let Ok(mut clicks) = proxy.receive_signal("ActionInvoked").await {
            let sessions = sessions.clone();
            tokio::spawn(async move {
                while let Some(msg) = clicks.next().await {
                    let Ok((id, action)) = msg.body().deserialize::<(u32, String)>() else { continue };
                    let session = sessions.lock().unwrap().get(&id).cloned();
                    if action == "default"
                        && let Some(session) = session
                    {
                        super::clicked(&session);
                    }
                }
            });
        }
        while let Some(msg) = rx.recv().await {
            match msg {
                Msg::Post(n) => {
                    // A newer one for a session replaces the older.
                    let replaces = ids.get(&n.session_id).copied().unwrap_or(0);
                    let hints: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::from([("desktop-entry", "muxin".into())]);
                    let shown: zbus::Result<u32> = proxy.call("Notify", &("Muxin", replaces, "muxin", n.title.as_str(), n.body.as_str(), vec!["default", "Open"], hints, -1i32)).await;
                    match shown {
                        Ok(id) => {
                            sessions.lock().unwrap().insert(id, n.session_id.clone());
                            ids.insert(n.session_id, id);
                        }
                        Err(e) => tracing::debug!("couldn't show a notification: {e}"),
                    }
                }
                Msg::Withdraw(session) => {
                    if let Some(id) = ids.remove(&session) {
                        sessions.lock().unwrap().remove(&id);
                        let _: zbus::Result<()> = proxy.call("CloseNotification", &(id,)).await;
                    }
                }
                Msg::WithdrawAll => {
                    for (_, id) in ids.drain() {
                        let _: zbus::Result<()> = proxy.call("CloseNotification", &(id,)).await;
                    }
                    sessions.lock().unwrap().clear();
                }
            }
        }
    }

    async fn connect() -> zbus::Result<zbus::Proxy<'static>> {
        let connection = zbus::Connection::session().await?;
        zbus::Proxy::new(&connection, "org.freedesktop.Notifications", "/org/freedesktop/Notifications", "org.freedesktop.Notifications").await
    }
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
mod imp {
    use super::Notice;

    pub fn prepare(_: &tokio::runtime::Handle) -> bool {
        false
    }
    pub fn post(_: &Notice) {}
    pub fn withdraw(_: &str) {}
    pub fn withdraw_all() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::sessions::SessionInfo;

    fn session(id: &str, state: &str, attention: Option<&str>) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            machine_id: "forge".into(),
            agent_name: "Claude Code".into(),
            title: format!("Task {id}"),
            status: "running".into(),
            state: state.into(),
            attention: attention.map(str::to_string),
            ..Default::default()
        }
    }

    fn snapshot(sessions: Vec<SessionInfo>) -> Snapshot {
        Snapshot { sessions, ..Default::default() }
    }

    #[test]
    fn only_a_change_to_waiting_is_news() {
        let mut seen = marks(&snapshot(vec![session("a", "working", None), session("b", "done", Some("done"))]));
        // b was done before the app looked: no news. a finishes: news.
        let (news, gone) = changes(&mut seen, &snapshot(vec![session("a", "done", Some("done")), session("b", "done", Some("done"))]));
        assert_eq!(news.len(), 1);
        assert_eq!(news[0].session_id, "a");
        assert_eq!(news[0].title, "Task a");
        assert_eq!(news[0].body, "Claude Code on forge is done.");
        assert!(gone.is_empty());
        // Nothing changed: nothing new.
        assert_eq!(changes(&mut seen, &snapshot(vec![session("a", "done", Some("done")), session("b", "done", Some("done"))])), (Vec::new(), Vec::new()));
    }

    #[test]
    fn needing_you_after_done_is_news_again() {
        let mut seen = marks(&snapshot(vec![session("a", "done", Some("done"))]));
        let (news, _) = changes(&mut seen, &snapshot(vec![session("a", "needs_you", Some("waiting"))]));
        assert_eq!(news[0].body, "Claude Code on forge needs you.");
    }

    #[test]
    fn opened_or_gone_takes_it_away() {
        let mut seen = marks(&snapshot(vec![session("a", "done", Some("done")), session("b", "needs_you", None)]));
        // a was opened (its attention cleared), b ended.
        let (news, mut gone) = changes(&mut seen, &snapshot(vec![session("a", "done", None)]));
        gone.sort();
        assert!(news.is_empty());
        assert_eq!(gone, ["a", "b"]);
    }
}
