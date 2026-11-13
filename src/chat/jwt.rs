//! Checking the identity token a sign-in returns (a JWT), the way OpenAI's
//! sign-in asks: its signature against the issuer's published keys, then who
//! issued it, who it's for, that it hasn't expired, and that it answers this
//! sign-in attempt and no other.
//!
//! Only what that needs: RS256 and ES256 signatures, keys given as a JWKS.

use base64::Engine as _;
use serde_json::Value;

/// How far this computer's clock may be off the issuer's, in seconds.
const SKEW: i64 = 5;

fn b64(text: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(text.trim_end_matches('=')).map_err(|_| "The identity token isn't readable.".to_string())
}

/// Who a checked identity token says signed in.
#[derive(Clone, Debug, PartialEq)]
pub struct Identity {
    /// The account, as the issuer names it. What tells accounts apart.
    pub subject: String,
    /// For showing; two accounts can have the same one.
    pub email: String,
}

/// The claims of `token`, once its signature is checked against `jwks` (the
/// issuer's keys: `{"keys": [...]}`).
pub fn verified(token: &str, jwks: &Value) -> Result<Value, String> {
    let mut parts = token.split('.');
    let (Some(head), Some(body), Some(sig), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err("The identity token isn't readable.".into());
    };
    let header: Value = serde_json::from_slice(&b64(head)?).map_err(|_| "The identity token isn't readable.".to_string())?;
    let signature = b64(sig)?;
    let signed = format!("{head}.{body}");
    let alg = header["alg"].as_str().unwrap_or("");
    let kid = header["kid"].as_str();
    let keys = jwks["keys"].as_array().cloned().unwrap_or_default();
    // The key the token names; with no name on it, any key of the right kind.
    let fits = |key: &&Value| match kid {
        Some(kid) => key["kid"].as_str() == Some(kid),
        None => true,
    };
    let field = |key: &Value, name: &str| key[name].as_str().ok_or(()).and_then(|v| b64(v).map_err(|_| ()));
    let good = keys.iter().filter(fits).any(|key| match (alg, key["kty"].as_str()) {
        ("RS256", Some("RSA")) => {
            let (Ok(n), Ok(e)) = (field(key, "n"), field(key, "e")) else { return false };
            ring::signature::RsaPublicKeyComponents { n: &n, e: &e }.verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, signed.as_bytes(), &signature).is_ok()
        }
        ("ES256", Some("EC")) => {
            let (Ok(x), Ok(y)) = (field(key, "x"), field(key, "y")) else { return false };
            let point = [&[0x04][..], &x, &y].concat();
            ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, point).verify(signed.as_bytes(), &signature).is_ok()
        }
        _ => false,
    });
    if !good {
        return Err(match alg {
            "RS256" | "ES256" => "The identity token's signature doesn't match the issuer's keys.".into(),
            other => format!("The identity token is signed in a way Muxin doesn't check ({other})."),
        });
    }
    serde_json::from_slice(&b64(body)?).map_err(|_| "The identity token isn't readable.".to_string())
}

/// Who signed in, from claims whose signature was checked: issued by
/// `issuer`, for `client_id`, not expired at `now`, and answering the
/// sign-in attempt that sent `nonce`.
pub fn identity(claims: &Value, issuer: &str, client_id: &str, nonce: &str, now: i64) -> Result<Identity, String> {
    if claims["iss"].as_str() != Some(issuer) {
        return Err("The identity token comes from somewhere else.".into());
    }
    let for_us = match &claims["aud"] {
        Value::String(aud) => aud == client_id,
        Value::Array(all) => all.iter().any(|aud| aud.as_str() == Some(client_id)),
        _ => false,
    };
    if !for_us {
        return Err("The identity token is for another app.".into());
    }
    match claims["exp"].as_i64() {
        Some(exp) if exp + SKEW > now => {}
        _ => return Err("The identity token has expired.".into()),
    }
    if claims["iat"].as_i64().is_none() {
        return Err("The identity token doesn't say when it was issued.".into());
    }
    if nonce.is_empty() || claims["nonce"].as_str() != Some(nonce) {
        return Err("The identity token answers another sign-in.".into());
    }
    let subject = claims["sub"].as_str().map(str::trim).filter(|s| !s.is_empty()).ok_or("The identity token doesn't say who signed in.")?;
    if let Some(sid) = claims.get("sid")
        && sid.as_str().is_none_or(|s| s.trim().is_empty())
    {
        return Err("The identity token's session isn't readable.".into());
    }
    Ok(Identity { subject: subject.to_string(), email: claims["email"].as_str().unwrap_or_default().to_string() })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// A token signed (RS256) by a throwaway key, and that key's public half.
    /// Claims: iss https://auth.example.test, sub user-1, aud [oaiapp_test],
    /// nonce nonce-1, email alex@example.com, iat 1794590000, exp 4102444800.
    const TOKEN: &str = "eyJhbGciOiJSUzI1NiIsImtpZCI6InRlc3QtcnNhIiwidHlwIjoiSldUIn0.eyJpc3MiOiJodHRwczovL2F1dGguZXhhbXBsZS50ZXN0Iiwic3ViIjoidXNlci0xIiwiYXVkIjpbIm9haWFwcF90ZXN0Il0sIm5vbmNlIjoibm9uY2UtMSIsImVtYWlsIjoiYWxleEBleGFtcGxlLmNvbSIsImlhdCI6MTc5NDU5MDAwMCwiZXhwIjo0MTAyNDQ0ODAwfQ.XVvbwhSfAzqF3vHtipGaPsdAObE6co2Rs-xd44mZTjEpi9i-AIb8fIODYjBmWY6wQGI_G3s1vYzJVm1k_Ep6HeMwfEjVr71Z_SdPzQpTJMFHxMjb-H8G5atNLzRPZWrh-YKjTOCbxLnCh81Dqy8N3A60KSrjxVai9-ElsIDxt-g6X2v3SypIkXblzJO3fTOyCpjRF01W4W9uAJQ6fu-t_V23jxrvhAmu5B1BUuBSlj-8eEFxs-ESSZhXfTWc3_JcU97eio7UdhcgndNLmZwHc33pyrc-HajlZdqZOIZwarA4VDDbcGpnrsIgU1TahilGXzIivFHwC-sAITsZdPiIHg";
    const N: &str = "jmOshRx2hwLt6lEHtXqDhSenEGDFN7yMZv8sA1XDmkjjv5fOuSrSDmqdx_Qa8A-lUp-nyo3OBHz056dJAzNQfta8ESnUktyfCVGVJlk54C1yViDsPeFzMK5BrKXaPfM6RPSI6F-zplfxUPXPCTgFtBsjOjeEQNWzOyzODIp9ypsmp96OTQGgaEKNboVe413Vl8ZiDf2tVISlQJgmolwNYt7PDAoRvJCm_vo_5f8QyA6ogpKbBjuJr6d9MDwSkosDR8jqInQYEExOIuJrPhfuBYGmD5aJEwjJCeSfjqXSFQU-cAZ6bgRL1pyjGuHrPvZiVG5MWmXFO_X17oId250p8w";

    fn rsa_keys() -> Value {
        json!({"keys": [{"kty": "RSA", "kid": "another", "n": "AQAB", "e": "AQAB"}, {"kty": "RSA", "kid": "test-rsa", "use": "sig", "alg": "RS256", "n": N, "e": "AQAB"}]})
    }

    /// A signer for tests: makes tokens (ES256) with a key of its own, and
    /// publishes that key as a JWKS.
    pub(crate) struct Signer {
        key: ring::signature::EcdsaKeyPair,
        rng: ring::rand::SystemRandom,
    }

    impl Signer {
        pub(crate) fn new() -> Signer {
            let rng = ring::rand::SystemRandom::new();
            let alg = &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING;
            let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(alg, &rng).unwrap();
            Signer { key: ring::signature::EcdsaKeyPair::from_pkcs8(alg, pkcs8.as_ref(), &rng).unwrap(), rng }
        }

        pub(crate) fn jwks(&self) -> Value {
            use ring::signature::KeyPair;
            let enc = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
            let point = self.key.public_key().as_ref();
            json!({"keys": [{"kty": "EC", "crv": "P-256", "kid": "test-ec", "x": enc(&point[1..33]), "y": enc(&point[33..65])}]})
        }

        pub(crate) fn token(&self, claims: &Value) -> String {
            let enc = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
            let signed = format!("{}.{}", enc(br#"{"alg":"ES256","kid":"test-ec"}"#), enc(claims.to_string().as_bytes()));
            let sig = self.key.sign(&self.rng, signed.as_bytes()).unwrap();
            format!("{signed}.{}", enc(sig.as_ref()))
        }
    }

    #[test]
    fn a_token_signed_by_the_issuers_key_gives_its_claims() {
        let claims = verified(TOKEN, &rsa_keys()).unwrap();
        assert_eq!(claims["sub"], "user-1");
        let who = identity(&claims, "https://auth.example.test", "oaiapp_test", "nonce-1", 1_794_600_000).unwrap();
        assert_eq!(who, Identity { subject: "user-1".into(), email: "alex@example.com".into() });
    }

    #[test]
    fn a_token_thats_been_changed_or_signed_by_someone_else_is_refused() {
        // One character of the claims changed: the signature no longer fits.
        let mut parts: Vec<String> = TOKEN.split('.').map(String::from).collect();
        parts[1] = parts[1].replacen("dXNlci0x", "dXNlci0y", 1);
        assert!(verified(&parts.join("."), &rsa_keys()).unwrap_err().contains("signature doesn't match"));
        // The right key isn't among the issuer's.
        let others = json!({"keys": [{"kty": "RSA", "kid": "test-rsa", "n": "AQAB", "e": "AQAB"}]});
        assert!(verified(TOKEN, &others).unwrap_err().contains("signature doesn't match"));
        assert!(verified(TOKEN, &json!({})).unwrap_err().contains("signature doesn't match"));
        // An unsigned token ("alg": "none") is never taken.
        let enc = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        let unsigned = format!("{}.{}.", enc(br#"{"alg":"none"}"#), parts[1]);
        assert!(verified(&unsigned, &rsa_keys()).unwrap_err().contains("doesn't check (none)"));
        for junk in ["", "a.b", "a.b.c.d", "!!.!!.!!"] {
            assert!(verified(junk, &rsa_keys()).is_err(), "{junk}");
        }
    }

    #[test]
    fn an_es256_token_is_checked_too() {
        let signer = Signer::new();
        let token = signer.token(&json!({"iss": "i", "sub": "s", "aud": "c", "nonce": "n", "iat": 1, "exp": 100}));
        assert_eq!(verified(&token, &signer.jwks()).unwrap()["sub"], "s");
        // Another signer's key doesn't vouch for it.
        assert!(verified(&token, &Signer::new().jwks()).is_err());
    }

    #[test]
    fn the_claims_must_be_about_this_sign_in() {
        let good = json!({"iss": "https://auth.openai.com", "sub": "user-1", "aud": ["oaiapp_x"], "nonce": "n1", "iat": 900, "exp": 1000, "email": "a@b.c", "sid": "sid_1"});
        let check = |change: Value, now: i64| {
            let mut claims = good.clone();
            for (k, v) in change.as_object().unwrap() {
                claims[k] = v.clone();
            }
            identity(&claims, "https://auth.openai.com", "oaiapp_x", "n1", now)
        };
        assert_eq!(check(json!({}), 950).unwrap().subject, "user-1");
        // The audience may be one name rather than a list.
        assert!(check(json!({"aud": "oaiapp_x"}), 950).is_ok());
        assert!(check(json!({"iss": "https://evil.example"}), 950).unwrap_err().contains("somewhere else"));
        assert!(check(json!({"aud": ["oaiapp_other"]}), 950).unwrap_err().contains("another app"));
        assert!(check(json!({"nonce": "n2"}), 950).unwrap_err().contains("another sign-in"));
        assert!(check(json!({"sub": " "}), 950).unwrap_err().contains("who signed in"));
        assert!(check(json!({"sid": ""}), 950).unwrap_err().contains("session"));
        // Expired, but for a few seconds of clocks disagreeing.
        assert!(check(json!({}), 1003).is_ok());
        assert!(check(json!({}), 1006).unwrap_err().contains("expired"));
        assert!(check(json!({"exp": null}), 950).unwrap_err().contains("expired"));
        // An attempt that sent no nonce can't be answered.
        assert!(identity(&good, "https://auth.openai.com", "oaiapp_x", "", 950).is_err());
    }
}
