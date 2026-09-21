//! A session as one self-contained HTML file.
//!
//! The same renderer `ilar serve` uses, with the data already in the
//! page instead of behind an HTTP request: styles, vendored modules,
//! `app.js` and the projected session all inline. It opens from
//! `file://` with no server, no network and no account, and it has not
//! left the machine until someone sends it.
//!
//! **Why blobs and not an import map.** `app.js` is an ES module that
//! imports the bare specifiers `preact`, `preact/hooks` and `htm`, and
//! the hooks build imports `preact` itself. An inline
//! `<script type="module">` cannot be imported by specifier, and a
//! module fetched from `file://` runs into opaque-origin rules. So the
//! page carries each module's source as inert `text/plain`, and a
//! classic bootstrap script turns them into `blob:` URLs in dependency
//! order, rewriting each bare specifier to the concrete URL of the
//! blob before making the next one. Blob URLs inherit the document's
//! origin and nothing is fetched, so the graph resolves the same way
//! everywhere. No import map, no build step, no CDN.

use ilar::session::SessionEvent;
use serde_json::{Value, json};

/// Everything a shared page needs, in the shapes `app.js` would have
/// fetched: the listing row, the session page, the children it lists,
/// and each delegation's own timeline under the two routes a task row
/// asks for. Built from the whole log, so a compacted session shares
/// in full and a rewound turn stays withdrawn.
pub(crate) fn payload(
    store: &ilar::session::SessionStore,
    session_id: &str,
) -> std::io::Result<Value> {
    let events = store.whole_events(session_id)?;
    let head = store.head(session_id)?;
    // Never driven and never running: a file has no engine behind it,
    // and a row that said otherwise would offer controls nothing can
    // answer.
    let session = super::view::session_summary(&head, false, "idle", Value::Null);
    let mut extra = serde_json::Map::new();
    cut_results(&events, &events, session_id, &mut extra);

    // A task row opens a child transcript. On the server that is two
    // routes — the child a call is writing, then that child's slice
    // for the call — and a file answers both from here. A child whose
    // log cannot be read is still listed but carries no routes, so its
    // row says so, rather than one delegation failing the whole share.
    let mut children = Vec::new();
    for child in store.children_of(session_id) {
        children.push(json!({
            "id": child.id,
            "agent": child.agent,
            "model": child.model,
            "title": child.title,
            "parent_id": session_id,
        }));
        let Ok(child_events) = store.whole_events(&child.id) else {
            continue;
        };
        let calls = child_events.iter().filter_map(|event| match event {
            SessionEvent::SubagentInvocation {
                parent_tool_call_id,
                ..
            } => Some(parent_tool_call_id.as_str()),
            _ => None,
        });
        for call in calls {
            let slice = super::view::invocation_slice(&child_events, call);
            extra.insert(
                format!(
                    "/api/sessions/{}/invocations/{}",
                    urlish(session_id),
                    urlish(call)
                ),
                json!({ "parent_tool_call_id": call, "child_session_id": child.id }),
            );
            extra.insert(
                format!(
                    "/api/sessions/{}?invocation={}",
                    urlish(&child.id),
                    urlish(call)
                ),
                page_of(&child.id, slice, Value::Null),
            );
            cut_results(&child_events, slice, &child.id, &mut extra);
        }
    }

    Ok(json!({
        "id": session_id,
        "session": session,
        "children": children,
        "extra": extra,
        "page": page_of(session_id, &events, session),
    }))
}

/// One transcript page in the shape `/api/sessions/{id}` answers —
/// the whole of `events` as a single page, since a file has nothing
/// left to scroll back to. `session` is the listing row, or null for a
/// child slice, which the server does not summarise either.
fn page_of(id: &str, events: &[SessionEvent], session: Value) -> Value {
    json!({
        "id": id,
        "session": session,
        "events": super::view::project_page(events, events, false),
        "cursor": 0,
        "has_more": false,
        "count": events.len(),
        "line": events.len(),
        "usage": super::view::usage_totals(events),
    })
}

/// The full text behind every result in `page` the projection had to
/// cut, under the route the page will ask for it by. Inputs are
/// harvested from `view`, the whole log, the way the server's own
/// route does: a resumed subagent can answer in one invocation a call
/// it made in the one before, and the redaction reads the call.
///
/// A cut result carries a "show the whole thing" affordance, and on the
/// server that is a route. A file has no route, so those texts travel
/// with it — otherwise the affordance is there and answers with an
/// error.
///
/// Only the cut ones. The rest are already in the page whole, and a
/// second copy would undo the bulk-cutting the projection exists to do
/// — on the one surface that leaves the machine.
///
/// Redacted, through the same call `serve`'s own full-text route makes:
/// the persisted body keeps raw values by design, and a route that
/// hands them back undoes what every bounded display cut. This is the
/// *only* copy that leaves the machine, so it is the last place that
/// may forget.
fn cut_results(
    view: &[SessionEvent],
    page: &[SessionEvent],
    session_id: &str,
    extra: &mut serde_json::Map<String, Value>,
) {
    let mut inputs = std::collections::HashMap::new();
    super::view::harvest_call_inputs(view, &mut inputs);
    for event in page {
        let SessionEvent::ToolResult {
            tool_use_id,
            content,
            images,
            ..
        } = event
        else {
            continue;
        };
        let input = inputs
            .get(tool_use_id.as_str())
            .cloned()
            .unwrap_or(Value::Null);
        let redacted = ilar::agent::redact_tool_result(&input, content);
        let shown =
            ilar::text::bounded_detail(&format!("{redacted}{}", ilar::image::markers(images)));
        if shown.len() >= redacted.len() {
            // Nothing was cut, so there is nothing behind the row.
            continue;
        }
        extra.insert(
            format!(
                "/api/sessions/{}/results/{}",
                urlish(session_id),
                urlish(tool_use_id)
            ),
            Value::String(redacted),
        );
    }
}

/// The bare specifiers the page's modules import, each with the source
/// that satisfies it, in **dependency order**: a module is turned into
/// a blob URL only once everything it imports already has one, because
/// its imports are rewritten to those URLs on the way in. `hooks`
/// imports `preact`, so `preact` comes first.
///
/// This is not the order the *replacement* wants — there, a specifier
/// that is a prefix of another has to go first, or rewriting
/// `"preact"` would turn `"preact/hooks"` into a blob URL with
/// `/hooks` glued on the end. The bootstrap sorts for that separately.
/// Getting these two confused is exactly what a browser caught: with
/// the list in replacement order, `hooks` was blobbed before `preact`
/// had a URL and its bare import survived into the page.
fn modules() -> Vec<(&'static str, &'static str)> {
    vec![
        ("preact", super::assets::PREACT),
        ("preact/hooks", super::assets::HOOKS),
        ("htm", super::assets::HTM),
    ]
}

/// `encodeURIComponent`, for the paths the page will ask for. Session
/// and tool-call ids are uuids and tool names in practice, but the key
/// has to match what the page builds byte for byte, so it is encoded
/// the same way rather than assumed safe.
fn urlish(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The page, with `session` the payload `app.js` would otherwise have
/// fetched from `/api/sessions/{id}`.
pub(crate) fn page(title: &str, session: &Value) -> String {
    let mut sources = String::new();
    let mut order = Vec::new();
    for (index, (specifier, source)) in modules().into_iter().enumerate() {
        let id = format!("ilar-module-{index}");
        sources.push_str(&inert_script(&id, source));
        order.push(json!({ "id": id, "specifier": specifier }));
    }
    let app_id = "ilar-app";
    sources.push_str(&inert_script(app_id, super::assets::APP_JS));

    format!(
        "<!doctype html>\n\
         <!-- An ilar session, whole and offline. Every byte this page \n\
              needs is below: nothing is fetched, and there is no link \n\
              to anywhere. -->\n\
         <html lang=\"en\">\n\
         <head>\n\
         <meta charset=\"utf-8\" />\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\n\
         <meta name=\"referrer\" content=\"no-referrer\" />\n\
         <title>{title}</title>\n\
         <style>\n{css}\n</style>\n\
         </head>\n\
         <body>\n\
         <div id=\"app\"></div>\n\
         <script type=\"application/json\" id=\"ilar-session\">\n{session}\n</script>\n\
         {sources}\
         <script type=\"application/json\" id=\"ilar-modules\">\n{order}\n</script>\n\
         <script>\n{bootstrap}\n</script>\n\
         </body>\n\
         </html>\n",
        title = escape_text(title),
        css = super::assets::APP_CSS,
        session = escape_script_json(session),
        order = escape_script_json(&Value::Array(order)),
        sources = sources,
        bootstrap = BOOTSTRAP,
    )
}

/// One module's source, parked where no parser will run it. The type
/// is deliberately not `module`: these are raw material for the
/// bootstrap, not scripts of the page.
fn inert_script(id: &str, source: &str) -> String {
    // Verbatim: the bootstrap hands this text to the module loader, so
    // an escape here would be a syntax error there. That is safe only
    // because these are our own vendored assets — see
    // `the_inlined_assets_cannot_close_their_own_block`, which is the
    // guard, and which fails if anything vendored later could.
    debug_assert!(!closes_a_script(source), "asset {id} could close its block");
    format!("<script type=\"text/plain\" id=\"{id}\">\n{source}\n</script>\n")
}

/// Whether a source could end the `<script>` element holding it.
/// Case-insensitive, like the tokenizer; `<!--` too, because it opens
/// the escaped state where the rules change.
fn closes_a_script(source: &str) -> bool {
    let lower = source.to_ascii_lowercase();
    lower.contains("</script") || lower.contains("<!--")
}

/// JSON for a `<script>` block.
///
/// serde escapes what *JSON* needs. This escapes what *HTML* needs,
/// which is exactly `<`. The tokenizer ends a script element at
/// `</script` matched ASCII-case-insensitively, so a transcript
/// containing `</ScRiPt>` would close the block and everything after
/// it would become live markup in the reader's browser — verified in
/// two browsers before it was fixed. A case-sensitive replace of
/// `</script` does not stop it, and escaping `<!--` as `<\!--`
/// produces JSON that `JSON.parse` rejects outright, blanking the
/// page for any conversation that mentions an HTML comment.
///
/// `\u003C` is none of those things: valid JSON, parses back to `<`,
/// and cannot begin a tag. JSON has no `<` of its own, so replacing
/// every one is safe.
fn escape_script_json(value: &Value) -> String {
    value.to_string().replace('<', "\\u003C")
}

/// Text going into an HTML element rather than a script.
fn escape_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Turn the inert sources into a live module graph, then start the app.
///
/// A classic script, so it runs before any module does and can build
/// the URLs the modules will import each other by.
const BOOTSTRAP: &str = r#"
(function () {
  var read = function (id) { return document.getElementById(id).textContent; };
  var blob = function (src) {
    return URL.createObjectURL(new Blob([src], { type: "text/javascript" }));
  };
  // Each module's bare specifiers are rewritten to the concrete blob
  // URL of the module that satisfies them, before that module is
  // itself turned into a blob. The list is ordered so a dependency is
  // always built before whatever imports it.
  var resolved = [];
  var rewrite = function (src) {
    // Longest specifier first, whatever order they were built in: a
    // shorter one that prefixes a longer would otherwise rewrite half
    // of it. Dependency order is the list's business, not this pass's.
    var by_length = resolved.slice().sort(function (a, b) {
      return b.specifier.length - a.specifier.length;
    });
    for (var i = 0; i < by_length.length; i += 1) {
      src = src.split('"' + by_length[i].specifier + '"').join('"' + by_length[i].url + '"');
      src = src.split("'" + by_length[i].specifier + "'").join('"' + by_length[i].url + '"');
    }
    return src;
  };
  var modules = JSON.parse(read("ilar-modules"));
  for (var i = 0; i < modules.length; i += 1) {
    resolved.push({
      specifier: modules[i].specifier,
      url: blob(rewrite(read(modules[i].id))),
    });
  }
  // Inside the guard: a session that would not parse used to leave a
  // blank page with the reason only in a console nobody opens.
  var fail = function (error) {
    document.getElementById("app").textContent =
      "this transcript could not be rendered: " + error;
  };
  try {
    window.__ILAR_SHARE__ = JSON.parse(read("ilar-session"));
  } catch (error) {
    fail(error);
    return;
  }
  import(blob(rewrite(read("ilar-app")))).catch(fail);
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> String {
        page(
            "fix the parser",
            &json!({ "id": "abc", "events": [{ "type": "user_message", "text": "hello" }] }),
        )
    }

    /// The page's own markup, with every script and style body taken
    /// out. What is left is what an HTML parser acts on — the only
    /// place a reference can make the browser fetch something.
    ///
    /// The bodies have to go or the check is meaningless: `app.js`
    /// contains the string `src=` in the branch it does *not* take
    /// when shared, the vendored modules carry their provenance as
    /// `https://unpkg.com/…` comments, and preact carries the XML
    /// namespace constants it hands to `createElementNS`. None of
    /// those is a request; all of them would trip a naive grep.
    fn markup_only(html: &str) -> String {
        let mut out = String::new();
        let mut rest = html;
        loop {
            let Some((at, tag)) = ["<script", "<style"]
                .into_iter()
                .filter_map(|tag| rest.find(tag).map(|at| (at, tag)))
                .min_by_key(|(at, _)| *at)
            else {
                out.push_str(rest);
                return out;
            };
            out.push_str(&rest[..at]);
            let after = &rest[at..];
            let Some(gt) = after.find('>') else {
                out.push_str(after);
                return out;
            };
            // The opening tag stays, attributes and all: that is where
            // a `<script src=…>` would be.
            out.push_str(&after[..=gt]);
            let close = if tag == "<script" {
                "</script>"
            } else {
                "</style>"
            };
            let body = &after[gt + 1..];
            rest = match body.find(close) {
                Some(end) => &body[end + close.len()..],
                None => "",
            };
        }
    }

    /// The keys the page looks its own routes up by are built with
    /// `encodeURIComponent`, so the writer has to agree with it byte
    /// for byte or a truncated result answers "this shared transcript
    /// does not carry …". These cases were taken from the browser.
    #[test]
    fn route_keys_are_encoded_the_way_the_page_encodes_them() {
        for (raw, encoded) in [
            ("abc-123", "abc-123"),
            ("read-1", "read-1"),
            (
                "550e8400-e29b-41d4-a716-446655440000",
                "550e8400-e29b-41d4-a716-446655440000",
            ),
            ("a b/c?d&e", "a%20b%2Fc%3Fd%26e"),
            ("tool.name~x", "tool.name~x"),
        ] {
            assert_eq!(super::urlish(raw), encoded, "{raw:?}");
        }
    }

    /// The whole point: a file that needs nothing. One fetchable
    /// reference would make it a page that works on the machine that
    /// wrote it and breaks everywhere else.
    #[test]
    fn the_page_reaches_for_nothing() {
        let markup = markup_only(&sample());
        for reach in [
            "<link",
            "src=",
            "href=",
            "@import",
            "importmap",
            "<iframe",
            "<img",
        ] {
            assert!(
                !markup.contains(reach),
                "the page must not carry {reach:?}: {markup}"
            );
        }
        // And the renderer and its dependencies all travelled.
        let html = sample();
        assert!(html.contains("preact/hooks"), "the specifier map is there");
        assert!(html.contains("<style>"), "styles are inline");
        assert!(html.contains("__ILAR_SHARE__"), "so is the session");
    }

    /// The stripper has to actually strip, or the test above passes by
    /// looking at nothing.
    #[test]
    fn markup_only_keeps_tags_and_drops_bodies() {
        let stripped = markup_only("<p>hi</p><script>var src=\"x\";</script><style>a{}</style><b>");
        assert_eq!(stripped, "<p>hi</p><script><style><b>");
        // An opening tag's attributes survive: that is where a
        // `<script src=…>` would hide.
        assert!(markup_only("<script src=\"/app.js\"></script>").contains("src="));
    }

    /// Dependency order, checked against what the sources actually
    /// import. A module blobbed before something it imports has a URL
    /// keeps its bare specifier, and the page dies on load with "the
    /// specifier 'preact' was not remapped to anything" — which is how
    /// this was found, in a browser, with the list in the other order.
    #[test]
    fn modules_come_after_what_they_import() {
        let all = modules();
        for (index, (specifier, source)) in all.iter().enumerate() {
            for (other, _) in &all[index + 1..] {
                assert!(
                    !source.contains(&format!("\"{other}\"")),
                    "{specifier:?} imports {other:?}, so {other:?} must come first"
                );
            }
        }
        // And the renderer itself comes last of all: it imports every
        // one of them.
        for (specifier, _) in &all {
            assert!(
                super::super::assets::APP_JS.contains(&format!("\"{specifier}\"")),
                "the page imports {specifier:?}"
            );
        }
    }

    /// Session text is model- and user-authored, it lands inside a
    /// `<script>`, and the file is meant to be sent to other people.
    /// Nothing in it may close that block.
    ///
    /// The tokenizer matches `</script` case-insensitively, which a
    /// case-sensitive replace does not — `</ScRiPt>` executed in both
    /// browsers it was tried in before this was fixed.
    #[test]
    fn a_transcript_cannot_close_its_own_script_tag() {
        for payload in [
            "</script><img src=x onerror=alert(1)>",
            "</ScRiPt><img src=x onerror=alert(1)>",
            "</SCRIPT >",
            "here is a comment: <!-- TODO -->",
        ] {
            let html = page("x", &json!({ "events": [{ "text": payload }] }));
            let body = html
                .split("id=\"ilar-session\">")
                .nth(1)
                .expect("the session block");
            let body = body.split("</script>").next().expect("its end");
            assert!(
                !body.to_ascii_lowercase().contains("</script"),
                "{payload:?} survived into the block: {body}"
            );
            assert!(!body.contains("<!--"), "{payload:?}: {body}");
            assert!(body.contains("\\u003C"), "{payload:?} was escaped: {body}");
        }
    }

    /// And the escape has to be JSON the page can read back. `<\!--`
    /// was not: `JSON.parse` rejects it, and every transcript that
    /// mentioned an HTML comment rendered a blank page.
    #[test]
    fn the_escaped_session_is_still_json_that_parses_to_the_original() {
        let original = json!({
            "events": [
                { "text": "</ScRiPt> and <!-- a comment --> and 1 < 2" },
                { "text": "a </script > b" },
            ]
        });
        let escaped = escape_script_json(&original);
        assert!(
            !escaped.to_ascii_lowercase().contains("</script"),
            "{escaped}"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&escaped).expect("valid JSON"),
            original,
            "and it reads back byte for byte"
        );
    }

    /// The vendored sources go in verbatim, because the module loader
    /// parses exactly what is written. That is only safe while none of
    /// them can end the element holding it — which is this test's job,
    /// for whatever is vendored next.
    #[test]
    fn the_inlined_assets_cannot_close_their_own_block() {
        for (name, source) in [
            ("app.js", super::super::assets::APP_JS),
            ("preact", super::super::assets::PREACT),
            ("hooks", super::super::assets::HOOKS),
            ("htm", super::super::assets::HTM),
        ] {
            assert!(!closes_a_script(source), "{name} could close its block");
        }
    }

    /// The title is an element's text, not a script's.
    #[test]
    fn a_title_cannot_carry_markup() {
        let html = page("<img src=x onerror=alert(1)>", &json!({}));
        assert!(html.contains("&lt;img src=x"), "{html}");
        assert!(!html.contains("<title><img"), "{html}");
    }
}

/// A session with one delegation, seeded into `store`: the parent asks
/// a question, reads a file, hands the lexer to a subagent, and
/// answers. The child's transcript holds the words the parent's does
/// not. Returns `(parent, child)`.
///
/// Shared by the tests and the browser fixture, so what a test asserts
/// about the payload is what a browser is then pointed at.
#[cfg(test)]
pub(crate) fn seed(store: &ilar::session::SessionStore) -> std::io::Result<(String, String)> {
    use ilar::session::{ContentBlock, SessionEvent, SessionMeta, Usage, new_id};

    let meta = |session_id: &str, parent_id: Option<&str>, agent: &str| SessionMeta {
        session_id: session_id.into(),
        parent_id: parent_id.map(str::to_string),
        agent: agent.into(),
        model: "zai/glm-4.7".into(),
        workspace: None,
        cwd: Some(std::path::PathBuf::from("/tmp/alpha")),
    };
    let text = |text: &str| ContentBlock::Text { text: text.into() };
    let assistant =
        |content: Vec<ContentBlock>, stop_reason: &str| SessionEvent::AssistantMessage {
            id: new_id(),
            model: "zai/glm-4.7".into(),
            content,
            usage: Usage::default(),
            stop_reason: stop_reason.into(),
            ts: chrono::Utc::now(),
        };
    let result = |tool_use_id: &str, content: &str, child: Option<&str>| SessionEvent::ToolResult {
        id: new_id(),
        tool_use_id: tool_use_id.into(),
        content: content.into(),
        is_error: false,
        images: Vec::new(),
        child_session_id: child.map(str::to_string),
        state: None,
        ts: chrono::Utc::now(),
    };
    let user = |text: &str| SessionEvent::UserMessage {
        id: new_id(),
        text: text.into(),
        images: Vec::new(),
        ts: chrono::Utc::now(),
    };

    let parent = new_id();
    let child = new_id();
    let mut session = store.create(meta(&parent, None, "build"))?;
    session.append(user(
        "why does the lexer loop forever on `--`?\n\nnote: </ScRiPt><img src=x onerror=alert(1)> and <!-- a comment -->",
    ))?;
    session.append(assistant(
        vec![
            text("Let me read the lexer.\n\nIt looks like `peek` never advances."),
            ContentBlock::ToolCall {
                id: "read-1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "src/lex.rs"}),
                item_id: None,
            },
        ],
        "tool_use",
    ))?;
    session.append(result(
        "read-1",
        "fn peek(&self) -> char { self.src[self.at] }",
        None,
    ))?;
    session.append(assistant(
        vec![
            text("I'll have a subagent confirm where `peek` is called."),
            ContentBlock::ToolCall {
                id: "task-1".into(),
                name: "task".into(),
                input: serde_json::json!({
                    "agent": "explore",
                    "prompt": "find every caller of peek in src/lex.rs",
                }),
                item_id: None,
            },
        ],
        "tool_use",
    ))?;
    session.append(result(
        "task-1",
        "peek is called from advance_while only, which never moves at",
        Some(&child),
    ))?;
    session.append(assistant(
        vec![text(
            "`peek` reads without advancing, so `--` never terminates.",
        )],
        "end_turn",
    ))?;
    drop(session);

    let mut delegate = store.create(meta(&child, Some(&parent), "explore"))?;
    delegate.append(SessionEvent::SubagentInvocation {
        id: new_id(),
        parent_tool_call_id: "task-1".into(),
        ts: chrono::Utc::now(),
    })?;
    delegate.append(user("find every caller of peek in src/lex.rs"))?;
    delegate.append(assistant(
        vec![
            text("Searching."),
            ContentBlock::ToolCall {
                id: "grep-1".into(),
                name: "grep".into(),
                input: serde_json::json!({"pattern": "peek", "path": "src/lex.rs"}),
                item_id: None,
            },
        ],
        "tool_use",
    ))?;
    // Long enough that the projection cuts it: the full text is what a
    // shared child row has to carry along.
    let matches = (0..2_000)
        .map(|line| format!("src/lex.rs:{line}: self.peek()\n"))
        .collect::<String>();
    delegate.append(result("grep-1", &matches, None))?;
    delegate.append(assistant(
        vec![text(
            "Only advance_while calls peek, and it never moves `at` — the delegate's finding.",
        )],
        "end_turn",
    ))?;
    drop(delegate);
    Ok((parent, child))
}

/// Write a share file from a seeded session, for a human or a browser
/// to look at. Not a test: a fixture the verification uses.
#[cfg(test)]
pub(crate) fn fixture(path: &std::path::Path) -> std::io::Result<()> {
    let dir = path.parent().unwrap().join("store");
    let store = ilar::session::SessionStore::new(dir);
    let (parent, _) = seed(&store)?;
    let payload = payload(&store, &parent)?;
    std::fs::write(path, page("why does the lexer loop", &payload))
}

#[cfg(test)]
mod payload_tests {
    use super::*;

    fn seeded() -> (
        tempfile::TempDir,
        ilar::session::SessionStore,
        String,
        String,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = ilar::session::SessionStore::new(dir.path().to_path_buf());
        let (parent, child) = seed(&store).unwrap();
        (dir, store, parent, child)
    }

    /// The issue's own acceptance criterion: a delegation opens in a
    /// shared file. The page asks for the child by the call, then for
    /// the child's slice for that call; both answers are in the file,
    /// and the slice holds the child's words.
    #[test]
    fn a_delegation_travels_with_the_share() {
        let (_dir, store, parent, child) = seeded();
        let payload = payload(&store, &parent).unwrap();
        let extra = payload["extra"].as_object().unwrap();

        let found = &extra[&format!("/api/sessions/{parent}/invocations/task-1")];
        assert_eq!(found["child_session_id"], child, "{found}");

        let slice = &extra[&format!("/api/sessions/{child}?invocation=task-1")];
        let words = slice.to_string();
        assert!(
            words.contains("the delegate's finding"),
            "the child's reply is in its slice: {words}"
        );
        assert!(
            !words.contains("why does the lexer loop"),
            "and the parent's words are not: {words}"
        );
        assert_eq!(slice["has_more"], false, "one page, whole");
        assert_eq!(slice["cursor"], 0);

        // And it all reaches the written file.
        let html = page("t", &payload);
        assert!(html.contains("the delegate's finding"), "{}", html.len());
    }

    /// A child's results are cut and carried like the parent's: the
    /// full text behind a truncated row sits under the child's own
    /// results route, redacted through the same call.
    #[test]
    fn a_childs_cut_result_travels_under_its_own_route() {
        let (_dir, store, parent, child) = seeded();
        let payload = payload(&store, &parent).unwrap();
        let extra = payload["extra"].as_object().unwrap();
        let full = extra[&format!("/api/sessions/{child}/results/grep-1")]
            .as_str()
            .expect("the full text");
        assert!(full.contains("src/lex.rs:1999:"), "whole, not the cut copy");
        // The parent's own cut results are still where they were — and
        // its short one is not carried twice.
        assert!(!extra.contains_key(&format!("/api/sessions/{parent}/results/read-1")));
    }

    /// The listing the sidebar reads still names the child.
    #[test]
    fn the_children_listing_names_the_delegate() {
        let (_dir, store, parent, child) = seeded();
        let payload = payload(&store, &parent).unwrap();
        let children = payload["children"].as_array().unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0]["id"], child);
        assert_eq!(children[0]["agent"], "explore");
    }
}

#[cfg(test)]
mod fixture_tests {
    /// Writes a share file to `target/share-fixture.html` so a browser
    /// can be pointed at it. The assertions are the cheap ones; the
    /// rendering is verified by opening it.
    #[test]
    fn a_fixture_is_written_for_a_browser_to_check() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join("share-fixture");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.html");
        super::fixture(&path).unwrap();
        let html = std::fs::read_to_string(&path).unwrap();
        assert!(
            html.contains("why does the lexer loop"),
            "the question is in it"
        );
        assert!(html.contains("src/lex.rs"), "so is the tool call");
        assert!(
            html.contains("the delegate"),
            "and the child's timeline travelled"
        );
        assert!(
            html.len() > 100_000,
            "the renderer travelled: {}",
            html.len()
        );
        eprintln!("share fixture: {}", path.display());
    }
}
