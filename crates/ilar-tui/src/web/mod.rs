//! The web surface, shared by the two things that render a session for
//! a browser.
//!
//! [`view`] projects session events to wire JSON and [`assets`] is the
//! page that renders it — a preact app with no build step and no CDN,
//! carried in the binary. `ilar serve` fetches the JSON over HTTP;
//! [`share`] inlines it into one file. One renderer, two outputs: a
//! second way to render a transcript is a second thing to keep in step
//! with the first, and the two that exist have drifted before.
//!
//! Compiled always. Only the *server* is behind the `serve` feature —
//! a share file needs no server, and refusing to build the renderer
//! without one would be the tail wagging the dog.

pub(crate) mod share;
pub(crate) mod view;

/// The page, as the binary carries it. `serve` hands these out as
/// separate routes; `share` inlines them into one file.
pub(crate) mod assets {
    /// The server's page shell. A share file builds its own, because
    /// it has no routes to point at.
    #[cfg(feature = "serve")]
    pub(crate) const INDEX: &str = include_str!("assets/index.html");
    pub(crate) const APP_CSS: &str = include_str!("assets/app.css");
    pub(crate) const APP_JS: &str = include_str!("assets/app.js");
    pub(crate) const PREACT: &str = include_str!("assets/vendor/preact.module.js");
    pub(crate) const HOOKS: &str = include_str!("assets/vendor/hooks.module.js");
    pub(crate) const HTM: &str = include_str!("assets/vendor/htm.module.js");
}
