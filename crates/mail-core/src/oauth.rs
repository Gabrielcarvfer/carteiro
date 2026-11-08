//! OAuth 2.0 sign-in for Google and Microsoft: authorization code flow with
//! PKCE and a loopback redirect (RFC 8252), as required for desktop apps.
//! Tokens never pass through anything but the provider and this process.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::account::AuthMethod;
use crate::error::{Error, Result};
use crate::http::{self, decode, encode, Request};

/// App registration. Client IDs of installed apps are not secret; Google
/// still issues a "secret" for desktop clients, which is sent if present.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientConfig {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// IMAP + SMTP.
    Mail,
    /// CalDAV (Google) or Graph (Microsoft).
    Calendar,
    /// CardDAV (Google) or Graph (Microsoft).
    Contacts,
}

struct Endpoints {
    auth: &'static str,
    token: &'static str,
}

fn endpoints(p: AuthMethod) -> Result<Endpoints> {
    match p {
        AuthMethod::Google => Ok(Endpoints { auth: "https://accounts.google.com/o/oauth2/v2/auth", token: "https://oauth2.googleapis.com/token" }),
        AuthMethod::Microsoft => Ok(Endpoints {
            auth: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
            token: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
        }),
        AuthMethod::Password => Err(Error::Other("not an OAuth account".into())),
    }
}

/// Scopes requested at sign-in (consent for everything at once).
pub fn consent_scopes(p: AuthMethod) -> &'static str {
    match p {
        AuthMethod::Google => "openid email profile https://mail.google.com/ https://www.googleapis.com/auth/calendar https://www.googleapis.com/auth/carddav",
        _ => "openid email profile offline_access https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send https://graph.microsoft.com/Calendars.ReadWrite https://graph.microsoft.com/Contacts.Read",
    }
}

/// Scopes for an access token. Microsoft issues one token per resource, so
/// mail (outlook.office.com) and calendar (graph.microsoft.com) differ.
pub fn token_scopes(p: AuthMethod, purpose: Purpose) -> &'static str {
    match (p, purpose) {
        (AuthMethod::Google, _) => "https://mail.google.com/ https://www.googleapis.com/auth/calendar https://www.googleapis.com/auth/carddav",
        (_, Purpose::Mail) => "offline_access https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send",
        (_, Purpose::Calendar) => "offline_access https://graph.microsoft.com/Calendars.ReadWrite",
        (_, Purpose::Contacts) => "offline_access https://graph.microsoft.com/Contacts.Read",
    }
}

#[derive(Debug, Clone)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix seconds.
    pub expires_at: i64,
    pub email: Option<String>,
    pub name: Option<String>,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn random_b64url(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut buf).map_err(|_| Error::Other("no randomness".into()))?;
    Ok(crate::b64::encode_with(buf, crate::b64::URL_SAFE, false))
}

/// PKCE S256 challenge for a verifier.
pub fn challenge(verifier: &str) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
    crate::b64::encode_with(d.as_ref(), crate::b64::URL_SAFE, false)
}

/// An in-progress sign-in: open `url` in the browser, then `finish()`.
pub struct Pending {
    pub url: String,
    provider: AuthMethod,
    client: ClientConfig,
    listener: TcpListener,
    redirect: String,
    verifier: String,
    state: String,
}

pub async fn begin(provider: AuthMethod, client: &ClientConfig, login_hint: Option<&str>) -> Result<Pending> {
    if client.client_id.trim().is_empty() {
        return Err(Error::Other("No OAuth client ID configured for this provider (Settings → Sign-in apps).".into()));
    }
    let ep = endpoints(provider)?;
    // Loopback only: the redirect can't be intercepted from the network.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let redirect = format!("http://127.0.0.1:{}/", listener.local_addr()?.port());
    let verifier = random_b64url(48)?;
    let state = random_b64url(24)?;
    let mut url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        ep.auth,
        encode(&client.client_id),
        encode(&redirect),
        encode(consent_scopes(provider)),
        challenge(&verifier),
        state
    );
    if provider == AuthMethod::Google {
        // Needed to get a refresh token on every sign-in.
        url.push_str("&access_type=offline&prompt=consent");
    } else {
        url.push_str("&prompt=select_account");
    }
    if let Some(h) = login_hint {
        url.push_str(&format!("&login_hint={}", encode(h)));
    }
    Ok(Pending { url, provider, client: client.clone(), listener, redirect, verifier, state })
}

const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Carteiro</title><body style=\"font:16px system-ui;text-align:center;padding:60px\"><h2>Signed in</h2><p>You can close this tab and return to Carteiro.</p>";
const FAIL_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Carteiro</title><body style=\"font:16px system-ui;text-align:center;padding:60px\"><h2>Sign-in failed</h2><p>Return to Carteiro and try again.</p>";

impl Pending {
    /// Waits (up to 5 minutes) for the browser redirect, then redeems the code.
    pub async fn finish(self) -> Result<Tokens> {
        let code = tokio::time::timeout(std::time::Duration::from_secs(300), self.wait_for_code())
            .await
            .map_err(|_| Error::Other("Sign-in timed out".into()))??;
        let ep = endpoints(self.provider)?;
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", self.redirect.as_str()),
            ("client_id", self.client.client_id.as_str()),
            ("code_verifier", self.verifier.as_str()),
        ];
        if let Some(s) = &self.client.client_secret {
            form.push(("client_secret", s));
        }
        if self.provider == AuthMethod::Microsoft {
            form.push(("scope", token_scopes(self.provider, Purpose::Mail)));
        }
        token_request(ep.token, &form).await
    }

    async fn wait_for_code(&self) -> Result<String> {
        loop {
            let (mut sock, _) = self.listener.accept().await?;
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await?;
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let target = req.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("");
            let query = target.split_once('?').map(|x| x.1).unwrap_or("");
            let params: Vec<(String, String)> = query.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (decode(k), decode(v))).collect();
            let get = |k: &str| params.iter().find(|(a, _)| a == k).map(|(_, b)| b.clone());
            if get("state").is_none() && get("error").is_none() {
                // Browsers also ask for /favicon.ico etc.
                let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                continue;
            }
            let ok = get("state").as_deref() == Some(self.state.as_str()) && get("code").is_some();
            let page = if ok { DONE_PAGE } else { FAIL_PAGE };
            let _ = sock
                .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len()).as_bytes())
                .await;
            if let Some(err) = get("error") {
                return Err(Error::Auth(get("error_description").unwrap_or(err)));
            }
            if !ok {
                return Err(Error::Auth("sign-in response didn't match the request".into()));
            }
            return Ok(get("code").unwrap());
        }
    }
}

/// Exchanges a refresh token for an access token with the given scopes.
pub async fn refresh(provider: AuthMethod, client: &ClientConfig, refresh_token: &str, purpose: Purpose) -> Result<Tokens> {
    let ep = endpoints(provider)?;
    let mut form = vec![("grant_type", "refresh_token"), ("refresh_token", refresh_token), ("client_id", client.client_id.as_str())];
    if let Some(s) = &client.client_secret {
        form.push(("client_secret", s));
    }
    if provider == AuthMethod::Microsoft {
        form.push(("scope", token_scopes(provider, purpose)));
    }
    token_request(ep.token, &form).await
}

async fn token_request(url: &str, form: &[(&str, &str)]) -> Result<Tokens> {
    let r = http::send(Request::new("POST", url).header("Accept", "application/json").form(form)).await?;
    let v = r.json().unwrap_or_default();
    if !r.is_success() {
        let msg = v.get("error_description").or_else(|| v.get("error")).and_then(|e| e.as_str()).unwrap_or("token request failed");
        return Err(Error::Auth(msg.lines().next().unwrap_or(msg).to_string()));
    }
    parse_token_response(&v)
}

pub fn parse_token_response(v: &serde_json::Value) -> Result<Tokens> {
    let str_of = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let access_token = str_of("access_token").ok_or_else(|| Error::Auth("no access token in response".into()))?;
    let claims = str_of("id_token").and_then(|t| id_token_claims(&t));
    let claim = |k: &str| claims.as_ref().and_then(|c| c.get(k)).and_then(|x| x.as_str()).map(str::to_string);
    Ok(Tokens {
        access_token,
        refresh_token: str_of("refresh_token"),
        expires_at: now() + v.get("expires_in").and_then(|x| x.as_i64()).unwrap_or(3600) - 60,
        email: claim("email").or_else(|| claim("preferred_username")),
        name: claim("name"),
    })
}

/// Reads the payload of an ID token. It came straight from the provider's
/// token endpoint over TLS, so the signature isn't re-verified here.
fn id_token_claims(jwt: &str) -> Option<serde_json::Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = crate::b64::decode_with(payload, crate::b64::URL_SAFE)?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_rfc7636_vector() {
        assert_eq!(challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn token_response_with_id_token() {
        let claims = crate::b64::encode_with(r#"{"email":"me@gmail.com","name":"Me"}"#, crate::b64::URL_SAFE, false);
        let v = serde_json::json!({"access_token":"at","refresh_token":"rt","expires_in":3600,"id_token":format!("h.{claims}.s")});
        let t = parse_token_response(&v).unwrap();
        assert_eq!((t.access_token.as_str(), t.refresh_token.as_deref(), t.email.as_deref()), ("at", Some("rt"), Some("me@gmail.com")));
    }

    #[tokio::test]
    async fn loopback_redirect_checks_state() {
        let client = ClientConfig { client_id: "id".into(), client_secret: None };
        let p = begin(AuthMethod::Google, &client, Some("me@gmail.com")).await.unwrap();
        assert!(p.url.contains("code_challenge_method=S256") && p.url.contains("login_hint=me%40gmail.com"));
        let port: u16 = p.redirect.trim_end_matches('/').rsplit(':').next().unwrap().parse().unwrap();
        let state = p.state.clone();
        let browser = tokio::spawn(async move {
            // favicon request first, then a forged state, then the real one.
            for path in ["/favicon.ico".to_string(), format!("/?code=abc&state={state}")] {
                let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
                s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).await.unwrap();
                let mut out = String::new();
                let _ = s.read_to_string(&mut out).await;
            }
        });
        assert_eq!(p.wait_for_code().await.unwrap(), "abc");
        browser.await.unwrap();
    }
}
