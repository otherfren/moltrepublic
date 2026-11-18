// SPDX-License-Identifier: GPL-3.0-or-later
//! The remote engine client (`docs/web_gui.md` §6.1): a handle the GUI
//! holds where the native build holds the engine's own, and the pump that
//! moves frames between it and a socket. The handle is `Send + Sync` and
//! owns no socket; one pump task does, so a browser's `!Send` WebSocket
//! never leaks into the GUI's futures.

use std::sync::{Arc, Mutex, OnceLock};

use futures_channel::{mpsc, oneshot};
use futures_util::future::{select, Either};
use futures_util::{FutureExt, Sink, SinkExt, Stream, StreamExt};
use molt_core::wire::CHUNK;
use molt_core::{Command, Event, MoltError, Reply};

use crate::protocol::{Answer, Broken, Notice, Protocol};

/// One WebSocket message, either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wire {
    /// A JSON frame.
    Text(String),
    /// A chunk of bytes.
    Binary(Vec<u8>),
}

type Request = (Command, oneshot::Sender<Answer>);

/// The GUI's handle onto a remote engine: the same two verbs as the
/// native one.
#[derive(Clone)]
pub struct RemoteHandle {
    requests: mpsc::UnboundedSender<Request>,
    notices: Arc<Mutex<Option<mpsc::UnboundedReceiver<Notice>>>>,
    welcome: Arc<OnceLock<Welcome>>,
}

/// The connection is gone: what a command answers then.
fn gone() -> MoltError {
    MoltError::Engine("connection closed".into())
}

impl RemoteHandle {
    /// Run one command on the remote engine.
    pub async fn execute(&self, cmd: Command) -> Result<Reply, MoltError> {
        let (tx, rx) = oneshot::channel();
        self.requests.unbounded_send((cmd, tx)).map_err(|_| gone())?;
        rx.await.unwrap_or_else(|_| Err(gone()))
    }

    /// [`upload`] through this connection.
    pub async fn upload(&self, name: &str, bytes: &[u8]) -> Result<String, MoltError> {
        upload(|cmd| self.execute(cmd), name, bytes).await
    }

    /// What the server said in its welcome; `None` until it has. A
    /// command is only answered after it, so whoever holds a reply may
    /// rely on this being there.
    #[must_use]
    pub fn welcome(&self) -> Option<Welcome> {
        self.welcome.get().cloned()
    }

    /// The server's notices. There is ONE stream per connection (the GUI
    /// has one mirror); a second call gets a stream that has ended.
    #[must_use]
    pub fn subscribe(&self) -> RemoteEvents {
        let taken = self.notices.lock().ok().and_then(|mut slot| slot.take());
        RemoteEvents { notices: taken }
    }
}

/// Put `bytes` into the engine's upload store under `name` and answer
/// the upload's id (`share_upload` and a picture proposal take it). One
/// chunk at a time, each awaited: the engine takes them in order only. A
/// failed chunk drops the upload. `execute` is the engine the caller
/// holds - a remote one or the native handle, the commands are the same.
pub async fn upload<F, Fut>(execute: F, name: &str, bytes: &[u8]) -> Result<String, MoltError>
where
    F: Fn(Command) -> Fut,
    Fut: std::future::Future<Output = Result<Reply, MoltError>>,
{
    let unexpected = || MoltError::Engine("upload: unexpected reply".into());
    let size = u64::try_from(bytes.len()).map_err(|_| unexpected())?;
    let Reply::UploadProgress { upload, .. } = execute(Command::UploadBegin { name: name.to_string(), size }).await? else {
        return Err(unexpected());
    };
    let mut offset = 0usize;
    loop {
        let end = offset.saturating_add(CHUNK).min(bytes.len());
        let last = end == bytes.len();
        let chunk = Command::UploadChunk {
            upload: upload.clone(),
            offset: u64::try_from(offset).map_err(|_| unexpected())?,
            bytes: bytes[offset..end].to_vec(),
            last,
        };
        if let Err(e) = execute(chunk).await {
            // best effort: the engine also drops an upload nobody finishes
            drop(execute(Command::UploadAbort { upload }).await);
            return Err(e);
        }
        if last {
            return Ok(upload);
        }
        offset = end;
    }
}

/// Why [`RemoteEvents::recv`] has no event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoEvent {
    /// Events were dropped: re-read everything.
    Lagged,
    /// The connection ended.
    Closed,
}

/// The remote engine's event stream.
pub struct RemoteEvents {
    notices: Option<mpsc::UnboundedReceiver<Notice>>,
}

impl RemoteEvents {
    /// The next event, in the native receiver's shape.
    pub async fn recv(&mut self) -> Result<Event, NoEvent> {
        loop {
            let Some(notices) = self.notices.as_mut() else { return Err(NoEvent::Closed) };
            match notices.next().await {
                Some(Notice::Event(e)) => return Ok(*e),
                Some(Notice::Lag) => return Err(NoEvent::Lagged),
                // the welcome is the pump's business ([`Pump::run`])
                Some(Notice::Welcome { .. }) => {}
                None => return Err(NoEvent::Closed),
            }
        }
    }
}

/// What the server said in its welcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    /// The server's version.
    pub build: String,
    /// Process facts the engine does not know.
    pub caps: molt_core::wire::Caps,
}

/// Why a connection ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// The socket closed (a refused hello closes it too).
    Closed,
    /// The server broke the protocol.
    Broken(Broken),
}

/// The task that owns the socket.
pub struct Pump {
    requests: mpsc::UnboundedReceiver<Request>,
    notices: mpsc::UnboundedSender<Notice>,
    welcome: Arc<OnceLock<Welcome>>,
}

/// A handle and the pump that serves it. Nothing happens until the pump
/// runs on a socket.
#[must_use]
pub fn remote() -> (RemoteHandle, Pump) {
    let (req_tx, req_rx) = mpsc::unbounded();
    let (note_tx, note_rx) = mpsc::unbounded();
    let welcome = Arc::new(OnceLock::new());
    let handle =
        RemoteHandle { requests: req_tx, notices: Arc::new(Mutex::new(Some(note_rx))), welcome: welcome.clone() };
    (handle, Pump { requests: req_rx, notices: note_tx, welcome })
}

impl Pump {
    /// Say hello on the socket and serve the handle until it ends.
    /// `welcomed` hears the server's welcome; every command still open at
    /// the end is answered with an error, and the event stream closes.
    pub async fn run<I, O>(
        mut self,
        token: &str,
        mut incoming: I,
        mut outgoing: O,
        welcomed: oneshot::Sender<Welcome>,
    ) -> Ended
    where
        I: Stream<Item = Wire> + Unpin,
        O: Sink<Wire> + Unpin,
    {
        let mut protocol = Protocol::new();
        let mut welcomed = Some(welcomed);
        let ended = async {
            if outgoing.send(Wire::Text(Protocol::hello(token))).await.is_err() {
                return Ended::Closed;
            }
            // commands wait until the server has welcomed us
            loop {
                let step = if protocol.welcomed() {
                    match select(incoming.next(), self.requests.next()).await {
                        Either::Left((frame, _)) => Step::Frame(frame),
                        Either::Right((request, _)) => Step::Request(request.map(Box::new)),
                    }
                } else {
                    Step::Frame(incoming.next().await)
                };
                match step {
                    Step::Frame(None) | Step::Request(None) => return Ended::Closed,
                    Step::Frame(Some(Wire::Binary(bytes))) => {
                        if let Err(broken) = protocol.on_binary(&bytes) {
                            return Ended::Broken(broken);
                        }
                    }
                    Step::Frame(Some(Wire::Text(text))) => match protocol.on_text(&text) {
                        Err(broken) => return Ended::Broken(broken),
                        Ok(None) => {}
                        Ok(Some(Notice::Welcome { build, caps })) => {
                            // the handle first: a waiter woken by `welcomed`
                            // already reads it there
                            let welcome = Welcome { build, caps };
                            drop(self.welcome.set(welcome.clone()));
                            if let Some(w) = welcomed.take() {
                                drop(w.send(welcome));
                            }
                        }
                        // a GUI that stopped listening is not this task's problem
                        Ok(Some(notice)) => drop(self.notices.unbounded_send(notice)),
                    },
                    Step::Request(Some(request)) => {
                        let (cmd, waiter) = *request;
                        // `None`: answered without the server
                        let Some(out) = protocol.request_into(cmd, waiter) else { continue };
                        if outgoing.send(Wire::Text(out.text)).await.is_err() {
                            return Ended::Closed;
                        }
                        if let Some(chunk) = out.binary {
                            if outgoing.send(Wire::Binary(chunk)).await.is_err() {
                                return Ended::Closed;
                            }
                        }
                    }
                }
            }
        }
        .await;
        protocol.on_close();
        // commands queued but never sent are answered too
        self.requests.close();
        while let Some(Some((_, waiter))) = self.requests.next().now_or_never() {
            drop(waiter.send(Err(gone())));
        }
        ended
    }
}

enum Step {
    Frame(Option<Wire>),
    Request(Option<Box<Request>>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use molt_core::wire::{Caps, ClientFrame, ServerFrame, WIRE_VERSION};

    /// A fake server: what the client sent, and a way to answer.
    struct Server {
        from_client: mpsc::UnboundedReceiver<Wire>,
        to_client: mpsc::UnboundedSender<Wire>,
    }

    impl Server {
        fn say(&self, frame: &ServerFrame) {
            self.to_client.unbounded_send(Wire::Text(serde_json::to_string(frame).expect("json"))).expect("open");
        }
        async fn heard(&mut self) -> ClientFrame {
            match self.from_client.next().await.expect("a frame") {
                Wire::Text(t) => serde_json::from_str(&t).expect("a client frame"),
                Wire::Binary(_) => panic!("unexpected bytes"),
            }
        }
        async fn heard_bytes(&mut self) -> Vec<u8> {
            match self.from_client.next().await.expect("a frame") {
                Wire::Binary(b) => b,
                Wire::Text(t) => panic!("unexpected text: {t}"),
            }
        }
        fn welcome(&self) {
            self.say(&ServerFrame::Welcome { v: WIRE_VERSION, build: "7".into(), caps: Caps { embedded_tor: true } });
        }
    }

    /// The pump on in-memory pipes, driven by hand: (handle, server,
    /// the welcome, the pump's future).
    fn rig() -> (RemoteHandle, Server, oneshot::Receiver<Welcome>, impl std::future::Future<Output = Ended>) {
        let (handle, pump) = remote();
        let (c2s_tx, c2s_rx) = mpsc::unbounded();
        let (s2c_tx, s2c_rx) = mpsc::unbounded();
        let (w_tx, w_rx) = oneshot::channel();
        let fut = async move { pump.run("tok", s2c_rx, c2s_tx, w_tx).await };
        (handle, Server { from_client: c2s_rx, to_client: s2c_tx }, w_rx, fut)
    }

    /// Poll `fut` alongside `test` until the test body is done.
    fn run<T>(pump: impl std::future::Future<Output = Ended>, test: impl std::future::Future<Output = T>) -> T {
        futures_util::pin_mut!(pump);
        futures_util::pin_mut!(test);
        match block_on(select(test, pump)) {
            Either::Left((out, _)) => out,
            Either::Right((ended, _)) => panic!("the pump ended first: {ended:?}"),
        }
    }

    /// A minimal executor: these futures only wait on in-memory channels.
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        use std::task::{Context, Poll, Wake, Waker};
        struct Flag(std::sync::atomic::AtomicBool);
        impl Wake for Flag {
            fn wake(self: Arc<Self>) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let flag = Arc::new(Flag(std::sync::atomic::AtomicBool::new(true)));
        let waker = Waker::from(flag.clone());
        let mut cx = Context::from_waker(&waker);
        futures_util::pin_mut!(fut);
        for _ in 0..10_000 {
            if flag.0.swap(false, std::sync::atomic::Ordering::SeqCst) {
                if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
                    return out;
                }
            } else {
                std::thread::yield_now();
            }
        }
        panic!("the future never finished");
    }

    #[test]
    fn hello_goes_first_and_the_welcome_is_reported() {
        let (_handle, mut server, welcome, pump) = rig();
        run(pump, async {
            assert!(matches!(server.heard().await, ClientFrame::Hello { token, .. } if token == "tok"));
            server.welcome();
            let w = welcome.await.expect("welcomed");
            assert_eq!((w.build.as_str(), w.caps.embedded_tor), ("7", true));
        });
    }

    /// The handle knows the welcome too: the window reads the server's
    /// caps from the engine it holds (`docs/web_gui.md` §6.6).
    #[test]
    fn the_handle_holds_the_welcome_once_it_arrived() {
        let (handle, mut server, welcome, pump) = rig();
        assert_eq!(handle.welcome(), None, "nothing before the server spoke");
        let twin = handle.clone();
        run(pump, async {
            server.heard().await;
            server.welcome();
            let w = welcome.await.expect("welcomed");
            assert_eq!(handle.welcome(), Some(w.clone()));
            assert_eq!(twin.welcome(), Some(w), "every clone reads the one connection");
        });
    }

    #[test]
    fn a_command_is_held_until_the_welcome_then_answered() {
        let (handle, mut server, _welcome, pump) = rig();
        run(pump, async {
            let answer = handle.execute(Command::ReadSession);
            let server_side = async {
                server.heard().await; // hello
                server.welcome();
                let ClientFrame::Cmd { id, cmd } = server.heard().await else { panic!("a command") };
                assert!(matches!(*cmd, Command::ReadSession));
                server.say(&ServerFrame::Reply { id, result: Ok(Box::new(Reply::Ack)) });
            };
            let (answer, ()) = futures_util::future::join(answer, server_side).await;
            assert!(matches!(answer, Ok(Reply::Ack)));
        });
    }

    #[test]
    fn events_and_lag_reach_the_subscriber_in_the_native_shape() {
        let (handle, mut server, _welcome, pump) = rig();
        run(pump, async {
            let mut events = handle.subscribe();
            server.heard().await;
            server.welcome();
            let e = Event::SessionChanged { scope: molt_core::SessionScope::Full };
            server.say(&ServerFrame::Event(Box::new(e)));
            server.say(&ServerFrame::Lag);
            assert!(matches!(events.recv().await, Ok(Event::SessionChanged { .. })));
            assert_eq!(events.recv().await.err(), Some(NoEvent::Lagged));
            let mut second = handle.subscribe();
            assert_eq!(second.recv().await.err(), Some(NoEvent::Closed), "one stream per connection");
        });
    }

    #[test]
    fn a_closed_socket_answers_open_commands_and_ends_the_stream() {
        let (handle, mut server, _welcome, pump) = rig();
        let mut events = handle.subscribe();
        let asked = handle.execute(Command::ReadSession);
        let story = async {
            server.heard().await;
            server.welcome();
            server.heard().await; // the command, never answered
            drop(server);
        };
        let (answer, ended, ()) = block_on(futures_util::future::join3(asked, pump, story));
        assert!(matches!(answer, Err(MoltError::Engine(_))));
        assert_eq!(ended, Ended::Closed);
        assert_eq!(block_on(events.recv()).err(), Some(NoEvent::Closed));
        assert!(matches!(block_on(handle.execute(Command::ReadSession)), Err(MoltError::Engine(_))), "no hang after the end");
    }

    /// An upload over the pump: begun, then chunk by chunk in order -
    /// each a command frame and its chunk frame - and the bytes that
    /// arrive are the bytes that left.
    #[test]
    fn an_upload_goes_out_as_ordered_chunk_frames() {
        let (handle, mut server, _welcome, pump) = rig();
        let bytes: Vec<u8> = (0..=u8::MAX).cycle().take(CHUNK + 10).collect();
        run(pump, async {
            let sent = handle.upload("a.bin", &bytes);
            let server_side = async {
                server.heard().await; // hello
                server.welcome();
                let ClientFrame::Cmd { id, cmd } = server.heard().await else { panic!("a command") };
                let total = u64::try_from(bytes.len()).expect("small");
                assert!(matches!(*cmd, Command::UploadBegin { ref name, size } if name == "a.bin" && size == total));
                let upload = |received: usize, done| Reply::UploadProgress {
                    upload: "u1".into(),
                    received: u64::try_from(received).expect("small"),
                    size: total,
                    done,
                };
                server.say(&ServerFrame::Reply { id, result: Ok(Box::new(upload(0, false))) });
                let mut got = Vec::new();
                loop {
                    let ClientFrame::Cmd { id, cmd } = server.heard().await else { panic!("a command") };
                    let Command::UploadChunk { upload: of, offset, bytes: inline, last } = *cmd else { panic!("a chunk") };
                    assert_eq!(of, "u1");
                    assert!(inline.is_empty());
                    let frame = server.heard_bytes().await;
                    let chunk = molt_core::wire::Chunk::decode(&frame).expect("a chunk frame");
                    assert_eq!((chunk.id, chunk.offset, chunk.last), (id, offset, last));
                    assert_eq!(usize::try_from(offset).ok(), Some(got.len()), "in order");
                    assert!(chunk.data.len() <= CHUNK);
                    got.extend_from_slice(chunk.data);
                    server.say(&ServerFrame::Reply { id, result: Ok(Box::new(upload(got.len(), last))) });
                    if last {
                        return got;
                    }
                }
            };
            let (sent, got) = futures_util::future::join(sent, server_side).await;
            assert_eq!(sent.expect("uploaded"), "u1");
            assert_eq!(got, bytes);
        });
    }

    /// A refused chunk ends the upload: the caller hears the refusal and
    /// the store is told to drop what it has.
    #[test]
    fn a_refused_chunk_aborts_the_upload() {
        let (handle, mut server, _welcome, pump) = rig();
        run(pump, async {
            let sent = handle.upload("a.bin", b"abc");
            let server_side = async {
                server.heard().await;
                server.welcome();
                let ClientFrame::Cmd { id, .. } = server.heard().await else { panic!("a command") };
                let begun = Reply::UploadProgress { upload: "u1".into(), received: 0, size: 3, done: false };
                server.say(&ServerFrame::Reply { id, result: Ok(Box::new(begun)) });
                let ClientFrame::Cmd { id, .. } = server.heard().await else { panic!("a command") };
                server.heard_bytes().await;
                server.say(&ServerFrame::Reply { id, result: Err(MoltError::BadPayload("upload: no".into())) });
                let ClientFrame::Cmd { id, cmd } = server.heard().await else { panic!("a command") };
                assert!(matches!(*cmd, Command::UploadAbort { ref upload } if upload == "u1"));
                server.say(&ServerFrame::Reply { id, result: Ok(Box::new(Reply::Ack)) });
            };
            let (sent, ()) = futures_util::future::join(sent, server_side).await;
            assert!(matches!(sent, Err(MoltError::BadPayload(_))));
        });
    }

    #[test]
    fn a_server_that_breaks_the_protocol_ends_the_connection() {
        let (_handle, mut server, _welcome, pump) = rig();
        let story = async {
            server.heard().await;
            server.to_client.unbounded_send(Wire::Text("{".into())).expect("open");
            server
        };
        let (ended, _server) = block_on(futures_util::future::join(pump, story));
        assert!(matches!(ended, Ended::Broken(_)));
    }
}
