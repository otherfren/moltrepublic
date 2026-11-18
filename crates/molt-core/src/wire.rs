// SPDX-License-Identifier: GPL-3.0-or-later
//! The web door's wire (`docs/web_gui.md` §5.1): the frames a browser GUI
//! and `moltd` exchange over one WebSocket. JSON text frames carry the
//! contract ([`Command`], [`Reply`], [`Event`], [`MoltError`]) as it
//! already serializes; binary frames carry bulk bytes in bounded chunks,
//! and a large text frame deflated, for a client that asked (§5.6).
//!
//! Types and codecs only - no socket, no gate. Which commands a client may
//! send is the server's decision (`INTERNAL` below is its input).

use serde::{Deserialize, Serialize};

use crate::{Command, Event, MoltError, Reply};

/// The frame layout version. Client and server must agree exactly; the
/// bundle is served by the same `moltd`, so there is no compatibility
/// window to keep.
pub const WIRE_VERSION: u32 = 1;

/// The most bytes one transfer carries, either way. The engine has no
/// default file cap, so the byte doors carry this one; a larger file
/// takes the server-path flow.
pub const MAX_TRANSFER: u64 = 64 * 1024 * 1024;

/// The most bytes one [`Chunk`] carries.
pub const CHUNK: usize = 256 * 1024;

/// A server frame whose JSON is shorter than this is never packed.
pub const PACK_MIN: usize = 1024;

/// The most text one packed frame may inflate to. The client stops
/// inflating there (a small frame must not be able to ask for the tab's
/// whole memory); the server sends anything larger unpacked.
pub const MAX_UNPACKED: usize = 64 * 1024 * 1024;

/// The close code for a key that is missing, wrong, or no longer the
/// seat key.
pub const CLOSE_UNAUTHORIZED: u16 = 4401;

/// The close code for a broken protocol.
pub const CLOSE_PROTOCOL: u16 = 4400;

/// Client to server.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientFrame {
    /// The first frame, and only the first: the seat token.
    Hello {
        /// [`WIRE_VERSION`].
        v: u32,
        /// The seat token. Never in a URL, a header or a cookie.
        token: String,
        /// The client reads packed frames ([`Chunk::encode_packed`]).
        /// Additive: left out when false, so a client that cannot is
        /// the hello it always was, and a server that predates the
        /// field ignores it and never packs.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        packed: bool,
    },
    /// One command; the reply echoes `id`.
    Cmd {
        /// Chosen by the client, unique among its open requests.
        id: u64,
        /// The command.
        cmd: Box<Command>,
    },
}

/// No secret reaches a log line: not the token, and not a command's
/// payload (a settings patch carries keys, a recover a phrase).
impl std::fmt::Debug for ClientFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientFrame::Hello { v, .. } => f.debug_struct("Hello").field("v", v).finish_non_exhaustive(),
            ClientFrame::Cmd { id, cmd } => {
                let name: &'static str = (&**cmd).into();
                f.debug_struct("Cmd").field("id", id).field("cmd", &name).finish_non_exhaustive()
            }
        }
    }
}

/// What the serving process tells a client about itself, once.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caps {
    /// The binary was built with the embedded Tor client.
    #[serde(default)]
    pub embedded_tor: bool,
}

/// Server to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerFrame {
    /// The answer to an accepted `Hello`.
    Welcome {
        /// [`WIRE_VERSION`].
        v: u32,
        /// The server's version; a bundle of another version reloads.
        build: String,
        /// Process facts the engine does not know.
        #[serde(default)]
        caps: Caps,
    },
    /// The answer to one `Cmd`. Replies may arrive out of order.
    Reply {
        /// The request's id.
        id: u64,
        /// What the engine answered.
        result: Result<Box<Reply>, MoltError>,
    },
    /// An engine event. Events are invalidations; a client re-reads.
    Event(Box<Event>),
    /// Events were dropped for this client: re-read everything.
    Lag,
}

/// A binary frame: `le64 id ‖ le64 offset ‖ flags ‖ bytes`. `id` is the
/// request the bytes belong to; `last` (flag bit 1) closes the transfer.
///
/// Server to client only, flag bit 2 marks a PACKED frame: the transfer
/// is one text frame, raw-deflated, and `id` counts the connection's
/// packed frames instead of naming a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk<'a> {
    /// The request these bytes belong to.
    pub id: u64,
    /// Where `data` starts in the whole transfer.
    pub offset: u64,
    /// No chunk follows.
    pub last: bool,
    /// The bytes; may be empty on the last chunk.
    pub data: &'a [u8],
}

const CHUNK_HEADER: usize = 17;
const FLAG_LAST: u8 = 1;
const FLAG_PACKED: u8 = 2;

impl<'a> Chunk<'a> {
    /// The frame bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        self.encode_flagged(0)
    }

    /// The frame bytes of a chunk of a packed text frame.
    #[must_use]
    pub fn encode_packed(&self) -> Vec<u8> {
        self.encode_flagged(FLAG_PACKED)
    }

    fn encode_flagged(&self, flags: u8) -> Vec<u8> {
        let mut out = Vec::with_capacity(CHUNK_HEADER + self.data.len());
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.offset.to_le_bytes());
        out.push(flags | if self.last { FLAG_LAST } else { 0 });
        out.extend_from_slice(self.data);
        out
    }

    /// Parse a frame of bytes; `None` for a short header, an unknown
    /// flag, or a packed frame (only a client that asked reads those).
    #[must_use]
    pub fn decode(frame: &'a [u8]) -> Option<Self> {
        match Self::decode_any(frame)? {
            (chunk, false) => Some(chunk),
            (_, true) => None,
        }
    }

    /// Parse a frame of either kind: the chunk, and whether it is a
    /// piece of a packed text frame. `None` for a short header or an
    /// unknown flag.
    #[must_use]
    pub fn decode_any(frame: &'a [u8]) -> Option<(Self, bool)> {
        let (head, data) = frame.split_at_checked(CHUNK_HEADER)?;
        let id = u64::from_le_bytes(head[0..8].try_into().ok()?);
        let offset = u64::from_le_bytes(head[8..16].try_into().ok()?);
        let flags = head[16];
        if flags & !(FLAG_LAST | FLAG_PACKED) != 0 {
            return None;
        }
        Some((Chunk { id, offset, last: flags & FLAG_LAST != 0, data }, flags & FLAG_PACKED != 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{vault::VaultRefusal, wallet::WalletRefusal, MessageId, ProposalId, ProposalState, Surface};

    /// One of every `MoltError` variant.
    fn every_error() -> Vec<MoltError> {
        use MoltError as E;
        let p = ProposalId(7);
        let m = MessageId([9u8; 16]);
        vec![
            E::UnknownProposal(p),
            E::NotGated(Surface::Chat),
            E::BadPayload("x".into()),
            E::FeatureDisabled("vault".into()),
            E::AlreadyTerminal(p, ProposalState::Applied),
            E::NotTheProposer(p),
            E::AlreadyApproved(p),
            E::AlreadyDeclined(p),
            E::Settings("x".into()),
            E::UnknownWorkspace("w".into()),
            E::WorkspaceBusy("w".into()),
            E::WorkspaceEncrypted("w".into()),
            E::Storage("x".into()),
            E::UnknownView(Surface::Chat, "v".into()),
            E::UnknownMessage(m),
            E::CannotSign("x".into()),
            E::UnknownThread("t".into()),
            E::ThreadClosed("t".into()),
            E::NoFile(m),
            E::FileUnavailable(m),
            E::FileExpired(m),
            E::FileDeleted(m),
            E::NotYourFile(m),
            E::NotYourMessage(m),
            E::Restore("x".into()),
            E::Create("x".into()),
            E::Join("x".into()),
            E::Recover("x".into()),
            E::Poke("not enabled".into()),
            E::WikiExport("the wiki is empty".into()),
            E::WikiBasePending { have: 1, size: 2, want: "c".into() },
            E::VaultBasePending { have: 1, size: 2, want: "c".into() },
            E::Vault(VaultRefusal::Bounds),
            E::Wallet(WalletRefusal::Held(Box::new(WalletRefusal::NotYet))),
            E::IndexBuilding { done: 1, total: 2 },
            E::ChainLagging { peers_ahead: 3 },
            E::Engine("x".into()),
            E::Refused("not allowed here".into()),
        ]
    }

    /// An error crosses the wire as itself: same variant, same payload,
    /// same text. The GUI localizes on the variant, so a flattened string
    /// would lose the German copy.
    #[test]
    fn every_error_variant_round_trips() {
        let all = every_error();
        let mut seen: Vec<&'static str> = all.iter().map(<&'static str>::from).collect();
        seen.sort_unstable();
        seen.dedup();
        let mut expected = <MoltError as strum::VariantNames>::VARIANTS.to_vec();
        expected.sort_unstable();
        assert_eq!(seen, expected, "every_error() misses a variant");
        for e in all {
            let json = serde_json::to_string(&e).expect("serializes");
            let back: MoltError = serde_json::from_str(&json).expect("deserializes");
            assert_eq!(format!("{back:?}"), format!("{e:?}"), "{json}");
            assert_eq!(back.to_string(), e.to_string(), "{json}");
        }
    }

    /// The internal list names real commands, each once - a typo there
    /// would silently open or close a door.
    #[test]
    fn the_internal_list_names_real_commands_once() {
        let names = Command::variant_names();
        let mut seen = std::collections::BTreeSet::new();
        for n in crate::INTERNAL {
            assert!(names.contains(&n), "{n} is not a command");
            assert!(seen.insert(n), "{n} is listed twice");
        }
    }

    /// The frame shapes are the protocol: pinned, so a rename is a
    /// deliberate version bump.
    #[test]
    fn frame_shapes_are_pinned() {
        let hello = ClientFrame::Hello { v: WIRE_VERSION, token: "t".into(), packed: false };
        assert_eq!(serde_json::to_string(&hello).expect("json"), r#"{"hello":{"v":1,"token":"t"}}"#);
        let hello = ClientFrame::Hello { v: WIRE_VERSION, token: "t".into(), packed: true };
        assert_eq!(serde_json::to_string(&hello).expect("json"), r#"{"hello":{"v":1,"token":"t","packed":true}}"#);

        let cmd: ClientFrame =
            serde_json::from_str(r#"{"cmd":{"id":4,"cmd":{"cmd":"read_session"}}}"#).expect("a command frame");
        assert!(matches!(cmd, ClientFrame::Cmd { id: 4, ref cmd } if matches!(**cmd, Command::ReadSession)));

        let welcome = ServerFrame::Welcome { v: 1, build: "0.0.1".into(), caps: Caps { embedded_tor: true } };
        assert_eq!(
            serde_json::to_string(&welcome).expect("json"),
            r#"{"welcome":{"v":1,"build":"0.0.1","caps":{"embedded_tor":true}}}"#
        );

        let ok = ServerFrame::Reply { id: 4, result: Ok(Box::new(Reply::Ack)) };
        assert_eq!(serde_json::to_string(&ok).expect("json"), r#"{"reply":{"id":4,"result":{"Ok":{"reply":"ack"}}}}"#);

        let err = ServerFrame::Reply { id: 5, result: Err(MoltError::Engine("gone".into())) };
        assert_eq!(serde_json::to_string(&err).expect("json"), r#"{"reply":{"id":5,"result":{"Err":{"engine":"gone"}}}}"#);

        assert_eq!(serde_json::to_string(&ServerFrame::Lag).expect("json"), r#""lag""#);
    }

    /// A welcome from a server that knows no caps still parses.
    #[test]
    fn welcome_without_caps_parses() {
        let w: ServerFrame = serde_json::from_str(r#"{"welcome":{"v":1,"build":"x"}}"#).expect("parses");
        assert!(matches!(w, ServerFrame::Welcome { caps, .. } if caps == Caps::default()));
    }

    #[test]
    fn a_frame_never_prints_a_secret() {
        let hello = ClientFrame::Hello { v: 1, token: "sekrit".into(), packed: true };
        assert!(!format!("{hello:?}").contains("sekrit"));
        let patch = Command::PatchSettings { patch: serde_json::json!({ "mcp_token": "sekrit" }) };
        let shown = format!("{:?}", ClientFrame::Cmd { id: 1, cmd: Box::new(patch) });
        assert!(!shown.contains("sekrit"), "{shown}");
        assert!(shown.contains("patch_settings"), "{shown}");
    }

    /// The byte doors' shapes and bounds are the protocol too.
    #[test]
    fn byte_door_shapes_are_pinned() {
        assert_eq!(MAX_TRANSFER, 64 * 1024 * 1024);
        assert_eq!(CHUNK, 256 * 1024);
        // a full chunk frame stays under the socket's 1 MiB frame bound
        let full = Chunk { id: 1, offset: 0, last: false, data: &vec![0u8; CHUNK] }.encode();
        assert!(full.len() < 1024 * 1024);

        // the command frame of a chunk carries no bytes: they ride a `Chunk`
        let chunk: Command =
            serde_json::from_str(r#"{"cmd":"upload_chunk","upload":"u","offset":3}"#).expect("a chunk command");
        assert!(matches!(chunk, Command::UploadChunk { ref bytes, offset: 3, last: false, .. } if bytes.is_empty()));
        let begin = Command::UploadBegin { name: "a.bin".into(), size: 3 };
        assert_eq!(serde_json::to_string(&begin).expect("json"), r#"{"cmd":"upload_begin","name":"a.bin","size":3}"#);
        let progress = Reply::UploadProgress { upload: "u".into(), received: 1, size: 3, done: false };
        assert_eq!(
            serde_json::to_string(&progress).expect("json"),
            r#"{"reply":"upload_progress","upload":"u","received":1,"size":3,"done":false}"#
        );
        for door in [
            "upload_begin",
            "upload_chunk",
            "upload_abort",
            "share_upload",
            "read_file_bytes",
            "read_image_bytes",
            "read_export_bytes",
        ] {
            assert!(crate::INTERNAL.contains(&door), "{door} is no MCP tool");
        }
    }

    #[test]
    fn a_chunk_round_trips() {
        let c = Chunk { id: u64::MAX - 1, offset: 1 << 40, last: true, data: b"one" };
        let frame = c.encode();
        assert_eq!(frame.len(), 17 + 3);
        assert_eq!(Chunk::decode(&frame), Some(c));
        let empty = Chunk { id: 1, offset: 0, last: false, data: b"" };
        assert_eq!(Chunk::decode(&empty.encode()), Some(empty));
    }

    #[test]
    fn a_malformed_chunk_is_refused() {
        assert_eq!(Chunk::decode(&[0u8; 16]), None, "short header");
        let mut frame = Chunk { id: 1, offset: 0, last: false, data: b"x" }.encode();
        frame[16] = 4;
        assert_eq!(Chunk::decode(&frame), None, "unknown flag");
        assert_eq!(Chunk::decode_any(&frame), None, "unknown flag");
        assert_eq!(Chunk::decode_any(&[0u8; 16]), None, "short header");
    }

    /// A packed chunk is its own kind: the lenient decoder names it, the
    /// strict one (the server's, and every client's before packed frames
    /// existed) refuses it.
    #[test]
    fn a_packed_chunk_is_told_apart() {
        let c = Chunk { id: 3, offset: 9, last: true, data: b"zz" };
        let packed = c.encode_packed();
        assert_eq!(packed[16], 3, "last and packed");
        assert_eq!(Chunk::decode_any(&packed), Some((c, true)));
        assert_eq!(Chunk::decode(&packed), None);
        assert_eq!(Chunk::decode_any(&c.encode()), Some((c, false)));
        let open = Chunk { last: false, ..c };
        assert_eq!(open.encode_packed()[16], 2);
        assert_eq!(Chunk::decode_any(&open.encode_packed()), Some((open, true)));
    }

    /// **Old and new hello (`docs/web_gui.md` §5.6).** The field is
    /// additive both ways: a hello without it reads as "cannot unpack",
    /// and the hello type as it was before the field reads a hello that
    /// carries it - so neither pairing needs a version bump.
    #[test]
    fn the_packed_hello_is_additive_both_ways() {
        assert_eq!(WIRE_VERSION, 1, "packed frames did not move the version");
        let old: ClientFrame = serde_json::from_str(r#"{"hello":{"v":1,"token":"t"}}"#).expect("an old hello");
        assert!(matches!(old, ClientFrame::Hello { v: 1, packed: false, .. }));
        let new: ClientFrame =
            serde_json::from_str(r#"{"hello":{"v":1,"token":"t","packed":true}}"#).expect("a new hello");
        assert!(matches!(new, ClientFrame::Hello { v: 1, packed: true, .. }));

        /// The client frames exactly as a server before the field read them.
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum OldClientFrame {
            Hello {
                v: u32,
                token: String,
            },
            #[allow(dead_code)]
            Cmd {
                id: u64,
                cmd: Box<Command>,
            },
        }
        let seen: OldClientFrame =
            serde_json::from_str(r#"{"hello":{"v":1,"token":"t","packed":true}}"#).expect("an old server reads it");
        assert!(matches!(seen, OldClientFrame::Hello { v: 1, ref token } if token == "t"));
    }

    #[test]
    fn the_packed_bounds_are_pinned() {
        assert_eq!(PACK_MIN, 1024);
        assert_eq!(u64::try_from(MAX_UNPACKED).ok(), Some(MAX_TRANSFER), "one ceiling, packed or not");
    }
}
