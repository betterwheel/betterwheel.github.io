//! OAuth 2.0 `private_key_jwt` authentication for the IBKR Web API.
//!
//! Builds an RS256-signed `client_assertion` JWT from your RSA private key and
//! exchanges it for an access token. No secret is sent over the wire — only a
//! short-lived signed assertion. See `SETUP.md` for obtaining the credentials.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Serialize;

use super::models::TokenResponse;
use crate::config::OAuthConfig;

/// Claims for the `client_assertion` JWT (RFC 7521/7523 private_key_jwt).
#[derive(Debug, Serialize)]
pub struct AssertionClaims {
    pub iss: String,
    pub sub: String,
    pub aud: String,
    pub exp: u64,
    pub iat: u64,
    pub jti: String,
}

/// Build the assertion claims. Pure — `now` is unix seconds, `jti` supplied by
/// the caller — so it can be unit-tested without a key or clock.
pub fn build_claims(oauth: &OAuthConfig, token_url: &str, now: u64, jti: String) -> AssertionClaims {
    AssertionClaims {
        iss: oauth.client_id.clone(),
        sub: oauth.credential.clone(),
        aud: token_url.to_string(),
        iat: now,
        exp: now + 60,
        jti,
    }
}

/// Sign claims into a compact JWT with the RSA private key (RS256).
pub fn sign_assertion(claims: &AssertionClaims, kid: &str, private_key_pem: &[u8]) -> Result<String> {
    let key = EncodingKey::from_rsa_pem(private_key_pem)
        .context("loading RSA private key (expected a PKCS#1/PKCS#8 PEM)")?;
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.to_string());
    encode(&header, claims, &key).context("signing client_assertion JWT")
}

/// Read the key file and produce a fresh signed client assertion.
pub fn make_client_assertion(oauth: &OAuthConfig, token_url: &str) -> Result<String> {
    let pem = std::fs::read(&oauth.private_key_path)
        .with_context(|| format!("reading private key {}", oauth.private_key_path.display()))?;
    let now = unix_now()?;
    let claims = build_claims(oauth, token_url, now, uuid::Uuid::new_v4().to_string());
    sign_assertion(&claims, &oauth.kid, &pem)
}

/// Exchange a signed assertion for an access token at the token endpoint.
pub async fn fetch_access_token(
    http: &reqwest::Client,
    oauth: &OAuthConfig,
    token_url: &str,
) -> Result<TokenResponse> {
    let assertion = make_client_assertion(oauth, token_url)?;
    let mut form: Vec<(&str, String)> = vec![
        ("grant_type", oauth.grant_type.clone()),
        (
            "client_assertion_type",
            "urn:ietf:params:oauth:client-assertion-type:jwt-bearer".to_string(),
        ),
        ("client_assertion", assertion),
    ];
    if let Some(scope) = &oauth.scope {
        form.push(("scope", scope.clone()));
    }

    let resp = http
        .post(token_url)
        .form(&form)
        .send()
        .await
        .context("token request failed")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("token endpoint returned {status}: {body}"));
    }
    serde_json::from_str::<TokenResponse>(&body)
        .with_context(|| format!("parsing token response: {body}"))
}

fn unix_now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn oauth() -> OAuthConfig {
        OAuthConfig {
            client_id: "MYCONSUMER".into(),
            kid: "key1".into(),
            credential: "myuser".into(),
            private_key_path: PathBuf::from("/dev/null"),
            token_url: None,
            scope: None,
            grant_type: "urn:ietf:params:oauth:grant-type:jwt-bearer".into(),
        }
    }

    #[test]
    fn claims_shape() {
        let c = build_claims(
            &oauth(),
            "https://api.ibkr.com/v1/api/oauth2/token",
            1_000_000,
            "jti-1".into(),
        );
        assert_eq!(c.iss, "MYCONSUMER");
        assert_eq!(c.sub, "myuser");
        assert_eq!(c.aud, "https://api.ibkr.com/v1/api/oauth2/token");
        assert_eq!(c.iat, 1_000_000);
        assert_eq!(c.exp, 1_000_060); // 60s lifetime
        assert_eq!(c.jti, "jti-1");
    }
}
