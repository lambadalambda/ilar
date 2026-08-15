//! ChatGPT OAuth (PKCE) login for the OpenAI provider — the Codex-CLI
//! flow: authorize at auth.openai.com, callback on 127.0.0.1:1455,
//! token exchange with S256 PKCE, refresh-token rotation.

use anyhow::{Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTH_BASE: &str = "https://auth.openai.com";
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

#[derive(Debug, Clone, PartialEq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// S256 code challenge for a verifier (RFC 7636).
pub fn pkce_from_verifier(verifier: &str) -> Pkce {
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    Pkce {
        verifier: verifier.to_string(),
        challenge,
    }
}

fn random_verifier() -> String {
    // Three UUIDv4s joined: ~108 chars of unreserved-set entropy
    // (43..=128 chars, 256+ bits).
    (0..3)
        .map(|_| uuid::Uuid::new_v4().to_string())
        .collect::<Vec<_>>()
        .join("")
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

/// chatgpt_account_id from the id_token JWT claims (no signature check:
/// the token arrived over TLS from the issuer).
pub fn account_id_from_id_token(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(String::from)
}

/// File-backed token store: `<state dir>/auth.json`.
#[derive(Debug, Clone)]
pub struct AuthStore {
    path: std::path::PathBuf,
}

impl AuthStore {
    pub fn open(state_dir: std::path::PathBuf) -> Self {
        Self {
            path: state_dir.join("auth.json"),
        }
    }

    pub fn tokens_path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn load(&self) -> Option<TokenSet> {
        serde_json::from_str(&std::fs::read_to_string(&self.path).ok()?).ok()
    }

    pub fn save(&self, tokens: &TokenSet) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::atomic_file::replace(
            &self.path,
            serde_json::to_string_pretty(tokens)?.as_bytes(),
            crate::atomic_file::Mode::Force(0o600),
        )?;
        Ok(())
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

async fn token_post(
    http: &reqwest::Client,
    token_url: &str,
    form: &[(&str, &str)],
) -> Result<TokenSet> {
    let response = http
        .post(token_url)
        .form(form)
        .send()
        .await
        .context("token endpoint request")?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("token endpoint HTTP {status}: {body}");
    }
    let parsed: TokenResponse = serde_json::from_str(&body).context("token response")?;
    let expires_at = chrono::Utc::now().timestamp() + parsed.expires_in.unwrap_or(3600).max(60);
    let account_id = parsed
        .id_token
        .as_deref()
        .and_then(account_id_from_id_token);
    Ok(TokenSet {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        id_token: parsed.id_token,
        account_id,
        expires_at: Some(expires_at),
    })
}

/// Exchange the authorization code (plus PKCE verifier) for tokens.
pub async fn exchange_code(code: &str, verifier: &str) -> Result<TokenSet> {
    let http = reqwest::Client::new();
    token_post(
        &http,
        &format!("{AUTH_BASE}/oauth/token"),
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", verifier),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
        ],
    )
    .await
}

/// Refresh via the stored refresh token; persists the rotated set.
pub async fn refresh_tokens(
    store: &AuthStore,
    token_url: &str,
    http: &reqwest::Client,
) -> Result<TokenSet> {
    let current = store
        .load()
        .context("no stored tokens — run `ilar login`")?;
    let refresh_token = current
        .refresh_token
        .clone()
        .context("no refresh token stored — run `ilar login`")?;
    let mut next = token_post(
        http,
        token_url,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh_token),
            ("client_id", CLIENT_ID),
        ],
    )
    .await?;
    if next.refresh_token.is_none() {
        next.refresh_token = Some(refresh_token);
    }
    if next.account_id.is_none() {
        next.account_id = current.account_id;
    }
    store.save(&next)?;
    Ok(next)
}

/// Parse `GET /auth/callback?code=..&state=..` from the request head.
pub fn parse_callback(request_head: &[u8]) -> Option<(String, String)> {
    let text = String::from_utf8_lossy(request_head);
    let first = text.lines().next()?;
    let target = first.split_whitespace().nth(1)?;
    let query = target.split_once('?')?.1;
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=')?;
        match k {
            "code" => code = Some(v.to_string()),
            "state" => state = Some(v.to_string()),
            _ => {}
        }
    }
    Some((code?, state?))
}

/// The interactive login: browser -> localhost:1455 callback -> exchange
/// -> store. `open_browser` spawns the OS opener (tests pass false; the
/// URL is always printed).
pub async fn login_flow(
    store: &AuthStore,
    timeout: std::time::Duration,
    open_browser: bool,
) -> Result<TokenSet> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 1455))
        .await
        .context("binding callback port 1455 (another login running?)")?;
    let verifier = random_verifier();
    let pkce = pkce_from_verifier(&verifier);
    let state = uuid::Uuid::new_v4().to_string();
    let url = format!(
        "{AUTH_BASE}/oauth/authorize\
?client_id={CLIENT_ID}\
&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback\
&response_type=code\
&scope=openid%20profile%20email%20offline_access\
&code_challenge={}&code_challenge_method=S256&state={}",
        pkce.challenge, state
    );

    if open_browser {
        let _ = std::process::Command::new("open").arg(&url).spawn();
    }
    println!("Log in with your ChatGPT account:\n\n{url}\n");

    let (mut socket, _) = tokio::time::timeout(timeout, listener.accept())
        .await
        .context("timed out waiting for the browser callback")?
        .context("accepting callback connection")?;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = socket.read(&mut chunk).await.context("reading callback")?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let (code, returned_state) = parse_callback(&buf).context("callback missing code/state")?;
    anyhow::ensure!(returned_state == state, "OAuth state mismatch");
    socket
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
<html><body><h2>ilar: logged in</h2>You can close this tab.</body></html>",
        )
        .await
        .ok();

    let mut tokens = exchange_code(&code, &verifier).await?;
    if tokens.account_id.is_none() {
        tokens.account_id = tokens
            .id_token
            .as_deref()
            .and_then(account_id_from_id_token);
    }
    store.save(&tokens)?;
    Ok(tokens)
}
