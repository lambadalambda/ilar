//! Image generation on the OpenAI account, the way Codex does it.
//!
//! Not a Responses built-in: a function tool that posts JSON to
//! `{provider base}/images/generations` — or `/images/edits`, with the
//! reference images as data URLs — carrying the same credentials as the
//! Responses wire (bearer, `chatgpt-account-id`, `originator` on the
//! ChatGPT backend; a bare bearer on the public API), model
//! `gpt-image-2`, and decodes `data[0].b64_json`. The PNG is written
//! under the state directory and rides the tool result as an
//! attachment, so a vision model sees what it made and the transcript
//! shows the marker.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;

use crate::tools::{Tool, ToolConcurrency, ToolContext, ToolFuture, ToolOutput, WorkspaceAccess};

/// The image model Codex uses on both backends.
const IMAGE_MODEL: &str = "gpt-image-2";
/// Reference images an edit may carry (Codex's cap).
const MAX_REFERENCES: usize = 5;
/// A reference file larger than this is refused before it is read: the
/// API's own limit is 50 MB and a data URL is a third bigger again.
const MAX_REFERENCE_BYTES: u64 = 20 * 1024 * 1024;
/// 4K output at high quality takes a while; the connect stays short.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_ERROR_CHARS: usize = 2_048;
const PUBLIC_API_BASE: &str = "https://api.openai.com/v1";
const CHATGPT_BASE: &str = "https://chatgpt.com/backend-api/codex";

/// Who signs the request.
#[derive(Clone)]
enum ImageAuth {
    ApiKey(String),
    /// ChatGPT login: bearer from the token store, one refresh on 401 —
    /// the same arrangement the Responses provider makes.
    ChatGpt {
        store: crate::auth::AuthStore,
    },
}

/// The endpoint, credentials and output directory the tool works with.
#[derive(Clone)]
pub struct ImageGenBackend {
    auth: ImageAuth,
    base_url: String,
    token_url: String,
    images_dir: PathBuf,
    http: reqwest::Client,
}

impl ImageGenBackend {
    /// The public API with an API key; `base_url` overrides
    /// `https://api.openai.com/v1`.
    pub fn with_api_key(api_key: String, base_url: Option<String>, images_dir: PathBuf) -> Self {
        Self::new(
            ImageAuth::ApiKey(api_key),
            base_url.unwrap_or_else(|| PUBLIC_API_BASE.into()),
            images_dir,
        )
    }

    /// The ChatGPT backend with a login; `base_url` overrides the Codex
    /// backend the Responses provider also uses.
    pub fn with_chatgpt_auth(
        store: crate::auth::AuthStore,
        base_url: Option<String>,
        images_dir: PathBuf,
    ) -> Self {
        Self::new(
            ImageAuth::ChatGpt { store },
            base_url.unwrap_or_else(|| CHATGPT_BASE.into()),
            images_dir,
        )
    }

    fn new(auth: ImageAuth, base_url: String, images_dir: PathBuf) -> Self {
        Self {
            auth,
            base_url: base_url.trim_end_matches('/').to_string(),
            token_url: format!("{}/oauth/token", crate::auth::AUTH_BASE),
            images_dir,
            http: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("valid image client"),
        }
    }

    /// Test hook: point the token refresh at a mock server.
    pub fn with_token_url_for_test(mut self, url: impl Into<String>) -> Self {
        self.token_url = url.into();
        self
    }

    /// Where a session's images land.
    pub fn images_dir(&self) -> &Path {
        &self.images_dir
    }
}

pub struct ImageGenTool {
    backend: ImageGenBackend,
}

impl ImageGenTool {
    pub fn new(backend: ImageGenBackend) -> Self {
        Self { backend }
    }
}

#[derive(serde::Deserialize)]
struct Input {
    prompt: String,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    quality: Option<String>,
    #[serde(default)]
    reference_paths: Vec<String>,
}

/// `auto` or `WIDTHxHEIGHT`; the API judges the numbers.
fn valid_size(size: &str) -> bool {
    size == "auto"
        || size
            .split_once('x')
            .is_some_and(|(w, h)| w.parse::<u32>().is_ok() && h.parse::<u32>().is_ok())
}

impl Tool for ImageGenTool {
    fn name(&self) -> &'static str {
        "image_gen"
    }

    fn description(&self) -> &'static str {
        "Generate an image from a prompt with OpenAI's gpt-image-2 on this account, or \
         edit up to five reference images (reference_paths) — logos, mockups, diagrams, \
         illustrations. Writes a PNG under ilar's state directory, returns its path, and — \
         when this session's model accepts images — attaches the image so you can look at \
         it; the result says which happened. Each call is one image \
         and costs money: write a complete prompt (subject, style, composition, any text \
         to render verbatim) rather than iterating blindly. size: auto (default) or \
         WIDTHxHEIGHT such as 1536x1024 (landscape), 1024x1536 (portrait), 2048x2048; \
         quality: auto (default), low for drafts, high for final assets or dense text. \
         One call is given five minutes, which a 4K image at high quality can need."
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn workspace_access(&self) -> WorkspaceAccess {
        WorkspaceAccess::None
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "What to draw, completely"},
                "size": {"type": "string", "description": "auto or WIDTHxHEIGHT, e.g. 1536x1024"},
                "quality": {"type": "string", "enum": ["auto", "low", "medium", "high"]},
                "reference_paths": {
                    "type": "array",
                    "items": {"type": "string"},
                    "maxItems": MAX_REFERENCES,
                    "description": "Image files to edit or draw from (up to 5); relative to the working directory"
                }
            },
            "required": ["prompt"]
        })
    }

    fn run(&self, input: serde_json::Value, ctx: ToolContext) -> ToolFuture {
        let backend = self.backend.clone();
        Box::pin(async move {
            let input: Input = match super::parse_input(input, "image_gen") {
                Ok(input) => input,
                Err(error) => return error,
            };
            if input.prompt.trim().is_empty() {
                return ToolOutput::error("image_gen: prompt is empty; say what to draw");
            }
            let size = input.size.unwrap_or_else(|| "auto".into());
            if !valid_size(&size) {
                return ToolOutput::error(format!(
                    "image_gen: size {size:?} is not auto or WIDTHxHEIGHT (for example 1536x1024)"
                ));
            }
            let quality = input.quality.unwrap_or_else(|| "auto".into());
            if !matches!(quality.as_str(), "auto" | "low" | "medium" | "high") {
                return ToolOutput::error(format!(
                    "image_gen: quality {quality:?} is not one of auto, low, medium, high"
                ));
            }
            if input.reference_paths.len() > MAX_REFERENCES {
                return ToolOutput::error(format!(
                    "image_gen: reference_paths holds {} files; at most {MAX_REFERENCES} are sent",
                    input.reference_paths.len()
                ));
            }
            let mut references = Vec::with_capacity(input.reference_paths.len());
            for path in &input.reference_paths {
                match reference_data_url(&ctx.cwd, path) {
                    Ok(url) => references.push(url),
                    Err(error) => return ToolOutput::error(format!("image_gen: {error}")),
                }
            }

            let mut body = serde_json::json!({
                "model": IMAGE_MODEL,
                "prompt": input.prompt,
                "size": size,
                "quality": quality,
                "background": "auto",
            });
            let endpoint = if references.is_empty() {
                "images/generations"
            } else {
                body["images"] = serde_json::json!(
                    references
                        .iter()
                        .map(|url| serde_json::json!({"image_url": url}))
                        .collect::<Vec<_>>()
                );
                "images/edits"
            };
            let url = format!("{}/{endpoint}", backend.base_url);

            let png = match backend.post(&url, &body).await {
                Ok(png) => png,
                Err(error) => {
                    return ToolOutput::error(format!("image_gen: {error}"));
                }
            };
            let call = ctx.call_id.clone().unwrap_or_else(crate::session::new_id);
            let dir = backend.images_dir.join(&ctx.session_id);
            let path = dir.join(format!("{}.png", sanitize(&call)));
            if let Err(error) =
                std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, &png))
            {
                return ToolOutput::error(format!(
                    "image_gen: generated but could not be saved to {}: {error}",
                    path.display()
                ));
            }
            // Bounded and downscaled for the model where needed; the file
            // on disk keeps every pixel. A text-only session gets none of
            // it — the same test read applies before promising an image.
            let attachment = ctx
                .vision
                .then(|| crate::image::from_file_bytes(&png))
                .flatten();
            attached_output(&path, png.len(), ctx.vision, attachment)
        })
    }
}

impl ImageGenBackend {
    /// POST the request, refreshing a ChatGPT token once on 401, and
    /// return the decoded first image.
    async fn post(&self, url: &str, body: &serde_json::Value) -> Result<Vec<u8>, String> {
        let (mut token, mut account, store) = match &self.auth {
            ImageAuth::ApiKey(key) => (key.clone(), None, None),
            ImageAuth::ChatGpt { store } => {
                let tokens = store
                    .load()
                    .map_err(|error| format!("OpenAI ChatGPT auth store: {error:#}"))?
                    .ok_or_else(|| {
                        "OpenAI ChatGPT auth: not logged in — run `ilar login`".to_string()
                    })?;
                (tokens.access_token, tokens.account_id, Some(store.clone()))
            }
        };
        let mut refreshed = false;
        loop {
            let mut request = self.http.post(url).bearer_auth(&token).json(body);
            if store.is_some() {
                request = request.header("originator", "codex_cli_rs");
                if let Some(account) = &account {
                    request = request.header("chatgpt-account-id", account);
                }
            }
            let response = request
                .send()
                .await
                .map_err(|error| redact(&error.to_string(), &token))?;
            let status = response.status();
            if status == reqwest::StatusCode::UNAUTHORIZED
                && !refreshed
                && let Some(store) = &store
            {
                refreshed = true;
                let tokens =
                    crate::auth::refresh_tokens(store, &token, &self.token_url, &self.http)
                        .await
                        .map_err(|error| format!("token refresh failed: {error:#}"))?;
                token = tokens.access_token;
                if tokens.account_id.is_some() {
                    account = tokens.account_id;
                }
                continue;
            }
            let bytes = response
                .bytes()
                .await
                .map_err(|error| redact(&error.to_string(), &token))?;
            if !status.is_success() {
                let text = String::from_utf8_lossy(&bytes);
                let text: String = text.chars().take(MAX_ERROR_CHARS).collect();
                return Err(format!("HTTP {status}: {}", redact(&text, &token)));
            }
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|error| format!("unreadable image response: {error}"))?;
            let b64 = value["data"][0]["b64_json"]
                .as_str()
                .ok_or_else(|| "image response carried no image data".to_string())?;
            return base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .map_err(|error| format!("image data is not base64: {error}"));
        }
    }
}

/// A reference file as the data URL the edits endpoint takes: sniffed,
/// bounded, never a path the model could not read itself.
/// The saved image's result, promising an attachment only when the
/// result actually carries one: a text-only session takes no images, and
/// the per-result cap can drop one that is too large (it says so
/// itself). The model's only account of what it drew must not claim a
/// picture that is not there.
fn attached_output(
    path: &Path,
    png_bytes: usize,
    vision: bool,
    attachment: Option<crate::session::ImageContent>,
) -> ToolOutput {
    let mut output = ToolOutput::text(format!(
        "saved {} ({})",
        path.display(),
        crate::text::format_bytes(png_bytes as u64)
    ))
    .with_images(attachment.into_iter().collect());
    output.content.push_str(if !output.images().is_empty() {
        "\nThe image is attached to this result; the file is the full-resolution original."
    } else if vision {
        "\nThe image is not attached (see the note above). The file is the result."
    } else {
        "\nThe image is not attached: this session's model takes no images. The file is the result."
    });
    output
}

fn reference_data_url(cwd: &Path, path: &str) -> Result<String, String> {
    let resolved = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        cwd.join(path)
    };
    let size = std::fs::metadata(&resolved)
        .map_err(|error| format!("cannot read reference {path:?}: {error}"))?
        .len();
    if size > MAX_REFERENCE_BYTES {
        return Err(format!(
            "reference {path:?} is {} — larger than the {} the tool sends",
            crate::text::format_bytes(size),
            crate::text::format_bytes(MAX_REFERENCE_BYTES)
        ));
    }
    let bytes = std::fs::read(&resolved)
        .map_err(|error| format!("cannot read reference {path:?}: {error}"))?;
    let media_type = crate::image::media_type(&bytes)
        .ok_or_else(|| format!("reference {path:?} is not a PNG, JPEG, WebP or GIF"))?;
    Ok(format!(
        "data:{media_type};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    ))
}

/// A call id as a file stem: nothing a path could misread.
fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn redact(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        text.to_string()
    } else {
        text.replace(secret, "<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The result used to promise an attachment unconditionally, even
    /// in a session whose model takes no images at all.
    #[test]
    fn the_result_promises_an_attachment_only_when_it_carries_one() {
        let path = Path::new("/tmp/out.png");
        let image =
            crate::image::from_file_bytes(&crate::image::encode_png(2, 2, &[7u8; 16]).unwrap());
        assert!(image.is_some());

        let attached = attached_output(path, 1024, true, image);
        assert_eq!(attached.images().len(), 1);
        assert!(
            attached.content.contains("is attached"),
            "{}",
            attached.content
        );

        let text_only = attached_output(path, 1024, false, None);
        assert!(text_only.images().is_empty());
        assert!(
            text_only
                .content
                .contains("not attached: this session's model takes no images"),
            "{}",
            text_only.content
        );
        // Both still name the file, which is the whole result then.
        assert!(
            text_only.content.contains("/tmp/out.png"),
            "{}",
            text_only.content
        );
    }
}
