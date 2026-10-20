//! The share server, from either side: where the two meet before they talk
//! directly (share-server/ in this repository, and muxin.dev's own). HTTP
//! and JSON under `<server>/share/v1`; a room is a share's id:
//!
//! - `POST /rooms/{room}/host`, with `X-Muxin-Host: <key>`: the sharing app
//!   checks in (every couple of seconds) and takes the guests' offers
//!   waiting: `{ice, offers: [{id, sdp}]}`. 409 when another computer holds
//!   the room.
//! - `POST /rooms/{room}/offers/{id}/answer` (the host): `{sdp}`.
//! - `DELETE /rooms/{room}` (the host): the share ended.
//! - `GET /rooms/{room}`: `{open, ice}`, for a guest.
//! - `POST /rooms/{room}/offers`: a guest's `{sdp}`, answered `{id}`.
//! - `GET /rooms/{room}/offers/{id}`: `{answer, open}`, polled by the guest.
//!
//! Errors are `{error}` with a status. Descriptions go whole, every
//! candidate in them (no trickling), and `ice` names the STUN and TURN
//! servers to use.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Value, json};

/// How the server answered when it wasn't a yes.
#[derive(Debug)]
pub enum Error {
    /// Another computer holds this share's room.
    Taken,
    /// Not there (the share isn't open, the offer expired).
    Gone(String),
    Other(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Taken => f.write_str("That share is open on another computer."),
            Error::Gone(e) | Error::Other(e) => f.write_str(e),
        }
    }
}

#[derive(Clone)]
pub struct Server {
    base: String,
    http: reqwest::Client,
}

/// A host's key for its room: only it can check in there.
pub fn host_key() -> String {
    let mut key = [0u8; 32];
    let _ = SystemRandom::new().fill(&mut key);
    URL_SAFE_NO_PAD.encode(key)
}

impl Server {
    /// The server at `origin` (`https://muxin.dev`).
    pub fn new(origin: &str) -> Server {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(15)).connect_timeout(Duration::from_secs(8)).build().unwrap_or_default();
        Server { base: format!("{}/share/v1", origin.trim_end_matches('/')), http }
    }

    async fn call(&self, req: reqwest::RequestBuilder) -> Result<Value, Error> {
        let res = req.header("Accept", "application/json").send().await.map_err(|e| Error::Other(format!("Couldn't reach the share server: {}", short(e))))?;
        let status = res.status();
        let body: Value = res.json().await.unwrap_or(Value::Null);
        let why = || body["error"].as_str().map(str::to_string).unwrap_or_else(|| format!("The share server said {status}."));
        match status.as_u16() {
            200..=299 => Ok(body),
            409 => Err(Error::Taken),
            404 | 410 => Err(Error::Gone(why())),
            _ => Err(Error::Other(why())),
        }
    }

    /// The host checks in: the STUN/TURN servers, and the offers waiting.
    pub async fn check_in(&self, room: &str, key: &str) -> Result<(Value, Vec<(String, String)>), Error> {
        let v = self.call(self.http.post(format!("{}/rooms/{room}/host", self.base)).header("X-Muxin-Host", key).json(&json!({}))).await?;
        let offers = v["offers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|o| Some((o["id"].as_str()?.to_string(), o["sdp"].as_str()?.to_string())))
            .collect();
        Ok((v["ice"].clone(), offers))
    }

    pub async fn answer(&self, room: &str, key: &str, id: &str, sdp: &str) -> Result<(), Error> {
        self.call(self.http.post(format!("{}/rooms/{room}/offers/{id}/answer", self.base)).header("X-Muxin-Host", key).json(&json!({ "sdp": sdp }))).await.map(|_| ())
    }

    /// The share ended: guests still knocking hear so.
    pub async fn close(&self, room: &str, key: &str) {
        let _ = self.call(self.http.delete(format!("{}/rooms/{room}", self.base)).header("X-Muxin-Host", key)).await;
    }

    /// Whether the share is open, and the STUN/TURN servers to use.
    pub async fn room(&self, room: &str) -> Result<(bool, Value), Error> {
        let v = self.call(self.http.get(format!("{}/rooms/{room}", self.base))).await?;
        Ok((v["open"].as_bool() == Some(true), v["ice"].clone()))
    }

    /// Leaves a guest's offer; its id, to wait for the answer with.
    pub async fn offer(&self, room: &str, sdp: &str) -> Result<String, Error> {
        let v = self.call(self.http.post(format!("{}/rooms/{room}/offers", self.base)).json(&json!({ "sdp": sdp }))).await?;
        v["id"].as_str().map(str::to_string).ok_or_else(|| Error::Other("The share server sent something odd.".into()))
    }

    /// The answer to an offer, once there is one, and whether the share is still open.
    pub async fn answer_to(&self, room: &str, id: &str) -> Result<(Option<String>, bool), Error> {
        let v = self.call(self.http.get(format!("{}/rooms/{room}/offers/{id}", self.base))).await?;
        Ok((v["answer"].as_str().map(str::to_string), v["open"].as_bool() != Some(false)))
    }
}

/// An error without the URL (which says nothing useful to the user).
fn short(e: reqwest::Error) -> String {
    if e.is_timeout() {
        "it didn't answer".into()
    } else if e.is_connect() {
        "no connection".into()
    } else {
        e.without_url().to_string()
    }
}
