// SPDX-License-Identifier: GPL-3.0-or-later
#![allow(missing_docs)]

//! Co-equality, proven end to end from the MCP side: commands built by the
//! tool layer's `build` closures drive a real engine and yield byte-identical
//! replies to engine-direct commands. This test lives HERE (not in
//! molt-engine's tests) because mcp → engine is the legal dependency
//! direction of the crate layering; the engine must never depend on a
//! surface crate, not even dev-only.

use molt_core::{ChannelRef, Command, GroupConfig, Reply, SessionView, Surface, SurfaceSnapshot};
use molt_engine::WalletHandle;
use molt_mcp::ToolDef;
use serde_json::{json, Value};

/// A single-member group: no demo peers, so no brain ever injects a
/// message and every comparison below is exact.
fn spawn_solo() -> WalletHandle {
    molt_engine::spawn(
        GroupConfig {
            member: "me".to_string(),
            members: vec!["me".to_string()],
            threshold: 1,
            self_cosign: false,
        },
        SessionView::default(),
    )
}

fn tool(name: &str) -> ToolDef {
    molt_mcp::tools()
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("{name} is in the tool catalogue"))
}

async fn run(w: &WalletHandle, cmd: Command) -> Reply {
    w.execute(cmd).await.expect("engine executes the command")
}

async fn snap(reply: Reply) -> SurfaceSnapshot {
    match reply {
        Reply::State(s) => s,
        other => panic!("unexpected reply: {other:?}"),
    }
}

/// Sends ride the tool layer, reads are compared tool-built vs direct —
/// unfiltered, `Group`, `Patch`, and `Topic` (incl. the normalization the
/// MCP layer shares with the engine).
#[tokio::test]
async fn tool_built_reads_equal_engine_direct_reads() {
    let w = spawn_solo();
    let chat_send = tool("chat_send");
    for args in [
        json!({ "body": "all hands" }),
        json!({ "body": "patch talk", "channel": { "kind": "patch", "id": 7 } }),
        json!({ "body": "budget talk", "channel": { "kind": "topic", "name": "  Budget  " } }),
    ] {
        let cmd = (chat_send.build)(&args).expect("chat_send builds");
        run(&w, cmd).await;
    }

    let read_state = tool("read_state");
    for (args, channel) in [
        (json!({ "surface": "chat" }), None),
        (
            json!({ "surface": "chat", "channel": { "kind": "group" } }),
            Some(ChannelRef::Group),
        ),
        (
            json!({ "surface": "chat", "channel": { "kind": "patch", "id": 7 } }),
            Some(ChannelRef::Patch {
                id: molt_core::ProposalId(7),
            }),
        ),
        (
            // the tool normalizes the topic name exactly like the engine
            // normalized it on send, so the filter key matches
            json!({ "surface": "chat", "channel": { "kind": "topic", "name": "  Budget  " } }),
            Some(ChannelRef::Topic {
                name: "Budget".to_string(),
            }),
        ),
    ] {
        let cmd = (read_state.build)(&args).expect("read_state builds");
        let via_tool = snap(run(&w, cmd).await).await;
        let direct = snap(
            run(
                &w,
                Command::ReadState {
                    surface: Surface::Chat,
                    channel: channel.clone(),
                    view: None,
                },
            )
            .await,
        )
        .await;
        assert_eq!(
            serde_json::to_value(&via_tool).expect("snapshot serializes"),
            serde_json::to_value(&direct).expect("snapshot serializes"),
            "tool-built and engine-direct reads must be identical for {args}"
        );
        if let Some(c) = channel {
            // every filtered row really is in the claimed channel
            for v in &via_tool.applied {
                let got: Value = v.get("channel").cloned().unwrap_or(json!({ "kind": "group" }));
                let want = serde_json::to_value(&c).expect("channel serializes");
                assert_eq!(got, want, "a filtered row leaked from another channel");
            }
        }
    }
}

/// The paged wiki reads (`knowledge_base_scale.md` §4.3) map their
/// arguments the way the engine reads them - including the two the plan
/// leaves optional, where absent must mean "the default" and not "the
/// empty string".
#[tokio::test]
async fn the_wiki_tools_build_the_reads_the_engine_serves() {
    let w = spawn_solo();
    let wiki_list = tool("wiki_list");

    // absent optionals are None / 0, never "" - a "" prefix would read as
    // a folder named "" and a 0 limit is the handler's default
    let cmd = (wiki_list.build)(&json!({})).expect("wiki_list builds bare");
    assert!(
        matches!(
            &cmd,
            Command::WikiList {
                prefix: None,
                cursor: None,
                limit: 0
            }
        ),
        "bare wiki_list built {cmd:?}"
    );
    let via_tool = run(&w, cmd).await;
    let direct = run(
        &w,
        Command::WikiList {
            prefix: None,
            cursor: None,
            limit: 0,
        },
    )
    .await;
    assert_eq!(
        serde_json::to_value(&via_tool).expect("reply serializes"),
        serde_json::to_value(&direct).expect("reply serializes"),
    );

    let cmd = (wiki_list.build)(&json!({ "prefix": "people/", "cursor": "people/a.md", "limit": 5 }))
        .expect("wiki_list builds");
    match cmd {
        Command::WikiList {
            prefix,
            cursor,
            limit,
        } => {
            assert_eq!(prefix.as_deref(), Some("people/"));
            assert_eq!(cursor.as_deref(), Some("people/a.md"));
            assert_eq!(limit, 5);
        }
        other => panic!("wiki_list built {other:?}"),
    }
    // a wrong type is a typo the caller hears about, not a silent default
    assert!((wiki_list.build)(&json!({ "prefix": 7 })).is_err());
    assert!((wiki_list.build)(&json!({ "limit": -1 })).is_err());

    let wiki_get = tool("wiki_get");
    assert!(
        (wiki_get.build)(&json!({})).is_err(),
        "wiki_get needs a path"
    );
    let cmd = (wiki_get.build)(&json!({ "path": "a.md" })).expect("wiki_get builds");
    // an empty wiki has no document, and both surfaces say so the same way
    let via_tool = w.execute(cmd).await;
    let direct = w
        .execute(Command::WikiGet {
            path: "a.md".to_string(),
        })
        .await;
    assert_eq!(
        format!("{:?}", via_tool.map(|_| ())),
        format!("{:?}", direct.map(|_| ())),
    );
}

/// The two query parameters an agent writes as JSON rather than as the
/// command's own shape: `wiki_search`'s `props` object, and the traversal
/// options `wiki_neighbors` takes. A wrong type is a typo the caller
/// hears about, never a silent default.
#[test]
fn the_wiki_query_tools_map_their_arguments() {
    let wiki_search = tool("wiki_search");
    let cmd = (wiki_search.build)(&json!({
        "query": "x",
        "props": { "status": "draft", "born": 1975 }
    }))
    .expect("wiki_search builds");
    match cmd {
        Command::WikiSearch { mut props, .. } => {
            props.sort();
            assert_eq!(
                props,
                vec![
                    ("born".to_string(), "1975".to_string()),
                    ("status".to_string(), "draft".to_string()),
                ],
                "the object becomes the command's pairs, a number as its text"
            );
        }
        other => panic!("wiki_search built {other:?}"),
    }
    let cmd = (wiki_search.build)(&json!({ "query": "x" })).expect("bare wiki_search builds");
    assert!(
        matches!(&cmd, Command::WikiSearch { props, .. } if props.is_empty()),
        "an absent props is no filter at all: {cmd:?}"
    );
    assert!((wiki_search.build)(&json!({ "props": ["status", "draft"] })).is_err());
    assert!((wiki_search.build)(&json!({ "props": { "status": ["draft"] } })).is_err());
}
