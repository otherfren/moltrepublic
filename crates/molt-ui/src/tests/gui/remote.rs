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
    let web = root.join("web");
    std::fs::create_dir_all(&web).expect("web dir");
    for f in ["index.html", "molt_web.js", "molt_web_bg.wasm"] {
        std::fs::write(web.join(f), "x").expect("bundle file");
    }
    let port = rt.block_on(async {
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
        w.execute(Command::PatchSettings { patch: serde_json::json!({ "mcp_token": SEAT }) })
            .await
            .expect("a seat key");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let door = molt_remote::WebDoor {
            build: "test".into(),
            caps: molt_core::wire::Caps::default(),
            dir: web,
            pre_auth: std::time::Duration::from_secs(10),
        };
        tokio::spawn(molt_remote::serve(listener, w.clone(), door));
        port
    });
    (w, port)
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
