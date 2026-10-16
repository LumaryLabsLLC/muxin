//! The app's side of crate::update: a look at GitHub soon after launch and
//! every twelve hours (while Settings › General › Updates allows it), kept for
//! the sidebar's footer and the About page.

use std::sync::Arc;
use std::time::Duration;

use gpui::{App, AsyncApp, Global};

use crate::fleet::Engine;
use crate::update::Update;

const FIRST: Duration = Duration::from_secs(8);
const EVERY: Duration = Duration::from_secs(12 * 60 * 60);

#[derive(Default, Clone)]
pub struct Updates {
    /// A newer version, when there is one.
    pub latest: Option<Update>,
    /// A look finished since Muxin opened.
    pub checked: bool,
    pub checking: bool,
    pub error: Option<String>,
}

impl Global for Updates {}

/// Starts looking (not in demo mode: it shows what the app does, offline).
pub fn start(engine: Arc<Engine>, cx: &mut App) {
    cx.set_global(Updates::default());
    if engine.is_demo() {
        return;
    }
    cx.spawn(async move |cx| {
        cx.background_executor().timer(FIRST).await;
        loop {
            if engine.settings().check_updates {
                look(&engine, cx).await;
            }
            cx.background_executor().timer(EVERY).await;
        }
    })
    .detach();
}

/// Looks now: the About page's Check for Updates.
pub fn check_now(engine: Arc<Engine>, cx: &mut App) {
    if get(cx).checking {
        return;
    }
    cx.spawn(async move |cx| look(&engine, cx).await).detach();
}

pub fn get(cx: &App) -> Updates {
    cx.try_global::<Updates>().cloned().unwrap_or_default()
}

async fn look(engine: &Arc<Engine>, cx: &mut AsyncApp) {
    let _ = cx.update(|cx| {
        cx.default_global::<Updates>().checking = true;
        cx.refresh_windows();
    });
    let result = engine.runtime().spawn(crate::update::check()).await.unwrap_or_else(|e| Err(e.to_string()));
    let _ = cx.update(|cx| {
        let u = cx.default_global::<Updates>();
        u.checking = false;
        u.checked = true;
        match result {
            Ok(latest) => {
                u.latest = latest;
                u.error = None;
            }
            Err(e) => u.error = Some(e),
        }
        cx.refresh_windows();
    });
}
