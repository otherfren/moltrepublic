// SPDX-License-Identifier: GPL-3.0-or-later
//! The window against a REMOTE engine (`docs/web_gui.md` W2): a real web
//! door on loopback, the shipped client (`molt-ui-kit`) over a tokio
//! WebSocket, and the same mirror the native window runs. What the
//! browser build does, minus the browser.

use futures_util::{SinkExt, StreamExt};
use molt_ui_kit::client::{remote, Wire};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use super::*;
use crate::engine::Engine;

const SEAT: &str = "seat-key";

/// A founded republic with chat in it, behind a web door.
fn node(rt: &tokio::runtime::Runtime, root: &std::path::Path) -> (WalletHandle, u16) {
    let (w, _) = node_with_chat(root);
    let port = door(rt, &w, root, molt_core::wire::Caps::default());
    (w, port)
}

/// Found a republic on `w` and open a web door onto it.
fn door(rt: &tokio::runtime::Runtime, w: &WalletHandle, root: &std::path::Path, caps: molt_core::wire::Caps) -> u16 {
    rt.block_on(async {
        w.execute(Command::CreateStart {
            name: "DevTest".to_string(),
            member: "walter".to_string(),
            threshold: 1,
            members: 1,
            relays: Vec::new(),
        })
        .await
        .ok();
        for body in ["one", "two"] {
            w.execute(Command::Chat { body: body.to_string(), quote: None, channel: ChannelRef::Group }).await.ok();
        }
        open_door(w, root, caps).await
    })
}

/// A web door onto `w` as it is; the bundle directory goes under `root`.
async fn open_door(w: &WalletHandle, root: &std::path::Path, caps: molt_core::wire::Caps) -> u16 {
    let web = root.join("web");
    std::fs::create_dir_all(&web).expect("web dir");
    for f in ["index.html", "molt_web.js", "molt_web_bg.wasm"] {
        std::fs::write(web.join(f), "x").expect("bundle file");
    }
    w.execute(Command::PatchSettings { patch: serde_json::json!({ "mcp_token": SEAT }) }).await.expect("a seat key");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let door = molt_remote::WebDoor { build: "test".into(), caps, dir: web, pre_auth: std::time::Duration::from_secs(10) };
    tokio::spawn(molt_remote::serve(listener, w.clone(), door));
    port
}

/// Connect the shipped client to the door: the engine handle a browser
/// window would hold.
async fn connect(port: u16) -> Engine {
    let host = format!("127.0.0.1:{port}");
    let mut req = format!("ws://{host}/ws").into_client_request().expect("request");
    req.headers_mut().insert("origin", format!("http://{host}").parse().expect("origin"));
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
    let (ws, _) = tokio_tungstenite::client_async(req, stream).await.expect("upgrade");
    let (sink, stream) = ws.split();
    let incoming = Box::pin(stream.filter_map(|m| async move {
        match m {
            Ok(Message::Text(t)) => Some(Wire::Text(t.as_str().to_string())),
            Ok(Message::Binary(b)) => Some(Wire::Binary(b.to_vec())),
            _ => None,
        }
    }));
    let outgoing = Box::pin(sink.with(|w: Wire| async move {
        Ok::<_, tokio_tungstenite::tungstenite::Error>(match w {
            Wire::Text(t) => Message::text(t),
            Wire::Binary(b) => Message::binary(b),
        })
    }));
    let (handle, pump) = remote();
    let (welcomed, welcome) = futures_channel::oneshot::channel();
    tokio::spawn(async move { pump.run(SEAT, incoming, outgoing, welcomed).await });
    assert_eq!(welcome.await.expect("welcomed").build, "test");
    Engine::from(handle)
}

type Mirrored = (AppWindow, Arc<Mutex<Option<SessionSettings>>>, Arc<Mutex<ChatUiState>>);

fn window() -> Mirrored {
    (AppWindow::new().expect("headless window"), Arc::new(Mutex::new(None)), Arc::new(Mutex::new(ChatUiState::default())))
}

/// **The keystone.** A window mirroring through the web door shows what a
/// window on the engine's own handle shows.
#[test]
fn a_window_behind_the_web_door_mirrors_like_a_native_one() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let (w, port) = node(&rt, tmp.path());

    let (native, n_last, n_chat) = window();
    rt.block_on(mirror(&w, &native, &n_last, &n_chat));
    let (web, w_last, w_chat) = window();
    let engine = rt.block_on(connect(port));
    rt.block_on(mirror_engine(&engine, &web, &w_last, &w_chat));

    assert!(chat_rows(&native) >= 2, "the fixture has chat: {}", chat_rows(&native));
    assert_eq!(chat_rows(&web), chat_rows(&native));
    assert_eq!(web.get_surfaces().row_count(), native.get_surfaces().row_count());
    assert_eq!(web.get_screen(), native.get_screen());
    assert_eq!(web.get_lang_index(), native.get_lang_index());
}

/// **§6.6.** A window behind the door has no process inputs: the
/// embedded-Tor row follows the server's welcome and the settings footer
/// the engine's own config path.
#[test]
fn a_window_behind_the_web_door_takes_its_startup_inputs_from_the_server() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let session = SessionView {
        config_path: "/srv/molt/config.toml".to_string(),
        settings: SessionSettings { workspace_dir: tmp.path().display().to_string(), ..SessionSettings::default() },
        ..SessionView::default()
    };
    let w = molt_engine::spawn_with_storage(GroupConfig::demo(), session);
    let port = door(&rt, &w, tmp.path(), molt_core::wire::Caps { embedded_tor: true });

    // the mirror's session landing, called directly: the headless backend
    // never drains `invoke_from_event_loop` (see `mirror`)
    let land = |engine: &Engine, ui: &AppWindow, chat: &Arc<Mutex<ChatUiState>>| {
        let Ok(Reply::Session(sv)) = rt.block_on(engine.execute(Command::ReadSession)) else { panic!("a session") };
        crate::mirror::land_session(ui, engine, &sv, true, None, chat);
    };
    let (web, _, chat) = window();
    assert_eq!(web.get_tor_mode_enabled().iter().collect::<Vec<_>>(), [true, false, true], "the window's default");
    let engine = rt.block_on(connect(port));
    land(&engine, &web, &chat);

    assert_eq!(web.get_tor_mode_enabled().iter().collect::<Vec<_>>(), [true, true, true]);
    assert_eq!((web.get_config_dir().as_str(), web.get_config_file().as_str()), ("/srv/molt/", "config.toml"));
    assert!(!web.get_folder_browse(), "its paths are the server's: no folder dialog");

    // the native window keeps what `run_app` was handed
    let (native, _, n_chat) = window();
    native.set_config_dir("/elsewhere/".into());
    land(&Engine::from(w.clone()), &native, &n_chat);
    assert_eq!(native.get_tor_mode_enabled().iter().collect::<Vec<_>>(), [true, false, true]);
    assert_eq!(native.get_config_dir().as_str(), "/elsewhere/");
    assert!(native.get_folder_browse(), "a native window keeps its folder dialogs");
}

/// The footer split reads the SERVER's path: either separator, the
/// directory keeping its own.
#[test]
fn a_config_path_splits_at_its_last_separator() {
    let split = |p| crate::app::config_location(p);
    assert_eq!(split("/srv/molt/config.toml"), ("/srv/molt/".to_string(), "config.toml".to_string()));
    assert_eq!(split("C:\\molt\\config.toml"), ("C:\\molt\\".to_string(), "config.toml".to_string()));
    assert_eq!(split("config.toml"), (String::new(), "config.toml".to_string()));
    assert_eq!(split(""), (String::new(), String::new()));
}

/// The native footer splits its OWN path as this target does: on Unix a
/// backslash belongs to the file name.
#[cfg(unix)]
#[test]
fn a_local_config_path_splits_as_this_target_does() {
    let split = |p: &str| crate::app::local_config_location(std::path::Path::new(p));
    assert_eq!(split("/home/u/my\\cfg.toml"), ("/home/u/".to_string(), "my\\cfg.toml".to_string()));
    assert_eq!(split("/srv/molt/config.toml"), ("/srv/molt/".to_string(), "config.toml".to_string()));
    assert_eq!(split("/config.toml"), ("/".to_string(), "config.toml".to_string()));
    assert_eq!(split("config.toml"), (String::new(), "config.toml".to_string()));
}

/// A command issued through the door lands in the one engine, and its
/// event comes back through the door.
#[test]
fn a_command_through_the_web_door_reaches_the_engine_and_its_event_returns() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let (w, port) = node(&rt, tmp.path());
    let engine = rt.block_on(connect(port));
    let mut events = engine.subscribe();

    rt.block_on(async {
        engine.execute(Command::SetLanguage { lang: "de".into() }).await.expect("accepted");
        let Ok(Reply::Session(s)) = w.execute(Command::ReadSession).await else { panic!("a session") };
        assert_eq!(s.language, "de", "the native handle sees what the door did");
        loop {
            let e = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
                .await
                .expect("an event within 10 s")
                .expect("the stream is open");
            if matches!(e, molt_core::Event::SessionChanged { .. }) {
                break;
            }
        }
    });
}

/// The write-only secrets never cross the wire, so a browser's settings
/// form holds them empty. A Save from there must keep every stored
/// secret - the seat key above all, or the door closes on its own user
/// (`docs/web_gui.md` §6.5).
#[test]
fn a_save_through_the_web_door_keeps_the_stored_secrets() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let (w, port) = node(&rt, tmp.path());
    let engine = rt.block_on(connect(port));
    rt.block_on(async {
        w.execute(Command::PatchSettings {
            patch: serde_json::json!({ "mcp_read_token": "read-key", "s3_secret_key": "s3-secret" }),
        })
        .await
        .expect("secrets set");

        // what the browser's form is filled from: the secrets arrive empty
        let Ok(Reply::Session(seen)) = engine.execute(Command::ReadSession).await else { panic!("a session") };
        let shown = seen.settings.clone();
        assert_eq!((shown.mcp_token.as_str(), shown.s3_secret_key.as_str()), ("", ""));

        // an untouched draft, saved
        let mut draft = shown.clone();
        draft.wake_chat_lines = !draft.wake_chat_lines;
        crate::settings::save_draft(&engine, String::new(), draft.clone(), &shown).await.expect("the save lands");

        let Ok(Reply::Session(stored)) = w.execute(Command::ReadSession).await else { panic!("a session") };
        assert_eq!(stored.settings.mcp_token, SEAT, "the seat key");
        assert_eq!(stored.settings.mcp_read_token, "read-key");
        assert_eq!(stored.settings.s3_secret_key, "s3-secret");
        assert_eq!(stored.settings.wake_chat_lines, draft.wake_chat_lines, "the edit itself landed");

        // a secret the user typed does go through
        let mut typed = shown.clone();
        typed.s3_secret_key = "new-secret".into();
        crate::settings::save_draft(&engine, String::new(), typed, &shown).await.expect("the save lands");
        let Ok(Reply::Session(stored)) = w.execute(Command::ReadSession).await else { panic!("a session") };
        assert_eq!(stored.settings.s3_secret_key, "new-secret");
        assert_eq!(stored.settings.mcp_token, SEAT);
    });
}

/// **§6.4.** The link read is the same engine read behind the door: the
/// gate admits it and the verdict crosses the wire whole.
#[test]
fn the_link_read_answers_through_the_web_door() {
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let (w, port) = node(&rt, tmp.path());
    let engine = rt.block_on(connect(port));
    let link = super::link_read::invite_link();
    let read = |e: &Engine, l: &str| rt.block_on(crate::actions::ritual::read_link(e, l.to_string())).expect("a verdict");

    let remote = read(&engine, &link);
    assert_eq!((remote.kind, remote.link.as_str(), remote.who.as_str()), (1, link.as_str(), "walter"));
    assert_eq!(remote.relays, ["ws://127.0.0.1:7777"]);
    assert_eq!(remote, read(&Engine::from(w), &link), "the same answer as the engine's own handle");
    assert_eq!(read(&engine, "hello").kind, 0);
}

// ---- two engines, the byte doors (`docs/web_gui.md` §7, W1's proof) ----

use std::collections::HashMap;
use std::time::{Duration, Instant};

use molt_ui_kit::platform::{FilePick, Platform, Save, Task};

use crate::bytes::{self, Handed};

/// The user's side of a browser: files to pick by name, and what was
/// handed back as a download.
#[derive(Default)]
struct Hands {
    files: Mutex<HashMap<String, Vec<u8>>>,
    saved: Mutex<Vec<Save>>,
}

impl Hands {
    fn holding(file: &str, bytes: &[u8]) -> Self {
        let hands = Hands::default();
        hands.files.lock().expect("files").insert(file.to_string(), bytes.to_vec());
        hands
    }

    fn saved(&self) -> Vec<Save> {
        self.saved.lock().expect("saved").clone()
    }
}

impl Platform for Hands {
    fn copy(&self, _text: String) {}
    fn paste(&self) -> Task<Option<String>> {
        Box::pin(async { None })
    }
    fn play_alert(&self, _kind: &str, _wav: fn(&str) -> Vec<u8>) {}
    fn unix_now(&self) -> u64 {
        0
    }
    fn monotonic(&self) -> Duration {
        Duration::ZERO
    }
    fn sleep(&self, time: Duration) -> Task<()> {
        Box::pin(async move { tokio::time::sleep(time).await })
    }
    fn random_token(&self) -> Option<String> {
        None
    }
    fn pick_folder(&self, _start: String) -> Task<Option<String>> {
        Box::pin(async { None })
    }
    fn pick_file(&self, _pick: FilePick) -> Task<Option<String>> {
        Box::pin(async { None })
    }
    /// A browser has no path for an engine to write to.
    fn pick_save(&self, _name: String) -> Task<Option<String>> {
        Box::pin(async { None })
    }
    fn read_file(&self, file: String) -> Task<std::io::Result<Vec<u8>>> {
        let bytes = self.files.lock().expect("files").get(&file).cloned();
        Box::pin(async move { bytes.ok_or_else(|| std::io::Error::other("no such file")) })
    }
    /// A download: the name it went under is where it went.
    fn save_file(&self, save: Save) -> Task<std::io::Result<Option<String>>> {
        let name = save.name.clone();
        self.saved.lock().expect("saved").push(save);
        Box::pin(async move { Ok(Some(name)) })
    }
}

async fn session(e: &Engine) -> SessionView {
    match e.execute(Command::ReadSession).await {
        Ok(Reply::Session(sv)) => *sv,
        other => panic!("no session: {other:?}"),
    }
}

/// Poll `read` until it answers, 60 s at most.
async fn until<T, F, Fut>(what: &str, mut read: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(got) = read().await {
            return got;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_session(e: &Engine, what: &str, pred: impl Fn(&SessionView) -> bool) -> SessionView {
    until(what, || async {
        let sv = session(e).await;
        pred(&sv).then_some(sv)
    })
    .await
}

/// A node as `moltd` starts it: storage under `root`, nothing founded.
fn bare_node(root: &std::path::Path) -> WalletHandle {
    let session = SessionView {
        workspaces: Vec::new(),
        settings: molt_core::SessionSettings {
            workspace_dir: root.join("workspaces").display().to_string(),
            download_dir: root.join("downloads").display().to_string(),
            // the file plane publishes a piece per second, not per default beat
            mirror_publish_interval_secs: 1,
            ..molt_core::SessionSettings::default()
        },
        ..SessionView::default()
    };
    molt_engine::spawn_with_storage(GroupConfig::demo(), session)
}

/// Add, confirm and unlock the local relay, as its user would.
async fn adopt_relay(e: &Engine, url: &str) {
    e.execute(Command::RelayAdd { url: url.to_string() }).await.expect("relay add");
    e.execute(Command::RelayConfirm { url: url.to_string(), accept_clearnet: true }).await.expect("relay confirm");
    wait_session(e, "the relay probe", |s| {
        s.settings.relays.iter().any(|r| r.url.trim_end_matches('/') == url.trim_end_matches('/') && r.confirmed)
    })
    .await;
    e.execute(Command::RelayClearnetSession { unlock: true }).await.expect("session unlock");
}

/// A real 2-of-2 republic over an in-process relay. petra founds it
/// THROUGH HER WEB DOOR (every command of hers below crosses the socket)
/// and walter joins on his engine's own handle; both end up entered,
/// each behind a door.
struct Pair {
    petra: WalletHandle,
    /// petra's engine as a browser window holds it.
    petra_web: Engine,
    walter: WalletHandle,
    walter_web: Engine,
    /// Session changes petra's door pushed while the ritual ran:
    /// `(when, run-scoped)`.
    ritual_events: Vec<(Duration, bool)>,
    /// How long the ritual took, relay adoption included.
    ritual_took: Duration,
    _relay: nostr_relay_builder::MockRelay,
}

async fn pair(root: &std::path::Path) -> Pair {
    let relay = nostr_relay_builder::MockRelay::run().await.expect("in-process relay");
    let url = relay.url().await.to_string();

    let petra = bare_node(&root.join("petra"));
    let petra_port = open_door(&petra, &root.join("petra"), molt_core::wire::Caps::default()).await;
    let a = connect(petra_port).await;
    let mut events = a.subscribe();
    let started = Instant::now();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let counting = seen.clone();
    let counter = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(molt_core::Event::SessionChanged { scope }) => {
                    counting.lock().expect("seen").push((started.elapsed(), scope != SessionScope::Full));
                }
                Ok(_) | Err(crate::engine::NoEvent::Lagged) => {}
                Err(crate::engine::NoEvent::Closed) => break,
            }
        }
    });

    adopt_relay(&a, &url).await;
    a.execute(Command::CreateStart {
        name: "Bee Club".to_string(),
        member: "petra".to_string(),
        threshold: 2,
        members: 2,
        relays: Vec::new(),
    })
    .await
    .expect("create start");
    let link = wait_session(&a, "a joinable seat link", |s| s.create.seats.first().is_some_and(|seat| seat.joinable))
        .await
        .create
        .seats[0]
        .link
        .clone();

    let walter = bare_node(&root.join("walter"));
    let b = Engine::from(walter.clone());
    adopt_relay(&b, &url).await;
    b.execute(Command::JoinStart { invite: link, member: "walter".to_string(), relays: Vec::new() })
        .await
        .expect("join start");
    wait_session(&a, "the join", |s| s.create.can_propose).await;
    a.execute(Command::CreatePropose {
        name: "Bee Club".to_string(),
        agenda: String::new(),
        features: vec!["memory".to_string()],
    })
    .await
    .expect("charter proposed");
    let phrase = session(&a).await.create.seed.clone();
    a.execute(Command::ConfirmSeedBackup { phrase }).await.expect("founder backup confirm");
    wait_session(&b, "the charter", |s| s.join.awaiting_ratify).await;
    b.execute(Command::JoinConfirmCharter).await.expect("ratify");
    let phrase = session(&b).await.join.seed.clone();
    b.execute(Command::ConfirmSeedBackup { phrase }).await.expect("joiner backup confirm");
    wait_session(&a, "the founding to seal", |s| s.create.run.outcome == 1).await;
    a.execute(Command::CreateFinish).await.expect("create finish");
    wait_session(&b, "the join to seal", |s| s.join.run.outcome == 1 && !s.join.sealed_id.is_empty()).await;
    b.execute(Command::JoinFinish).await.expect("join finish");
    wait_session(&b, "walter to enter", |s| s.screen == molt_core::Screen::Main && !s.workspaces.is_empty()).await;
    wait_session(&a, "petra to enter", |s| s.screen == molt_core::Screen::Main).await;

    counter.abort();
    let ritual_took = started.elapsed();
    let ritual_events = seen.lock().expect("seen").clone();
    let port = open_door(&walter, &root.join("walter"), molt_core::wire::Caps::default()).await;
    let walter_web = connect(port).await;
    // a fresh connection for petra: the first one's event stream is spent
    let petra_web = connect(petra_port).await;
    Pair { petra, petra_web, walter, walter_web, ritual_events, ritual_took, _relay: relay }
}

async fn chat_bodies(e: &Engine) -> Vec<String> {
    let read = e.execute(Command::ReadState { surface: Surface::Chat, channel: None, view: None }).await;
    let Ok(Reply::State(snap)) = read else { return Vec::new() };
    snap.applied
        .iter()
        .filter_map(|v| serde_json::from_value::<molt_core::ChatMessage>(v.clone()).ok())
        .map(|m| m.body)
        .collect()
}

async fn uploads(e: &Engine) -> Vec<molt_core::UploadView> {
    match e.execute(Command::ReadUploads).await {
        Ok(Reply::Uploads { uploads }) => uploads,
        other => panic!("no uploads: {other:?}"),
    }
}

/// The second voice: approve the open proposal with this `op`.
async fn approve_op(e: &Engine, op: &str) {
    let id = until("the proposal to reach the second voice", || async {
        let Ok(Reply::Proposals { proposals, .. }) = e.execute(Command::LIST_ALL_PROPOSALS).await else { return None };
        proposals
            .iter()
            .find(|p| {
                p.state == molt_core::ProposalState::Proposed
                    && p.payload.get("op").and_then(serde_json::Value::as_str) == Some(op)
            })
            .map(|p| p.id)
    })
    .await;
    e.execute(Command::Approve { proposal: id, note: None }).await.expect("approve");
}

/// **W1's proof.** A republic founded through the web door, joined by a
/// second engine, and chat both ways: the founder's whole ritual crossed
/// the socket. Also where the session events a ritual pushes through the
/// door are counted (`docs/web_gui.md` §11).
#[test]
fn a_republic_is_founded_joined_and_chatted_in_through_the_web_door() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let p = rt.block_on(pair(tmp.path()));
    let walter = Engine::from(p.walter.clone());

    rt.block_on(async {
        let say = |body: &str| Command::Chat { body: body.to_string(), quote: None, channel: ChannelRef::Group };
        p.petra_web.execute(say("hello from the door")).await.expect("petra chats");
        until("walter to read petra", || async {
            chat_bodies(&walter).await.contains(&"hello from the door".to_string()).then_some(())
        })
        .await;
        walter.execute(say("hello back")).await.expect("walter chats");
        until("petra to read walter through the door", || async {
            chat_bodies(&p.petra_web).await.contains(&"hello back".to_string()).then_some(())
        })
        .await;

        // what the mirror does per session event, timed over the socket
        let reads = 50u32;
        let t = Instant::now();
        let mut bytes = 0;
        for _ in 0..reads {
            bytes = serde_json::to_vec(&session(&p.petra_web).await).expect("json").len();
        }
        let per_read = t.elapsed() / reads;
        let run: Vec<Duration> = p.ritual_events.iter().filter(|(_, run)| *run).map(|(at, _)| *at).collect();
        let full = p.ritual_events.len() - run.len();
        let busiest = run.iter().map(|at| run.iter().filter(|o| **o >= *at && **o < *at + Duration::from_secs(1)).count()).max();
        eprintln!(
            "ritual over the door: {} run-scoped + {full} full session events in {:?} (at {:?}), busiest second {:?}; \
             one session read {bytes} bytes, {per_read:?}",
            run.len(),
            p.ritual_took,
            p.ritual_events.iter().map(|(at, run)| (at.as_millis(), *run)).collect::<Vec<_>>(),
            busiest,
        );
        // the timing is a measurement for the plan, not a behaviour
        assert!(!run.is_empty(), "the founding pushed its progress through the door");
    });

    // and the window behind the door shows the republic like petra's own
    let (web, w_last, w_chat) = window();
    rt.block_on(mirror_engine(&p.petra_web, &web, &w_last, &w_chat));
    let (native, n_last, n_chat) = window();
    rt.block_on(mirror(&p.petra, &native, &n_last, &n_chat));
    assert!(chat_rows(&web) >= 2, "both messages: {}", chat_rows(&web));
    assert_eq!(chat_rows(&web), chat_rows(&native));
}

/// **§7.** A file shared from BYTES through the door arrives at the other
/// member, who downloads it through HIS door in two phases, byte for
/// byte. The sharer's own copy comes back at once.
#[test]
fn a_file_shared_from_bytes_arrives_and_downloads_byte_identical() {
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let p = rt.block_on(pair(tmp.path()));
    // over one chunk, so the upload and both reads cross in several frames
    let bytes: Vec<u8> = (0..300 * 1024usize).map(|i| u8::try_from(i % 251).expect("small")).collect();

    rt.block_on(async {
        // a browser's picker answers a name, never a path the server has
        let petra_hands = Hands::holding("7/bericht.bin", &bytes);
        bytes::share_picked(&p.petra_web, &petra_hands, "7/bericht.bin".to_string(), ChannelRef::Group)
            .await
            .expect("shared from bytes");
        let share = until("the share to reach walter", || async {
            uploads(&p.walter_web).await.into_iter().find(|u| u.name == "bericht.bin")
        })
        .await;
        assert_eq!(usize::try_from(share.size).ok(), Some(bytes.len()));
        assert_eq!(share.local, "none");

        // phase one: walter's engine fetches onto ITS disk
        let hands = Hands::default();
        let chat_ui = Arc::new(Mutex::new(ChatUiState::default()));
        let mut events = p.walter_web.subscribe();
        let name = "bericht.bin".to_string();
        let started = bytes::download(&p.walter_web, &hands, &chat_ui, share.id, name).await.expect("accepted");
        assert_eq!(started, Handed::Started);
        assert!(hands.saved().is_empty(), "nothing is handed over before the fetch is done");

        // phase two, on the event the mirror gets
        let deadline = Instant::now() + Duration::from_secs(60);
        let done = loop {
            let next = tokio::time::timeout_at(deadline.into(), events.recv()).await.expect("the fetch ends in time");
            if let Ok(molt_core::Event::FileTransfer { id, phase }) = next {
                assert!(!matches!(phase, molt_core::TransferPhase::Failed { .. }), "{phase:?}");
                if let Some(name) = bytes::transfer_ended(&chat_ui, id, &phase) {
                    break name;
                }
            }
        };
        let handed = bytes::finish_download(&p.walter_web, &hands, share.id, done).await.expect("phase two");
        assert_eq!(handed, Handed::Saved("bericht.bin".to_string()));
        let saved = hands.saved();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].name, "bericht.bin");
        assert!(saved[0].bytes == bytes, "the downloaded bytes are the shared bytes");
        assert!(chat_ui.lock().expect("chat ui").to_user.is_empty(), "the fetch is settled");

        // the share is on walter's disk now: a second download is one step
        let again = bytes::download(&p.walter_web, &hands, &chat_ui, share.id, "copy.bin".to_string()).await;
        assert_eq!(again.expect("handed"), Handed::Saved("copy.bin".to_string()));
        assert!(hands.saved()[1].bytes == bytes);
        // and so is the sharer's own
        let own = bytes::download(&p.petra_web, &petra_hands, &chat_ui, share.id, "mine.bin".to_string()).await;
        assert_eq!(own.expect("handed"), Handed::Saved("mine.bin".to_string()));
        assert!(petra_hands.saved()[0].bytes == bytes);

        // a file the picker cannot read names itself and shares nothing
        let unread = bytes::share_picked(&p.petra_web, &petra_hands, "7/gone.bin".to_string(), ChannelRef::Group).await;
        assert!(matches!(unread, Err(bytes::ByteError::Unreadable(ref n)) if n == "gone.bin"), "{unread:?}");
    });
}

/// **§7.** The engine in this process keeps its paths: a share is hashed
/// where it lies and a download lands where the user points, with no
/// bytes through the window.
#[test]
fn the_engine_in_this_process_shares_and_downloads_by_path() {
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let p = rt.block_on(pair(tmp.path()));
    let petra = Engine::from(p.petra.clone());
    let src = tmp.path().join("notiz.txt");
    std::fs::write(&src, b"by path").expect("source");

    /// Answers the save dialog with one path.
    struct SavesTo(String, Hands);
    impl Platform for SavesTo {
        fn copy(&self, _text: String) {}
        fn paste(&self) -> Task<Option<String>> {
            self.1.paste()
        }
        fn play_alert(&self, _kind: &str, _wav: fn(&str) -> Vec<u8>) {}
        fn unix_now(&self) -> u64 {
            0
        }
        fn monotonic(&self) -> Duration {
            Duration::ZERO
        }
        fn sleep(&self, time: Duration) -> Task<()> {
            self.1.sleep(time)
        }
        fn random_token(&self) -> Option<String> {
            None
        }
        fn pick_folder(&self, start: String) -> Task<Option<String>> {
            self.1.pick_folder(start)
        }
        fn pick_file(&self, pick: FilePick) -> Task<Option<String>> {
            self.1.pick_file(pick)
        }
        fn pick_save(&self, _name: String) -> Task<Option<String>> {
            let dest = self.0.clone();
            Box::pin(async move { Some(dest) })
        }
        fn read_file(&self, file: String) -> Task<std::io::Result<Vec<u8>>> {
            self.1.read_file(file)
        }
        fn save_file(&self, save: Save) -> Task<std::io::Result<Option<String>>> {
            self.1.save_file(save)
        }
    }

    rt.block_on(async {
        // the platform is never asked for the bytes
        let hands = Hands::default();
        bytes::share_picked(&petra, &hands, src.display().to_string(), ChannelRef::Group).await.expect("shared by path");
        let walter = Engine::from(p.walter.clone());
        let share = until("the share to reach walter", || async {
            uploads(&walter).await.into_iter().find(|u| u.name == "notiz.txt")
        })
        .await;
        let dest = tmp.path().join("landed.txt");
        let to = SavesTo(dest.display().to_string(), Hands::default());
        let chat_ui = Arc::new(Mutex::new(ChatUiState::default()));
        let started = bytes::download(&walter, &to, &chat_ui, share.id, "notiz.txt".to_string()).await;
        assert_eq!(started.expect("accepted"), Handed::Started);
        assert!(chat_ui.lock().expect("chat ui").to_user.is_empty(), "no second phase: the engine writes the file");
        until("the file to land", || async { std::fs::read(&dest).ok().filter(|b| b == b"by path") }).await;
        assert!(to.1.saved().is_empty(), "no bytes went through the window");
    });
}

/// **§7.** A member picture and the logo set from BYTES through the door
/// show in the window behind it and in the member's own, and a replaced
/// picture replaces the one on show although its file keeps its name.
#[test]
fn a_picture_set_from_bytes_through_the_web_door_shows() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let p = rt.block_on(pair(tmp.path()));
    let walter = Engine::from(p.walter.clone());

    // the window's own callbacks, wired to the engine behind the door
    let (web, w_last, w_chat) = window();
    let ctx = crate::app::Ctx {
        spawner: rt.handle().clone().into(),
        wallet: p.petra_web.clone(),
        weak: web.as_weak(),
        last_settings: w_last.clone(),
        chat_ui: w_chat.clone(),
    };
    crate::actions::org::wire(&web, &ctx);
    rt.block_on(mirror_engine(&p.petra_web, &web, &w_last, &w_chat));
    assert!(web.get_mp_img_budget() > 0, "the republic carries pictures");
    let face = |ui: &AppWindow| ui.get_org_members().iter().find(|m| m.name == "petra").expect("petra's row");
    assert!(!face(&web).avatar_set);

    let picked = |name: &str, edge: u32| {
        let path = tmp.path().join(name);
        std::fs::write(&path, crate::tests::images::noisy_png(edge, edge)).expect("picture");
        path.display().to_string()
    };
    let applied = |op: &'static str, n: usize| {
        rt.block_on(async {
            approve_op(&walter, op).await;
            until("the change to apply at petra", || async {
                let read = p.petra_web.execute(Command::ReadState { surface: Surface::Organization, channel: None, view: None });
                let Ok(Reply::State(snap)) = read.await else { return None };
                let ops = snap.applied.iter().filter(|v| v.get("op").and_then(serde_json::Value::as_str) == Some(op));
                (ops.count() == n).then_some(())
            })
            .await;
        });
    };

    web.invoke_member_propose("set_member_image".into(), "petra".into(), picked("me.png", 64).into());
    applied("set_member_image", 1);
    rt.block_on(mirror_engine(&p.petra_web, &web, &w_last, &w_chat));
    let row = face(&web);
    assert!(row.avatar_set, "the picture set from bytes shows behind the door");
    assert_eq!(row.avatar.size().width, 64);

    // petra's own window reads the same picture the same way
    let (native, n_last, n_chat) = window();
    rt.block_on(mirror(&p.petra, &native, &n_last, &n_chat));
    assert!(face(&native).avatar_set);
    assert_eq!(face(&native).avatar.size().width, 64);

    // replaced: the same file name on the engine's disk, another picture
    web.invoke_member_propose("set_member_image".into(), "petra".into(), picked("me.png", 32).into());
    applied("set_member_image", 2);
    rt.block_on(mirror_engine(&p.petra_web, &web, &w_last, &w_chat));
    assert_eq!(face(&web).avatar.size().width, 32, "the replaced picture is the one on show");
    rt.block_on(mirror(&p.petra, &native, &n_last, &n_chat));
    assert_eq!(face(&native).avatar.size().width, 32);

    // the republic's picture, the same way - and one too large for a
    // text frame of the door (1 MiB): it can only have gone up as an
    // upload, which the engine fits to what the republic carries
    assert!(!web.get_org_img_set());
    let crest = picked("crest.png", 700);
    let weight = std::fs::metadata(&crest).expect("crest").len();
    assert!(weight > 1024 * 1024, "the fixture must not fit a frame: {weight}");
    web.invoke_org_propose("set_image".into(), crest.into());
    applied("set_image", 1);
    rt.block_on(mirror_engine(&p.petra_web, &web, &w_last, &w_chat));
    assert!(web.get_org_img_set(), "the logo set from bytes shows behind the door");
    let shown = web.get_org_img().size();
    assert!(shown.width > 0 && shown.width == shown.height, "{shown:?}");

    // the carrier is the one fork: an upload id behind the door, the
    // bytes themselves for the engine in this process
    rt.block_on(async {
        let png = crate::tests::images::noisy_png(8, 8);
        let mut remote = serde_json::json!({ "op": "set_image" });
        bytes::attach_picture(&p.petra_web, &mut remote, "a.png", png.clone()).await.expect("uploaded");
        let upload = remote["upload"].as_str().expect("an upload id").to_string();
        assert!(remote.get("bytes_b64").is_none());
        p.petra_web.execute(Command::UploadAbort { upload }).await.expect("the store had it");
        let mut native = serde_json::json!({ "op": "set_image" });
        bytes::attach_picture(&Engine::from(p.petra.clone()), &mut native, "a.png", png).await.expect("inline");
        assert!(native.get("upload").is_none() && native["bytes_b64"].as_str().is_some_and(|b| !b.is_empty()));
    });
}

/// **§7.** An export under a bare name comes to the user as the blob the
/// engine wrote; a refused one hands over nothing.
#[test]
fn an_export_comes_to_the_user_through_the_web_door() {
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let p = rt.block_on(pair(tmp.path()));
    rt.block_on(async {
        let hands = Hands::default();
        let id = session(&p.petra_web).await.active_workspace.clone();
        let export = |name: &str, pass: &str| {
            bytes::export_to_user(&p.petra_web, &hands, id.clone(), name.to_string(), pass.to_string())
        };
        let handed = export("club.molt.enc", "a long passphrase").await.expect("exported");
        assert_eq!(handed, Handed::Saved("club.molt.enc".to_string()));
        let on_disk = std::fs::read(session(&p.petra_web).await.export.dest.clone()).expect("the engine's blob");
        assert!(!on_disk.is_empty());
        assert!(hands.saved()[0].bytes == on_disk, "the user got the blob the engine wrote");

        let weak = export("club.molt.enc", "short").await;
        assert!(!matches!(weak, Ok(Handed::Saved(_))), "the engine refuses a weak passphrase: {weak:?}");
        assert_eq!(hands.saved().len(), 1, "a refused export hands over nothing");
    });
}
