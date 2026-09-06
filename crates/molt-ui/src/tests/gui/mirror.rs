// SPDX-License-Identifier: GPL-3.0-or-later
//! The surfaces mirror PATCHES, it never rewrites (F7,
//! `docs/ui/wiki_pane_performance.md`): an engine event fires on every
//! chat line, vote and 30 s presence tick, and a wholesale rewrite made
//! that the most expensive frame the GUI has.

use super::*;

/// A founded, session-only republic with one chat line - the state the
/// mirror pushes from on every engine event.
fn founded(rt: &tokio::runtime::Runtime, root: &std::path::Path) -> WalletHandle {
    let (w, _) = node_with_chat(root);
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
        say(&w, "hello group").await;
    });
    w
}

async fn say(w: &WalletHandle, body: &str) {
    w.execute(Command::Chat {
        body: body.to_string(),
        quote: None,
        channel: ChannelRef::Group,
    })
    .await
    .ok();
}

/// The model's IDENTITY: a repeater keeps its element instances (focus,
/// scroll, the double-click counter) only while this stays one object.
fn id_of<T: Clone + PartialEq + 'static>(m: &ModelRc<T>) -> *const VecModel<T> {
    m.as_any().downcast_ref::<VecModel<T>>().expect("a VecModel")
}

/// Every model the surfaces row set hangs off, in row order.
fn instances(ui: &AppWindow) -> (*const VecModel<SurfaceTab>, Vec<(String, [usize; 5])>) {
    let m = ui.get_surfaces();
    let rows = (0..m.row_count())
        .filter_map(|i| m.row_data(i))
        .map(|t| {
            (
                t.key.to_string(),
                [
                    id_of(&t.log) as usize,
                    id_of(&t.pending) as usize,
                    id_of(&t.declined) as usize,
                    id_of(&t.accepted) as usize,
                    id_of(&t.views) as usize,
                ],
            )
        })
        .collect();
    (id_of(&m), rows)
}

/// **The keystone.** The same bundle applied twice must leave every
/// model instance - the row model AND each row's five nested models -
/// exactly where it was. A fresh `ModelRc` anywhere in there rebuilds
/// the repeater that binds to it.
#[test]
fn an_unchanged_surfaces_push_keeps_every_model_instance() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let w = founded(&rt, tmp.path());
    let ui = AppWindow::new().expect("headless window");
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));

    let b = rt
        .block_on(gather_surfaces(&w, &chat_ui))
        .expect("the founded workspace has surfaces")
        .1;
    apply_surfaces(&ui, &b);
    let (model, rows) = instances(&ui);
    assert!(!rows.is_empty(), "the surfaces landed - else this proves nothing");
    assert!(
        rows.iter().any(|(k, _)| k == "chat"),
        "the chat surface is what the log models hang off"
    );

    apply_surfaces(&ui, &b);
    let (model_again, rows_again) = instances(&ui);
    assert!(
        std::ptr::eq(model, model_again),
        "the surfaces model itself must survive a no-op push"
    );
    assert_eq!(
        rows, rows_again,
        "an engine event that changed nothing must not re-allocate a single nested model"
    );
}

/// A new chat line lands IN the existing log model: same instance, one
/// row more. The alternative (a fresh model) is what made one message
/// cost the whole pane.
#[test]
fn a_new_chat_line_grows_the_log_model_in_place() {
    i_slint_backend_testing::init_no_event_loop();
    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let w = founded(&rt, tmp.path());
    let ui = AppWindow::new().expect("headless window");
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));

    rt.block_on(async {
        let (_, b) = gather_surfaces(&w, &chat_ui).await.expect("surfaces");
        apply_surfaces(&ui, &b);
    });
    let before = surface_tab(&ui, "chat").expect("the chat tab");
    let rows_before = before.log.row_count();
    let (_, nested_before) = instances(&ui);

    rt.block_on(async {
        say(&w, "and one more").await;
        let (_, b) = gather_surfaces(&w, &chat_ui).await.expect("surfaces");
        apply_surfaces(&ui, &b);
    });
    let after = surface_tab(&ui, "chat").expect("the chat tab");
    assert_eq!(after.log.row_count(), rows_before + 1, "the line must appear");
    assert_eq!(
        after
            .log
            .row_data(after.log.row_count() - 1)
            .map(|l| l.text.to_string()),
        Some("and one more".to_string())
    );
    let (_, nested_after) = instances(&ui);
    assert_eq!(
        nested_before, nested_after,
        "the log grew IN PLACE - no model was replaced"
    );
}

/// The paint half of the keystone: the pane must not repaint when an
/// engine event carries nothing new. Rendered offscreen through the
/// software renderer, the only thing that can answer "did this frame
/// draw" - the measured 992 ms frame in
/// `docs/ui/wiki_pane_performance.md` is exactly this one.
#[test]
fn an_unchanged_surfaces_push_paints_nothing() {
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    struct P(std::rc::Rc<MinimalSoftwareWindow>);
    impl slint::platform::Platform for P {
        fn create_window_adapter(
            &self,
        ) -> Result<std::rc::Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }
    const W: u32 = 1200;
    const H: u32 = 800;
    let win = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    slint::platform::set_platform(Box::new(P(win.clone()))).expect("offscreen platform");
    win.set_size(slint::PhysicalSize::new(W, H));
    let mut buf = vec![slint::Rgb8Pixel::default(); (W * H) as usize];
    let mut draw = || win.draw_if_needed(|r| {
        r.render(&mut buf, W as usize);
    });

    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let w = founded(&rt, tmp.path());
    let ui = AppWindow::new().expect("offscreen window");
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("chat".into());
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let b = rt
        .block_on(gather_surfaces(&w, &chat_ui))
        .expect("surfaces")
        .1;
    apply_surfaces(&ui, &b);
    ui.show().expect("show offscreen");

    let mut quiet = false;
    for _ in 0..10 {
        if !draw() {
            quiet = true;
            break;
        }
    }
    assert!(quiet, "the window never settles - the probe cannot measure");

    apply_surfaces(&ui, &b);
    assert!(
        !draw(),
        "an engine event with nothing new must not repaint the pane"
    );
}

/// The measured frame (`docs/ui/wiki_pane_performance.md` §2.2): the
/// memory pane holding the real corpus, then ONE engine event - once
/// through `apply_surfaces`, once through the wholesale rewrite this
/// step replaced. Needs the corpus, so it is `#[ignore]`d:
/// `MOLT_WIKI_BENCH_CORPUS=<json [{path,content}]> … -- --ignored --nocapture`.
#[test]
#[ignore]
fn surfaces_push_frame_cost_offscreen() {
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    let Ok(path) = std::env::var("MOLT_WIKI_BENCH_CORPUS") else {
        return;
    };
    let raw = std::fs::read_to_string(&path).expect("corpus file");
    let corpus: Vec<serde_json::Value> = serde_json::from_str(&raw).expect("corpus json");
    struct P(std::rc::Rc<MinimalSoftwareWindow>);
    impl slint::platform::Platform for P {
        fn create_window_adapter(
            &self,
        ) -> Result<std::rc::Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }
    const W: u32 = 1600;
    const H: u32 = 1000;
    let win = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    slint::platform::set_platform(Box::new(P(win.clone()))).expect("offscreen platform");
    win.set_size(slint::PhysicalSize::new(W, H));
    let mut buf = vec![slint::Rgb8Pixel::default(); (W * H) as usize];
    let mut frame = |label: &str| {
        let t = std::time::Instant::now();
        let drew = win.draw_if_needed(|r| {
            r.render(&mut buf, W as usize);
        });
        println!("  frame {label}: {:?} drew={drew}", t.elapsed());
    };

    let tmp = tempfile::tempdir().expect("tmp");
    let rt = rt();
    let _guard = rt.enter();
    let w = founded(&rt, tmp.path());
    let ui = AppWindow::new().expect("offscreen window");
    apply_strings(&ui, 0);
    ui.set_screen(AppScreen::Main);
    ui.set_selected_surface("memory".into());
    ui.set_selected_view("brain".into());
    let chat_ui: Arc<Mutex<ChatUiState>> = Arc::new(Mutex::new(ChatUiState::default()));
    let b = rt
        .block_on(gather_surfaces(&w, &chat_ui))
        .expect("surfaces")
        .1;
    apply_surfaces(&ui, &b);
    let (_model, _last) = wire_wiki(&ui);
    let g = ui.global::<WikiState>();
    g.set_base_docs(ModelRc::new(VecModel::from(
        corpus
            .iter()
            .map(|d| WikiBase {
                path: d["path"].as_str().unwrap_or("").into(),
                content: String::new().into(),
                loaded: false,
            })
            .collect::<Vec<_>>(),
    )));
    g.set_base_rev(1);
    g.invoke_base_arrived();
    g.invoke_fold_all(true);
    ui.show().expect("show offscreen");
    println!("rows={}", g.get_nav_rows().row_count());
    frame("first (cold)");
    frame("idle");

    apply_surfaces(&ui, &b);
    frame("engine event via apply_surfaces (patched)");

    // what the mirror used to do: a fresh model, fresh rows, fresh nested
    // models - the 992 ms frame
    let shown = ui.get_surfaces();
    let rebuilt: Vec<SurfaceTab> = (0..shown.row_count())
        .filter_map(|i| shown.row_data(i))
        .map(|t| SurfaceTab {
            log: ModelRc::new(VecModel::from(t.log.iter().collect::<Vec<_>>())),
            pending: ModelRc::new(VecModel::from(t.pending.iter().collect::<Vec<_>>())),
            declined: ModelRc::new(VecModel::from(t.declined.iter().collect::<Vec<_>>())),
            accepted: ModelRc::new(VecModel::from(t.accepted.iter().collect::<Vec<_>>())),
            views: ModelRc::new(VecModel::from(t.views.iter().collect::<Vec<_>>())),
            ..t
        })
        .collect();
    ui.set_surfaces(ModelRc::new(VecModel::from(rebuilt)));
    frame("engine event via the wholesale rewrite (before)");
}
