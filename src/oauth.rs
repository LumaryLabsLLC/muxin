//! What a browser sign-in in Muxin proves itself with (OAuth as a public
//! client: no secret ships in the app): PKCE.

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
}
