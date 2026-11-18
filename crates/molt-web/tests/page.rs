// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! The hand-written page around the window (`page/`), read as text: what
//! the door's content policy and the key's handling demand of it
//! (`docs/web_gui.md` §5.3). No browser runs here - these are the rules a
//! later edit must not break, not a test that the page works.

use molt_web::rules;

const HTML: &str = include_str!("../page/index.html");
const JS: &str = include_str!("../page/page.js");
const CSS: &str = include_str!("../page/page.css");

/// The JS without its comments (line comments only; the file has no
/// block comments, which this also pins).
fn code(js: &str) -> String {
    assert!(!js.contains("/*"), "line comments only: the scans below read code");
    js.lines().map(|line| line.split("//").next().unwrap_or_default()).collect::<Vec<_>>().join("\n")
}

/// The door's policy is `default-src 'self'` with no `unsafe-inline`:
/// an inline script, style or handler would be dropped without a word.
#[test]
fn the_page_carries_nothing_inline() {
    let lower = HTML.to_lowercase();
    for script in lower.split("<script").skip(1) {
        let tag = script.split('>').next().unwrap_or_default();
        assert!(tag.contains("src="), "an inline script: <script{tag}>");
        let body = script.split_once('>').map_or("", |(_, rest)| rest).split("</script>").next().unwrap_or_default();
        assert!(body.trim().is_empty(), "a script with a body");
    }
    assert!(!lower.contains("<style"), "an inline style sheet");
    assert!(!lower.contains("style="), "an inline style attribute");
    let handlers: Vec<&str> =
        lower.split_whitespace().filter(|word| word.starts_with("on") && word.contains('=')).collect();
    assert!(handlers.is_empty(), "inline handlers: {handlers:?}");
    assert!(!lower.contains("javascript:"));
}

/// Everything the page loads is a file the door serves beside it, by a
/// bare relative name: no other host, no path.
#[test]
fn the_page_loads_only_its_own_files() {
    let mut loaded = Vec::new();
    for attr in ["src=\"", "href=\""] {
        for rest in HTML.split(attr).skip(1) {
            loaded.push(rest.split('"').next().unwrap_or_default());
        }
    }
    loaded.sort_unstable();
    assert_eq!(loaded, ["page.css", "page.js"]);
    let js = code(JS);
    let imports: Vec<&str> =
        js.split(" from \"").skip(1).map(|rest| rest.split('"').next().unwrap_or_default()).collect();
    assert_eq!(imports, ["./molt_web.js"]);
    for elsewhere in ["http://", "https://", "fetch(", "XMLHttpRequest", "import(", "eval(", "innerHTML"] {
        assert!(!js.contains(elsewhere), "page.js names {elsewhere}");
        assert!(!CSS.contains(elsewhere), "page.css names {elsewhere}");
    }
}

/// The seat key is typed on every load and kept nowhere: a password
/// field no browser autofills, cleared the moment it is read, and no
/// storage, cookie or address the page could put it in.
#[test]
fn the_key_is_asked_and_never_kept() {
    let input = HTML.split("<input").nth(1).and_then(|rest| rest.split('>').next()).expect("the key field");
    assert!(input.contains("id=\"key\""), "{input}");
    assert!(input.contains("type=\"password\""), "{input}");
    assert!(input.contains("autocomplete=\"off\""), "{input}");
    assert_eq!(HTML.matches("<input").count(), 1, "one field: the key");
    assert!(!HTML.contains("action="), "the form posts nowhere");

    let js = code(JS);
    for store in ["localStorage", "sessionStorage", "indexedDB", "cookie", "caches", ".search", "pushState", "postMessage", "console."] {
        assert!(!js.contains(store), "page.js names {store}");
    }
    // the field is emptied right after its one read
    let read = js.find("key.value;").expect("the key is read once");
    assert_eq!(js.matches("key.value;").count(), 1);
    let cleared = js.find("key.value = \"\";").expect("the field is cleared");
    assert!(cleared > read && js[read..cleared].lines().count() <= 2, "cleared on the line after the read");
    // the address carries one fixed marker and nothing else
    for write in js.split("location.hash = ").skip(1) {
        assert!(write.starts_with("RELOADED;"), "the address is written with: {}", write.lines().next().unwrap_or_default());
    }
    assert!(js.contains("const RELOADED = \"#reloaded\";"));
    assert!(!js.contains("location.href") && !js.contains("location.assign") && !js.contains("location.replace"));
}

/// Slint finds its canvas by id and sizes it itself; a rule that sets
/// the canvas's width or height fights that (`docs/web_gui.md` §8.4).
#[test]
fn the_canvas_is_slint_s_to_size() {
    assert_eq!(HTML.matches("<canvas").count(), 1);
    let canvas = HTML.split("<canvas").nth(1).and_then(|rest| rest.split('>').next()).expect("a canvas");
    assert!(canvas.contains("id=\"canvas\""), "{canvas}");
    assert!(!canvas.contains("width") && !canvas.contains("height") && !canvas.contains("hidden"), "{canvas}");
    for rule in CSS.split('}') {
        let Some((selector, body)) = rule.split_once('{') else { continue };
        if selector.contains("canvas") {
            for sized in ["width", "height", "inset", "flex", "transform", "zoom"] {
                assert!(!body.contains(sized), "the canvas rule sets {sized}: {body}");
            }
        }
    }
}

/// The page answers every word the window may tell it, and the file
/// dialogs' overlay is styled here: `rfd` brings an inline style sheet,
/// which the door's policy drops.
#[test]
fn the_page_knows_every_word_and_styles_the_file_overlay() {
    let js = code(JS);
    for word in [rules::READY, rules::VERSION, rules::REFUSED, rules::PROTOCOL, rules::CLOSED, rules::LAG] {
        assert!(js.contains(&format!("case \"{word}\":")), "page.js has no case for {word:?}");
    }
    for id in ["#rfd-overlay", "#rfd-card", ".rfd-button"] {
        assert!(CSS.contains(id), "page.css does not style {id}");
    }
    assert!(js.contains("Using exceptions for control flow"), "winit leaves `start` by throwing");
}

/// No em dash in what a human reads.
#[test]
fn no_em_dash_on_the_page() {
    for (name, text) in [("index.html", HTML), ("page.js", JS), ("page.css", CSS)] {
        assert!(!text.contains('\u{2014}') && !text.contains("&mdash;"), "{name}");
    }
}
