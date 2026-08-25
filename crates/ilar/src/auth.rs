//! ChatGPT OAuth (PKCE) login for the OpenAI provider — the Codex-CLI
//! flow: authorize at auth.openai.com, callback on 127.0.0.1:1455,
//! token exchange with S256 PKCE, refresh-token rotation.

use anyhow::{Context, Result};
use base64::Engine;
use fs2::FileExt;
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

    pub fn load(&self) -> Result<Option<TokenSet>> {
        let Some(content) = read_store(&self.path)
            .with_context(|| format!("reading OAuth store {}", self.path.display()))?
        else {
            return Ok(None);
        };
        serde_json::from_str(&content)
            .with_context(|| format!("parsing OAuth store {}", self.path.display()))
            .map(Some)
    }

    fn save_unlocked(&self, tokens: &TokenSet) -> Result<()> {
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

    async fn load_async(&self) -> Result<Option<TokenSet>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.load())
            .await
            .context("OAuth store read task failed")?
    }

    async fn save_with_lock_async(&self, tokens: &TokenSet, lock: std::fs::File) -> Result<()> {
        let store = self.clone();
        let tokens = tokens.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = lock;
            store.save_unlocked(&tokens)
        })
        .await
        .context("OAuth store write task failed")?
    }

    pub async fn save(&self, tokens: &TokenSet) -> Result<()> {
        let store = self.clone();
        let tokens = tokens.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = store.acquire_refresh_lock_blocking()?;
            store.save_unlocked(&tokens)
        })
        .await
        .context("OAuth store write task failed")?
    }

    fn acquire_refresh_lock_blocking(&self) -> Result<std::fs::File> {
        let path = self.path.with_extension("lock");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path)?;
        FileExt::lock_exclusive(&file)?;
        Ok(file)
    }

    async fn acquire_refresh_lock(&self) -> Result<std::fs::File> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.acquire_refresh_lock_blocking())
            .await
            .context("OAuth refresh lock task failed")?
            .with_context(|| format!("locking OAuth store {}", self.path.display()))
    }
}

#[cfg(unix)]
fn read_store(path: &std::path::Path) -> std::io::Result<Option<String>> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = std::fs::OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return match std::fs::symlink_metadata(path) {
                Err(metadata_error) if metadata_error.kind() == std::io::ErrorKind::NotFound => {
                    Ok(None)
                }
                _ => Err(error),
            };
        }
        Err(error) => return Err(error),
    };
    let mut content = String::new();
    std::io::Read::read_to_string(&mut file, &mut content)?;
    Ok(Some(content))
}

#[cfg(not(unix))]
fn read_store(path: &std::path::Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// `expires_in` is deliberately not kept: refresh is driven by the 401
/// the provider returns, so a stored expiry would be a second source of
/// truth that nothing consults.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

const TOKEN_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const MAX_TOKEN_RESPONSE_BYTES: usize = 1024 * 1024;

async fn token_post(
    http: &reqwest::Client,
    token_url: &str,
    form: &[(&str, &str)],
    timeout: std::time::Duration,
) -> Result<TokenSet> {
    let mut response = http
        .post(token_url)
        .form(form)
        .timeout(timeout)
        .send()
        .await
        .context("token endpoint request")?;
    let status = response.status();
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("reading token endpoint response")?
    {
        anyhow::ensure!(
            body.len().saturating_add(chunk.len()) <= MAX_TOKEN_RESPONSE_BYTES,
            "token endpoint response exceeds {MAX_TOKEN_RESPONSE_BYTES} bytes"
        );
        body.extend_from_slice(&chunk);
    }
    let body = String::from_utf8_lossy(&body);
    if !status.is_success() {
        anyhow::bail!("token endpoint HTTP {status}: {body}");
    }
    let parsed: TokenResponse = serde_json::from_str(&body).context("token response")?;
    let account_id = parsed
        .id_token
        .as_deref()
        .and_then(account_id_from_id_token);
    Ok(TokenSet {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        id_token: parsed.id_token,
        account_id,
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
        TOKEN_REQUEST_TIMEOUT,
    )
    .await
}

/// Refresh via the stored refresh token; persists the rotated set.
pub async fn refresh_tokens(
    store: &AuthStore,
    rejected_access_token: &str,
    token_url: &str,
    http: &reqwest::Client,
) -> Result<TokenSet> {
    refresh_tokens_with_timeout(
        store,
        rejected_access_token,
        token_url,
        http,
        TOKEN_REQUEST_TIMEOUT,
    )
    .await
}

async fn refresh_tokens_with_timeout(
    store: &AuthStore,
    rejected_access_token: &str,
    token_url: &str,
    http: &reqwest::Client,
    request_timeout: std::time::Duration,
) -> Result<TokenSet> {
    let lock = store.acquire_refresh_lock().await?;
    let current = store
        .load_async()
        .await
        .context("loading stored OAuth tokens")?
        .context("no stored tokens — run `ilar login`")?;
    if current.access_token != rejected_access_token {
        return Ok(current);
    }
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
        request_timeout,
    )
    .await?;
    if next.refresh_token.is_none() {
        next.refresh_token = Some(refresh_token);
    }
    if next.account_id.is_none() {
        next.account_id = current.account_id;
    }
    store.save_with_lock_async(&next, lock).await?;
    Ok(next)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthCallback {
    Code {
        code: String,
        state: String,
    },
    Error {
        error: String,
        description: Option<String>,
        state: String,
    },
}

/// Parse and percent-decode an OAuth redirect request.
pub fn parse_callback(request_head: &[u8]) -> Result<OAuthCallback> {
    let text = std::str::from_utf8(request_head).context("callback request is not UTF-8")?;
    let mut request = text
        .lines()
        .next()
        .context("callback request is empty")?
        .split_whitespace();
    anyhow::ensure!(request.next() == Some("GET"), "callback method must be GET");
    let target = request.next().context("callback target is missing")?;
    let url =
        url::Url::parse(&format!("http://localhost{target}")).context("invalid callback target")?;
    anyhow::ensure!(url.path() == "/auth/callback", "invalid callback path");
    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut description = None;
    for (key, value) in url.query_pairs() {
        let slot = match key.as_ref() {
            "code" => &mut code,
            "state" => &mut state,
            "error" => &mut error,
            "error_description" => &mut description,
            _ => continue,
        };
        anyhow::ensure!(slot.is_none(), "duplicate callback parameter: {key}");
        anyhow::ensure!(!value.is_empty(), "empty callback parameter: {key}");
        *slot = Some(value.into_owned());
    }
    let state = state.context("callback state is missing")?;
    anyhow::ensure!(
        code.is_some() ^ error.is_some(),
        "callback must contain exactly one of code or error"
    );
    anyhow::ensure!(
        error.is_some() || description.is_none(),
        "error_description requires error"
    );
    if let Some(error) = error {
        return Ok(OAuthCallback::Error {
            error,
            description,
            state,
        });
    }
    Ok(OAuthCallback::Code {
        code: code.expect("validated above"),
        state,
    })
}

const MAX_CALLBACK_HEAD: usize = 16 * 1024;
const CALLBACK_CONNECTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

async fn read_callback_head(
    socket: &mut tokio::net::TcpStream,
    overall_deadline: tokio::time::Instant,
) -> Result<Vec<u8>> {
    let deadline = std::cmp::min(
        overall_deadline,
        tokio::time::Instant::now() + CALLBACK_CONNECTION_TIMEOUT,
    );
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        anyhow::ensure!(
            buf.len() < MAX_CALLBACK_HEAD,
            "callback request head is too large"
        );
        let remaining = (MAX_CALLBACK_HEAD - buf.len()).min(chunk.len());
        let n = tokio::time::timeout_at(deadline, socket.read(&mut chunk[..remaining]))
            .await
            .context("timed out reading callback request")?
            .context("reading callback request")?;
        anyhow::ensure!(
            n != 0,
            "callback connection closed before request completed"
        );
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(buf);
        }
    }
}

async fn callback_code(
    listener: &tokio::net::TcpListener,
    expected_state: &str,
    timeout: std::time::Duration,
) -> Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let (mut socket, _) = tokio::time::timeout_at(deadline, listener.accept())
            .await
            .context("timed out waiting for the browser callback")?
            .context("accepting callback connection")?;
        let callback = match read_callback_head(&mut socket, deadline)
            .await
            .and_then(|head| parse_callback(&head))
        {
            Ok(callback) => callback,
            Err(error) => {
                respond(
                    &mut socket,
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    deadline,
                )
                .await;
                if tokio::time::Instant::now() >= deadline {
                    return Err(error).context("callback deadline elapsed");
                }
                continue;
            }
        };
        let returned_state = match &callback {
            OAuthCallback::Code { state, .. } | OAuthCallback::Error { state, .. } => state,
        };
        if returned_state != expected_state {
            respond(
                &mut socket,
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                deadline,
            )
            .await;
            continue;
        }
        match callback {
            OAuthCallback::Code { code, .. } => {
                respond(
                    &mut socket,
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
<html><body><h2>ilar: logged in</h2>You can close this tab.</body></html>",
                    deadline,
                )
                .await;
                return Ok(code);
            }
            OAuthCallback::Error {
                error, description, ..
            } => {
                respond(
                    &mut socket,
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    deadline,
                )
                .await;
                let description = description
                    .map(|description| format!(": {description}"))
                    .unwrap_or_default();
                anyhow::bail!("OAuth authorization failed: {error}{description}");
            }
        }
    }
}

async fn respond(
    socket: &mut tokio::net::TcpStream,
    response: &[u8],
    deadline: tokio::time::Instant,
) {
    let _ = tokio::time::timeout_at(deadline, socket.write_all(response)).await;
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
        open_in_browser(&url);
    }
    println!("Log in with your ChatGPT account:\n\n{url}\n");

    let code = callback_code(&listener, &state, timeout).await?;

    // `exchange_code` already derives `account_id` from the id_token.
    let tokens = exchange_code(&code, &verifier).await?;
    store.save(&tokens).await?;
    Ok(tokens)
}

/// Hand the URL to the platform's opener. Best effort by design: the URL
/// is printed either way, so a missing opener costs the user a copy and
/// paste rather than the login.
fn open_in_browser(url: &str) {
    let (program, leading_args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("open", &[])
    } else if cfg!(target_os = "windows") {
        // Not `cmd /C start`: cmd re-parses its command line and would
        // split the authorize URL at its first `&`. `rundll32` is an
        // ordinary program and takes the URL as one argument.
        ("rundll32", &["url.dll,FileProtocolHandler"])
    } else {
        ("xdg-open", &[])
    };
    let _ = std::process::Command::new(program)
        .args(leading_args)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stalled_refresh_times_out_and_releases_store_lock() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (first, _) = listener.accept().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            drop(first);

            let (mut second, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = second.read(&mut request).await.unwrap();
            let body = r#"{"access_token":"fresh","refresh_token":"rotated","expires_in":3600}"#;
            second
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::open(dir.path().to_path_buf());
        store
            .save(&TokenSet {
                access_token: "stale".into(),
                refresh_token: Some("refresh".into()),
                id_token: None,
                account_id: None,
            })
            .await
            .unwrap();
        let url = format!("http://{address}/token");
        let http = reqwest::Client::new();
        let error = refresh_tokens_with_timeout(
            &store,
            "stale",
            &url,
            &http,
            std::time::Duration::from_millis(50),
        )
        .await
        .expect_err("stalled refresh must time out");
        assert!(
            error.to_string().contains("token endpoint request"),
            "{error:#}"
        );

        let tokens = refresh_tokens_with_timeout(
            &store,
            "stale",
            &url,
            &http,
            std::time::Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(tokens.access_token, "fresh");
        server.await.unwrap();
    }
}
