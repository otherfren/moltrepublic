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

use crate::protocol::{Answer, Broken, Notice, Protocol, GONE};

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
    rekey: Rekey,
}

/// The rotation this handle has out, until the pump takes it over
/// ([`Pump::serve`]).
type Rekey = Arc<Mutex<Option<Rekeying>>>;

/// A seat key being written.
struct Rekeying {
    /// What the next connection presents first.
    key: String,
    /// Hears which key the server welcomed after the write's connection
    /// ended: `true` the new one, `false` the old one.
    welcomed: oneshot::Sender<bool>,
}

/// Why a rotation did not put the new key in force.
#[derive(Debug, Clone)]
pub enum NotRotated {
    /// The server refused the write; the old key stays.
    Refused(MoltError),
    /// The server no longer takes the new key but welcomed the old one:
    /// the write never landed.
    Kept,
    /// The connection ended and neither key was welcomed since: open.
    Unsure,
}

/// The connection is gone: what a command answers then.
fn gone() -> MoltError {
    MoltError::Engine(GONE.into())
}

/// Whether `error` says the connection ended before the answer came: the
/// command may or may not have run.
#[must_use]
pub fn is_gone(error: &MoltError) -> bool {
    matches!(error, MoltError::Engine(text) if text == GONE)
}

impl RemoteHandle {
    /// Run one command on the remote engine.
    pub async fn execute(&self, cmd: Command) -> Result<Reply, MoltError> {
        let (tx, rx) = oneshot::channel();
        self.requests.unbounded_send((cmd, tx)).map_err(|_| gone())?;
        rx.await.unwrap_or_else(|_| Err(gone()))
    }

    /// Run `write`, a command that makes `key` the seat key. The server
    /// closes every session of the old key, this one included, without
    /// answering the write that killed it, so the connection moves onto
    /// `key` ([`Pump::serve`]) and the answer is which key the server
    /// welcomes next: the new one (applied) or, after it refused that,
    /// the old one (never written).
    pub async fn rotate(&self, write: Command, key: String) -> Result<(), NotRotated> {
        let (welcomed, outcome) = oneshot::channel();
        self.set_rekey(Some(Rekeying { key, welcomed }));
        match self.execute(write).await {
            // answered after all: the connection still moves when it ends
            Ok(_) => Ok(()),
            Err(e) if is_gone(&e) => {
                // no pump is left to take the rotation over
                if self.requests.is_closed() {
                    self.set_rekey(None);
                }
                match outcome.await {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(NotRotated::Kept),
                    Err(_) => Err(NotRotated::Unsure),
                }
            }
            Err(e) => {
                self.set_rekey(None);
                Err(NotRotated::Refused(e))
            }
        }
    }

    fn set_rekey(&self, rekey: Option<Rekeying>) {
        if let Ok(mut slot) = self.rekey.lock() {
            *slot = rekey;
        }
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
    /// The server is another build than the one this client serves
    /// ([`Pump::only_build`]); no command was sent.
    Version,
}

/// The task that owns the socket.
pub struct Pump {
    requests: mpsc::UnboundedReceiver<Request>,
    notices: mpsc::UnboundedSender<Notice>,
    welcome: Arc<OnceLock<Welcome>>,
    /// The only server build this pump serves, if it insists on one.
    build: Option<String>,
    rekey: Rekey,
    /// A connection of this pump was welcomed before: the next welcome
    /// follows a gap in the events.
    served: bool,
    /// A rotation whose answer was lost with its connection, and what the
    /// next welcome says about it: `true` while the new key is presented.
    rotating: Option<(oneshot::Sender<bool>, bool)>,
}

impl Drop for Pump {
    /// A rotation nobody will follow any more is answered as open.
    fn drop(&mut self) {
        self.forget_rotation();
    }
}

/// What a connection tells whoever drives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    /// The server welcomed this connection.
    Welcomed(Welcome),
    /// The server dropped events; the window re-reads everything.
    Lagged,
}

/// A handle and the pump that serves it. Nothing happens until the pump
/// runs on a socket.
#[must_use]
pub fn remote() -> (RemoteHandle, Pump) {
    let (req_tx, req_rx) = mpsc::unbounded();
    let (note_tx, note_rx) = mpsc::unbounded();
    let welcome = Arc::new(OnceLock::new());
    let rekey = Rekey::default();
    let handle = RemoteHandle {
        requests: req_tx,
        notices: Arc::new(Mutex::new(Some(note_rx))),
        welcome: welcome.clone(),
        rekey: rekey.clone(),
    };
    (handle, Pump { requests: req_rx, notices: note_tx, welcome, build: None, rekey, served: false, rotating: None })
}

impl Pump {
    /// Serve only a server of this `build`: any other ends the connection
    /// at its welcome ([`Ended::Version`]), before a command is sent.
    #[must_use]
    pub fn only_build(mut self, build: &str) -> Self {
        self.build = Some(build.to_string());
        self
    }

    /// Say hello on the socket and serve the handle until it ends.
    /// `welcomed` hears the server's welcome; every command still open at
    /// the end is answered with an error, and the event stream closes.
    pub async fn run<I, O>(mut self, token: &str, incoming: I, outgoing: O, welcomed: oneshot::Sender<Welcome>) -> Ended
    where
        I: Stream<Item = Wire> + Unpin,
        O: Sink<Wire> + Unpin,
    {
        let mut welcomed = Some(welcomed);
        let mut heard = |signal| {
            if let (Signal::Welcomed(welcome), Some(tell)) = (signal, welcomed.take()) {
                drop(tell.send(welcome));
            }
        };
        self.connection(token, incoming, outgoing, &mut heard).await.0
        // dropping the pump answers what is still queued and ends the events
    }

    /// Serve the handle over connections `dial` opens, starting with
    /// `token`, until one ends for good; the pump stays usable, so the
    /// caller may serve again with another key. A connection that ends
    /// while a rotation is out ([`RemoteHandle::rotate`]) is followed by
    /// one with the new key, and by one with the old key if the server
    /// does not take the new one (the write never landed); the first
    /// welcome of the two answers the rotation. The handle's
    /// event stream stays open across connections; each later welcome
    /// reads as lag, so the window re-reads everything.
    pub async fn serve<D, Fut, I, O>(&mut self, token: &str, mut dial: D, mut heard: impl FnMut(Signal)) -> Ended
    where
        D: FnMut() -> Fut,
        Fut: std::future::Future<Output = Option<(I, O)>>,
        I: Stream<Item = Wire> + Unpin,
        O: Sink<Wire> + Unpin,
    {
        let mut token = token.to_string();
        // the key in force before the rotation whose new key is being tried
        let mut old: Option<String> = None;
        loop {
            let Some((incoming, outgoing)) = dial().await else {
                self.forget_rotation();
                return Ended::Closed;
            };
            let (ended, welcomed) = self.connection(&token, incoming, outgoing, &mut heard).await;
            if ended == Ended::Version {
                self.forget_rotation();
                return ended;
            }
            if let Some(old) = old.take() {
                if !welcomed {
                    // the new key opens nothing: the write never landed
                    if let Some((_, new)) = self.rotating.as_mut() {
                        *new = false;
                    }
                    token = old;
                    continue;
                }
            }
            // neither key was welcomed: whoever waits hears "open"
            self.rotating = None;
            let rotated = self.rekey.lock().ok().and_then(|mut slot| slot.take());
            match rotated {
                Some(Rekeying { key, welcomed }) => {
                    self.rotating = Some((welcomed, true));
                    old = Some(std::mem::replace(&mut token, key));
                }
                None => return ended,
            }
        }
    }

    /// Drop the rotation in flight, taken over or not: its caller hears
    /// that the outcome is open.
    fn forget_rotation(&mut self) {
        self.rotating = None;
        if let Ok(mut slot) = self.rekey.lock() {
            *slot = None;
        }
    }

    /// One connection: why it ended, and whether the server welcomed it.
    /// Every command open or queued at the end is answered with an error.
    async fn connection<I, O, H>(&mut self, token: &str, mut incoming: I, mut outgoing: O, heard: &mut H) -> (Ended, bool)
    where
        I: Stream<Item = Wire> + Unpin,
        O: Sink<Wire> + Unpin,
        H: FnMut(Signal),
    {
        let mut protocol = Protocol::new();
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
                    Step::Frame(Some(wire)) => {
                        // a packed frame's last piece says what a text frame says
                        let said = match wire {
                            Wire::Binary(bytes) => protocol.on_binary(&bytes),
                            Wire::Text(text) => protocol.on_text(&text),
                        };
                        match said {
                            Err(broken) => return Ended::Broken(broken),
                            Ok(None) => {}
                            Ok(Some(Notice::Welcome { build, caps })) => {
                                // the key was taken, whatever build answers
                                if let Some((tell, new)) = self.rotating.take() {
                                    let _ = tell.send(new);
                                }
                                // before `protocol.welcomed()` is read again:
                                // no request leaves the queue for another build
                                if self.build.as_deref().is_some_and(|only| only != build) {
                                    return Ended::Version;
                                }
                                // the handle first: whoever `heard` wakes
                                // already reads it there
                                let welcome = Welcome { build, caps };
                                drop(self.welcome.set(welcome.clone()));
                                // events were missed between two connections
                                if std::mem::replace(&mut self.served, true) {
                                    drop(self.notices.unbounded_send(Notice::Lag));
                                }
                                heard(Signal::Welcomed(welcome));
                            }
                            // a GUI that stopped listening is not this task's problem
                            Ok(Some(notice)) => {
                                if matches!(notice, Notice::Lag) {
                                    heard(Signal::Lagged);
                                }
                                drop(self.notices.unbounded_send(notice));
                            }
                        }
                    }
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
        let welcomed = protocol.welcomed();
        protocol.on_close();
        // commands queued but never sent are answered too: a click must
        // not run on a later connection
        while let Some(Some((_, waiter))) = self.requests.next().now_or_never() {
            drop(waiter.send(Err(gone())));
        }
        (ended, welcomed)
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

    /// A server of another build gets the hello and nothing else: a
    /// command queued before its welcome never reaches the socket and is
    /// answered as closed (`docs/web_gui.md` §5.1).
    #[test]
    fn another_build_ends_the_connection_before_a_command_crosses() {
        let (handle, pump) = remote();
        let (c2s_tx, c2s_rx) = mpsc::unbounded();
        let (s2c_tx, s2c_rx) = mpsc::unbounded();
        let (w_tx, welcome) = oneshot::channel();
        let mut server = Server { from_client: c2s_rx, to_client: s2c_tx };
        // queued before the socket says anything
        let asked = handle.execute(Command::ReadSession);
        server.welcome(); // build "7"
        let pump = pump.only_build("8").run("tok", s2c_rx, c2s_tx, w_tx);
        let (answer, ended) = block_on(futures_util::future::join(asked, pump));
        assert_eq!(ended, Ended::Version);
        assert!(matches!(answer, Err(MoltError::Engine(_))));
        assert!(matches!(block_on(server.heard()), ClientFrame::Hello { .. }));
        assert!(block_on(server.from_client.next()).is_none(), "nothing but the hello was sent");
        assert_eq!(handle.welcome(), None, "a foreign build's welcome is not this connection's");
        assert!(block_on(welcome).is_err());
    }

    /// The build this client insists on is welcomed like any other.
    #[test]
    fn the_expected_build_is_served() {
        let (handle, pump) = remote();
        let (c2s_tx, c2s_rx) = mpsc::unbounded();
        let (s2c_tx, s2c_rx) = mpsc::unbounded();
        let (w_tx, _welcome) = oneshot::channel();
        let mut server = Server { from_client: c2s_rx, to_client: s2c_tx };
        let pump = pump.only_build("7").run("tok", s2c_rx, c2s_tx, w_tx);
        run(pump, async {
            let answer = handle.execute(Command::ReadSession);
            let server_side = async {
                server.heard().await;
                server.welcome();
                let ClientFrame::Cmd { id, .. } = server.heard().await else { panic!("a command") };
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

    /// A connection's two ends as the pump holds them.
    type Pipes = (mpsc::UnboundedReceiver<Wire>, mpsc::UnboundedSender<Wire>);

    /// The connections a `serve` dials, handed out one by one: each is a
    /// fake server's two pipes.
    fn dialer(n: usize) -> (Vec<Server>, impl FnMut() -> futures_util::future::Ready<Option<Pipes>>) {
        let mut servers = Vec::new();
        let mut ends = std::collections::VecDeque::new();
        for _ in 0..n {
            let (c2s_tx, c2s_rx) = mpsc::unbounded();
            let (s2c_tx, s2c_rx) = mpsc::unbounded();
            servers.push(Server { from_client: c2s_rx, to_client: s2c_tx });
            ends.push_back((s2c_rx, c2s_tx));
        }
        (servers, move || futures_util::future::ready(ends.pop_front()))
    }

    async fn hello_token(server: &mut Server) -> String {
        let ClientFrame::Hello { token, .. } = server.heard().await else { panic!("a hello") };
        token
    }

    /// **Rotation (`docs/web_gui.md` §6.5).** The door never answers the
    /// write that kills its own session's key: it hangs up. The next
    /// connection presents the new key, and its welcome is the answer;
    /// the event stream lives on and reads as lag.
    #[test]
    fn a_rotated_key_moves_the_connection_onto_the_new_key() {
        let (handle, mut pump) = remote();
        let (mut servers, dial) = dialer(2);
        let mut second = servers.pop().expect("two");
        let mut first = servers.pop().expect("two");
        let mut events = handle.subscribe();
        let mut signals = Vec::new();
        let serving = pump.serve("old", dial, |s| signals.push(s));
        let story = async {
            assert_eq!(hello_token(&mut first).await, "old");
            first.welcome();
            let write = handle.rotate(Command::ReadSession, "new".into());
            let server_side = async {
                first.heard().await; // the write: stored, never answered
                drop(first); // the door closes every session of the old key
                assert_eq!(hello_token(&mut second).await, "new");
                second.welcome();
            };
            let (answer, ()) = futures_util::future::join(write, server_side).await;
            assert!(matches!(answer, Ok(())), "the new key's welcome is the answer: {answer:?}");
            assert_eq!(events.recv().await.err(), Some(NoEvent::Lagged), "events were missed: re-read");
            let asked = handle.execute(Command::ReadSession);
            let server_side = async {
                let ClientFrame::Cmd { id, .. } = second.heard().await else { panic!("a command") };
                second.say(&ServerFrame::Reply { id, result: Ok(Box::new(Reply::Ack)) });
            };
            let (answer, ()) = futures_util::future::join(asked, server_side).await;
            assert!(matches!(answer, Ok(Reply::Ack)), "the handle works on the new connection");
        };
        run(serving, story);
        assert_eq!(signals.iter().filter(|s| matches!(s, Signal::Welcomed(_))).count(), 2);
    }

    /// A server that does answer the write before it hangs up: the answer
    /// stands, and the connection still moves onto the new key.
    #[test]
    fn an_answered_rotation_is_applied_and_followed() {
        let (handle, mut pump) = remote();
        let (mut servers, dial) = dialer(2);
        let mut second = servers.pop().expect("two");
        let mut first = servers.pop().expect("two");
        let serving = pump.serve("old", dial, |_| {});
        let story = async {
            hello_token(&mut first).await;
            first.welcome();
            let write = handle.rotate(Command::ReadSession, "new".into());
            let server_side = async {
                let ClientFrame::Cmd { id, .. } = first.heard().await else { panic!("a command") };
                first.say(&ServerFrame::Reply { id, result: Ok(Box::new(Reply::Ack)) });
            };
            let (answer, ()) = futures_util::future::join(write, server_side).await;
            assert!(matches!(answer, Ok(())), "{answer:?}");
            drop(first);
            assert_eq!(hello_token(&mut second).await, "new");
        };
        run(serving, story);
    }

    /// A refused write keeps the old key: when the connection ends later
    /// nothing is redialled with a key the server never took.
    #[test]
    fn a_refused_rotation_keeps_the_old_key() {
        let (handle, mut pump) = remote();
        let (mut servers, dial) = dialer(2);
        let mut second = servers.pop().expect("two");
        let mut first = servers.pop().expect("two");
        let serving = pump.serve("old", dial, |_| {});
        let story = async {
            hello_token(&mut first).await;
            first.welcome();
            let write = handle.rotate(Command::ReadSession, "new".into());
            let server_side = async {
                let ClientFrame::Cmd { id, .. } = first.heard().await else { panic!("a command") };
                first.say(&ServerFrame::Reply { id, result: Err(MoltError::BadPayload("no".into())) });
            };
            let (answer, ()) = futures_util::future::join(write, server_side).await;
            assert!(matches!(answer, Err(NotRotated::Refused(MoltError::BadPayload(_)))), "{answer:?}");
            drop(first);
        };
        let (ended, ()) = block_on(futures_util::future::join(serving, story));
        assert_eq!(ended, Ended::Closed);
        assert!(block_on(second.from_client.next()).is_none(), "no second connection was used");
    }

    /// The write never landed: the new key is tried, the server refuses
    /// it, and the old key is presented again. Its welcome says the old
    /// key stays.
    #[test]
    fn a_rotation_the_server_never_took_falls_back_to_the_old_key() {
        let (handle, mut pump) = remote();
        let (mut servers, dial) = dialer(3);
        let mut third = servers.pop().expect("three");
        let mut second = servers.pop().expect("three");
        let mut first = servers.pop().expect("three");
        let serving = pump.serve("old", dial, |_| {});
        let story = async {
            hello_token(&mut first).await;
            first.welcome();
            let write = handle.rotate(Command::ReadSession, "new".into());
            let server_side = async {
                first.heard().await; // the write, never answered
                drop(first);
                assert_eq!(hello_token(&mut second).await, "new");
                drop(second); // refused: no welcome
                assert_eq!(hello_token(&mut third).await, "old");
                third.welcome();
            };
            let (answer, ()) = futures_util::future::join(write, server_side).await;
            assert!(matches!(answer, Err(NotRotated::Kept)), "{answer:?}");
            let asked = handle.execute(Command::ReadSession);
            let server_side = async {
                let ClientFrame::Cmd { id, .. } = third.heard().await else { panic!("a command") };
                third.say(&ServerFrame::Reply { id, result: Ok(Box::new(Reply::Ack)) });
            };
            let (answer, ()) = futures_util::future::join(asked, server_side).await;
            assert!(matches!(answer, Ok(Reply::Ack)));
        };
        run(serving, story);
    }

    /// Neither key is welcomed (the server is gone): the outcome is open,
    /// and the caller hears that instead of waiting.
    #[test]
    fn a_rotation_no_key_is_welcomed_after_stays_open() {
        let (handle, mut pump) = remote();
        let (mut servers, dial) = dialer(3);
        let mut third = servers.pop().expect("three");
        let mut second = servers.pop().expect("three");
        let mut first = servers.pop().expect("three");
        let serving = pump.serve("old", dial, |_| {});
        let story = async {
            hello_token(&mut first).await;
            first.welcome();
            let write = handle.rotate(Command::ReadSession, "new".into());
            let server_side = async {
                first.heard().await;
                drop(first);
                assert_eq!(hello_token(&mut second).await, "new");
                drop(second);
                assert_eq!(hello_token(&mut third).await, "old");
                drop(third);
            };
            futures_util::future::join(write, server_side).await.0
        };
        let (ended, answer) = block_on(futures_util::future::join(serving, story));
        assert_eq!(ended, Ended::Closed);
        assert!(matches!(answer, Err(NotRotated::Unsure)), "{answer:?}");
    }

    /// No further connection can be dialled: open as well, not a hang.
    #[test]
    fn a_rotation_with_no_way_back_stays_open() {
        let (handle, mut pump) = remote();
        let (mut servers, dial) = dialer(1);
        let mut first = servers.pop().expect("one");
        let serving = pump.serve("old", dial, |_| {});
        let story = async {
            hello_token(&mut first).await;
            first.welcome();
            let write = handle.rotate(Command::ReadSession, "new".into());
            let server_side = async {
                first.heard().await;
                drop(first);
            };
            futures_util::future::join(write, server_side).await.0
        };
        let (_, answer) = block_on(futures_util::future::join(serving, story));
        assert!(matches!(answer, Err(NotRotated::Unsure)), "{answer:?}");
    }

    /// A pump that served one connection only (`run`) and is gone: the
    /// rotation it left open is answered, and so is one asked after it.
    #[test]
    fn a_rotation_is_answered_when_its_pump_is_gone() {
        let (handle, mut server, _welcome, pump) = rig();
        let story = async {
            server.heard().await;
            server.welcome();
            let write = handle.rotate(Command::ReadSession, "new".into());
            let server_side = async {
                server.heard().await;
                drop(server);
            };
            futures_util::future::join(write, server_side).await.0
        };
        let (_, answer) = block_on(futures_util::future::join(pump, story));
        assert!(matches!(answer, Err(NotRotated::Unsure)), "{answer:?}");
        let late = block_on(handle.rotate(Command::ReadSession, "new".into()));
        assert!(matches!(late, Err(NotRotated::Unsure)), "{late:?}");
    }

    /// A pump outlives a connection that ended for good: the caller
    /// serves again with another key, and a command clicked into the dead
    /// connection was answered, not kept for the next one.
    #[test]
    fn a_pump_serves_again_after_its_connection_ended() {
        let (handle, mut pump) = remote();
        let (mut servers, mut dial) = dialer(2);
        let mut second = servers.pop().expect("two");
        let mut first = servers.pop().expect("two");
        let mut lag = 0;
        let stale = handle.execute(Command::ReadSession);
        let story = async {
            hello_token(&mut first).await;
            drop(first); // refused
        };
        let (ended, (), stale) = block_on(futures_util::future::join3(pump.serve("wrong", &mut dial, |_| {}), story, stale));
        assert_eq!(ended, Ended::Closed);
        assert!(stale.as_ref().is_err_and(is_gone));
        let serving = pump.serve("right", &mut dial, |s| lag += usize::from(s == Signal::Lagged));
        run(serving, async {
            assert_eq!(hello_token(&mut second).await, "right");
            second.welcome();
            second.say(&ServerFrame::Lag);
            let asked = handle.execute(Command::ReadSession);
            let server_side = async {
                let ClientFrame::Cmd { id, .. } = second.heard().await else { panic!("a command") };
                second.say(&ServerFrame::Reply { id, result: Ok(Box::new(Reply::Ack)) });
            };
            let (answer, ()) = futures_util::future::join(asked, server_side).await;
            assert!(matches!(answer, Ok(Reply::Ack)));
        });
        assert_eq!(lag, 1, "the server's lag is told to the driver");
    }

    /// Raw deflate, as the door packs a frame.
    fn deflated(text: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(3));
        e.write_all(text).expect("deflate");
        e.finish().expect("deflate")
    }

    impl Server {
        /// Say `frame` packed, in pieces.
        fn say_packed(&self, seq: u64, frame: &ServerFrame) {
            let packed = deflated(serde_json::to_string(frame).expect("json").as_bytes());
            let pieces: Vec<&[u8]> = packed.chunks(64).collect();
            let mut offset = 0u64;
            for (i, data) in pieces.iter().enumerate() {
                let chunk = molt_core::wire::Chunk { id: seq, offset, last: i + 1 == pieces.len(), data };
                self.to_client.unbounded_send(Wire::Binary(chunk.encode_packed())).expect("open");
                offset += u64::try_from(data.len()).expect("small");
            }
        }
    }

    /// The pump asks for packed frames and serves what arrives in them
    /// exactly like text: a reply to its caller, an event and a lag to
    /// the subscriber and the driver.
    #[test]
    fn packed_frames_are_served_like_text_frames() {
        let (handle, mut pump) = remote();
        let (mut servers, dial) = dialer(1);
        let mut server = servers.pop().expect("one");
        let mut events = handle.subscribe();
        let mut lag = 0;
        let serving = pump.serve("tok", dial, |s| lag += usize::from(s == Signal::Lagged));
        run(serving, async {
            let hello = server.heard().await;
            assert!(matches!(hello, ClientFrame::Hello { packed: true, .. }), "the client says it can unpack");
            server.welcome();
            let asked = handle.execute(Command::ReadSession);
            let server_side = async {
                let ClientFrame::Cmd { id, .. } = server.heard().await else { panic!("a command") };
                let e = Event::SessionChanged { scope: molt_core::SessionScope::Full };
                server.say_packed(1, &ServerFrame::Event(Box::new(e)));
                server.say_packed(2, &ServerFrame::Lag);
                let big = MoltError::Engine("word ".repeat(2000));
                server.say_packed(3, &ServerFrame::Reply { id, result: Err(big) });
            };
            let (answer, ()) = futures_util::future::join(asked, server_side).await;
            assert!(matches!(answer, Err(MoltError::Engine(t)) if t.len() == 10_000));
            assert!(matches!(events.recv().await, Ok(Event::SessionChanged { .. })));
            assert_eq!(events.recv().await.err(), Some(NoEvent::Lagged));
        });
        assert_eq!(lag, 1, "a packed lag is told to the driver too");
    }

    /// A frame that inflates past the ceiling ends the connection: the
    /// tab is not asked for the memory.
    #[test]
    fn a_packed_frame_over_the_ceiling_ends_the_connection() {
        let (handle, mut server, _welcome, pump) = rig();
        let asked = handle.execute(Command::ReadSession);
        let story = async {
            server.heard().await;
            server.welcome();
            server.heard().await;
            let bomb = deflated(&vec![0u8; molt_core::wire::MAX_UNPACKED + 1]);
            let chunk = molt_core::wire::Chunk { id: 1, offset: 0, last: true, data: &bomb };
            server.to_client.unbounded_send(Wire::Binary(chunk.encode_packed())).expect("open");
            server
        };
        let (answer, ended, _server) = block_on(futures_util::future::join3(asked, pump, story));
        assert_eq!(ended, Ended::Broken(Broken("packed frame over the ceiling")));
        assert!(answer.as_ref().is_err_and(is_gone), "whoever waited is answered");
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
