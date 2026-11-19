// SPDX-License-Identifier: GPL-3.0-or-later
// The page around the window (docs/web_gui.md §5.3, §9): it asks for the
// seat key on every load, hands it to the window and keeps nothing - no
// storage, no cookie, no address. The window says in one word how its
// connection fares; this file turns that word into the prompt.
import init, { start, reconnect, page_text } from "./molt_web.js";

const el = (id) => document.getElementById(id);
const gate = el("gate");
const why = el("why");
const key = el("key");
const go = el("go");
const note = el("note");

// set on the one reload a version mismatch gets; never anything else
const RELOADED = "#reloaded";

// The labels come with the window's module, and the first load fetches
// all of it: until then this one line is the page's own.
const LOADING = (navigator.language || "").toLowerCase().startsWith("de")
  ? "Lade das Fenster. Beim ersten Mal kann das auf einer langsamen Leitung Minuten dauern."
  : "Loading the window. The first load can take minutes on a slow link.";

let text = {};
let started = false;
// only a reload helps: another build's node, or a window that did not start
let reloadOnly = false;
let noteTimer = 0;

function label() {
  document.documentElement.lang = text.lang;
  el("key-label").textContent = text.key;
  go.textContent = text.connect;
  el("help-title").textContent = text.help_title;
  const lines = text.help.split("\n").map((line) => {
    const item = document.createElement("li");
    item.textContent = line;
    return item;
  });
  el("help-list").replaceChildren(...lines);
}

// Show the prompt again, with the reason the connection ended.
function ask(reason) {
  why.textContent = reason;
  gate.hidden = false;
  key.disabled = reloadOnly;
  go.disabled = false;
  go.textContent = reloadOnly ? text.reload : text.connect;
  if (!reloadOnly) {
    key.focus();
  }
}

// A passing line over the window.
function flash(line) {
  note.textContent = line;
  note.hidden = false;
  clearTimeout(noteTimer);
  noteTimer = setTimeout(() => {
    note.hidden = true;
  }, 4000);
}

function heard(word) {
  switch (word) {
    case "ready":
      if (location.hash === RELOADED) {
        history.replaceState(null, "", location.pathname);
      }
      why.textContent = "";
      gate.hidden = true;
      el("canvas").focus();
      break;
    case "lag":
      flash(text.lag);
      break;
    case "version":
      // a stale bundle: one reload fetches the node's own
      if (location.hash !== RELOADED) {
        location.hash = RELOADED;
        location.reload();
        return;
      }
      reloadOnly = true;
      ask(text.version);
      break;
    case "refused":
    case "protocol":
    case "closed":
      ask(text[word]);
      break;
  }
}

el("key-form").addEventListener("submit", (event) => {
  event.preventDefault();
  if (reloadOnly) {
    location.reload();
    return;
  }
  const token = key.value;
  key.value = "";
  if (token === "") {
    return;
  }
  key.disabled = true;
  go.disabled = true;
  why.textContent = text.connecting;
  if (started) {
    if (!reconnect(token)) {
      reloadOnly = true;
      ask(text.closed);
    }
    return;
  }
  started = true;
  try {
    start(token, heard);
  } catch (error) {
    // winit hands the thread to the browser by throwing; anything else
    // is a window that did not start
    const said = String((error && error.message) || error);
    if (!said.includes("Using exceptions for control flow")) {
      reloadOnly = true;
      ask(`${text.failed}: ${said}`);
    }
  }
});

why.textContent = LOADING;
init().then(
  () => {
    text = page_text(navigator.language || "");
    label();
    why.textContent = "";
    key.disabled = false;
    go.disabled = false;
    key.focus();
  },
  (error) => {
    why.textContent = String(error);
  },
);
