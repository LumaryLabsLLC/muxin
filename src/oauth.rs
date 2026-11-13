//! What every browser sign-in in Muxin shares (OAuth as a public client: no
//! secret ships in the app). Each sign-in proves itself with PKCE, and the
//! service sends the browser back to a port on this computer.

use std::collections::HashMap;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// PKCE: a random verifier, and its SHA-256 (base64url) for the authorization request.
pub fn pkce() -> (String, String) {
    let verifier = crate::util::random_token(64);
    let challenge = challenge_for(&verifier);
    (verifier, challenge)
}

pub fn challenge_for(verifier: &str) -> String {
    use base64::Engine as _;
    let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.as_ref())
}

/// A port on this computer the browser is sent back to when a sign-in is
/// done. Only this computer can reach it (127.0.0.1).
pub struct Loopback {
    listener: tokio::net::TcpListener,
    path: String,
    /// Where the service should send the browser: `http://127.0.0.1:<port><path>`.
    pub redirect_uri: String,
}

impl Loopback {
    /// Listens on the first of `ports` that's free, else on any free port,
    /// for a request to `path`. Do this before opening the browser.
    pub async fn bind(ports: &[u16], path: &str) -> Result<Loopback, String> {
        let mut listener = None;
        for port in ports {
            if let Ok(l) = tokio::net::TcpListener::bind(("127.0.0.1", *port)).await {
                listener = Some(l);
                break;
            }
        }
        let listener = match listener {
            Some(l) => l,
            None => tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| format!("Couldn't wait for the sign-in's answer: {e}"))?,
        };
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        Ok(Loopback { listener, path: path.to_string(), redirect_uri: format!("http://127.0.0.1:{port}{path}") })
    }

    /// Waits for the browser to come back, and returns what it brought (the
    /// request's query). `done(query)` says whether that's a sign-in that
    /// worked, for the page the user is left looking at.
    pub async fn wait(self, timeout: Duration, done: impl Fn(&HashMap<String, String>) -> bool) -> Result<HashMap<String, String>, String> {
        let accept = async {
            loop {
                let (mut sock, _) = self.listener.accept().await.map_err(|e| e.to_string())?;
                let mut buf = vec![0u8; 16 * 1024];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let target = request.split_whitespace().nth(1).unwrap_or("").to_string();
                let parsed = reqwest::Url::parse(&format!("http://127.0.0.1{target}")).ok();
                // Something else asking (a favicon): not the answer.
                let Some(url) = parsed.filter(|u| u.path() == self.path) else {
                    let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                    continue;
                };
                let query: HashMap<String, String> = url.query_pairs().map(|(k, v)| (k.to_string(), v.to_string())).collect();
                let body = if done(&query) { "Signed in. You can close this tab and go back to Muxin." } else { "The sign-in didn't finish. Go back to Muxin and try again." };
                let page = format!("<!doctype html><meta charset=utf-8><title>Muxin</title><body style=\"font:15px -apple-system,system-ui;padding:40px\">{body}</body>");
                let _ = sock.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}", page.len()).as_bytes()).await;
                let _ = sock.shutdown().await;
                return Ok::<_, String>(query);
            }
        };
        tokio::time::timeout(timeout, accept).await.map_err(|_| "The sign-in timed out.".to_string())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_the_rfc() {
        // RFC 7636, appendix B.
        assert_eq!(challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        let (v, c) = pkce();
        assert_eq!(v.len(), 64);
        assert_eq!(c, challenge_for(&v));
    }

    #[tokio::test]
    async fn the_browser_comes_back_with_its_answer() {
        let loopback = Loopback::bind(&[], "/auth/callback").await.unwrap();
        let uri = loopback.redirect_uri.clone();
        assert!(uri.starts_with("http://127.0.0.1:") && uri.ends_with("/auth/callback"), "{uri}");
        let waiting = tokio::spawn(loopback.wait(Duration::from_secs(10), |q| q.contains_key("code")));
        let http = reqwest::Client::new();
        // A request for something else doesn't end the wait.
        assert_eq!(http.get(uri.replace("/auth/callback", "/favicon.ico")).send().await.unwrap().status(), 404);
        let page = http.get(format!("{uri}?code=abc&state=x%20y")).send().await.unwrap().text().await.unwrap();
        assert!(page.contains("Signed in."));
        let query = waiting.await.unwrap().unwrap();
        assert_eq!((query["code"].as_str(), query["state"].as_str()), ("abc", "x y"));
    }

    #[tokio::test]
    async fn a_port_thats_taken_falls_back_to_a_free_one_and_a_refusal_says_so() {
        let first = Loopback::bind(&[], "/cb").await.unwrap();
        let taken: u16 = first.redirect_uri.trim_start_matches("http://127.0.0.1:").trim_end_matches("/cb").parse().unwrap();
        let second = Loopback::bind(&[taken], "/cb").await.unwrap();
        assert_ne!(second.redirect_uri, first.redirect_uri);
        let uri = second.redirect_uri.clone();
        let waiting = tokio::spawn(second.wait(Duration::from_secs(10), |q| q.contains_key("code")));
        let page = reqwest::get(format!("{uri}?error=access_denied")).await.unwrap().text().await.unwrap();
        assert!(page.contains("didn't finish"));
        assert_eq!(waiting.await.unwrap().unwrap()["error"], "access_denied");
        // Nobody comes back: the wait ends by itself.
        let nobody = Loopback::bind(&[], "/cb").await.unwrap();
        assert_eq!(nobody.wait(Duration::from_millis(50), |_| true).await.unwrap_err(), "The sign-in timed out.");
    }
}
