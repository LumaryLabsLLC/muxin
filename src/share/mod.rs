//! Sharing a session or a machine by link, peer to peer from this computer.
//!
//! The person sharing gets a link; whoever opens it joins from Muxin or a
//! browser, straight to this computer over WebRTC (a data channel; an
//! encrypted relay only when no direct path works). This app hosts the
//! share: guests never reach the machine itself, only what the gateway here
//! lets through (`host.rs`). It ends when the share expires, is stopped, or
//! this app quits.
//!
//! The link: `https://muxin.dev/s#1.<ticket>` (`ticket.rs`). The server only
//! introduces the two sides (`signal.rs`); the part after `#` never reaches
//! it. The ticket holds the share's id, a secret, and the SHA-256
//! fingerprint of the share's own DTLS certificate.
//!
//! Joining:
//!
//! 1. The guest leaves an offer in the share's room (its id) at the server;
//!    the host, checking in every couple of seconds, answers it.
//! 2. The guest refuses an answer whose certificate fingerprint isn't the
//!    one in the link, so the server can't sit in the middle.
//! 3. Over the data channel the guest proves it has the secret:
//!    `hello.proof = HMAC-SHA256(secret, "muxin-share-v1\n" + host fp + "\n"
//!    + guest fp)`, bound to both certificates of this connection.
//! 4. Both sides show the same 6-digit code, made from the secret and the
//!    guest's certificate; the host lets the guest in (or a share lets
//!    anyone with the link watch).
//!
//! On the data channel each message is one frame, `[channel: u32 BE][kind:
//! u8][payload]`, or, for one over 48 KB, its parts in order: MORE frames,
//! then the last part with the frame's kind. The link's kinds
//! (`fleet::link`): JSON on channel 0; on
//! the others what the guest opened: a terminal (DATA, EXIT, SIZE, CLOSE),
//! the desktop (TILES and FRAME out, INPUT in) or a server (DATA, CLOSE).
//!
//! Guest to host: `{"op": "hello", "v": 1, "name", "client": "app" |
//! "browser", "proof"}`; once in, `{"op": "watch", "channel", "session"}`
//! (a machine's session; a session's share ignores the name), `{"op":
//! "desktop", "channel"}`, `{"op": "tcp", "channel", "port"}`, `{"op":
//! "close", "channel"}`, `{"op": "bye"}`. Host to guest: `{"event":
//! "share", ...}` (what's shared), `"waiting"`, `"admitted"`, `"denied"`,
//! `"sessions"` (a machine's, when they change), replies `"watching"`,
//! `"desktop"`, `"tcp"` or `"refused"` (`channel`, `error`), `"typing"` and
//! `"control"` (`allowed` changed), `"access"` (the desktop or servers
//! shared changed), `"ended"` (`why`), `"error"`.
//!
//! Pairing an iPad (`pair.rs`) goes the same way, with a link of its own.

pub mod guest;
pub mod host;
pub mod pair;
mod rtc;
mod signal;
#[cfg(test)]
mod tests;
pub mod ticket;

/// Frame kinds on the data channel (the link's).
pub const JSON: u8 = 10;
pub const DATA: u8 = 0;
pub const EXIT: u8 = 2;
pub const SIZE: u8 = 3;
pub const CLOSE: u8 = 11;
/// Part of a frame too big for one message; the rest follows on its channel.
pub const MORE: u8 = 12;

/// A frame for the data channel.
pub fn frame(channel: u32, kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(5 + payload.len());
    f.extend(channel.to_be_bytes());
    f.push(kind);
    f.extend(payload);
    f
}

/// A frame's channel, kind and payload.
pub fn parse_frame(msg: &[u8]) -> Option<(u32, u8, &[u8])> {
    let head = msg.get(..5)?;
    Some((u32::from_be_bytes([head[0], head[1], head[2], head[3]]), head[4], &msg[5..]))
}

/// The share server used when Settings don't name another (or
/// `MUXIN_SHARE_SERVER`, for a server of your own while developing).
pub fn default_server() -> String {
    std::env::var("MUXIN_SHARE_SERVER").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "https://muxin.dev".into())
}
