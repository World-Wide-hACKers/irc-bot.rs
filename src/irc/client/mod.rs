pub use self::msg_ctx::MessageContext;
pub use self::reaction::Reaction;
use self::session::Session;
use irc::Error;
use irc::ErrorKind;
use irc::Message;
use irc::Result;
use irc::connection::Connection;
use irc::connection::GenericConnection;
use irc::connection::GetMioTcpStream;
use irc::connection::ReceiveMessage;
use irc::connection::SendMessage;
use mio;
use pircolate;
use std::io;
use std::io::Write;

pub mod msg_ctx;
pub mod reaction;
pub mod session;

pub mod prelude {
    pub use super::session;
    pub use super::super::Message as IrcMessage;
    pub use super::super::connection::prelude::*;
}

#[derive(Debug)]
pub struct Client {
    // TODO: use smallvec.
    sessions: Vec<SessionEntry>,
}

#[derive(Debug)]
struct SessionEntry {
    inner: Session<GenericConnection>,
    // TODO: use smallvec.
    output_queue: Vec<Message>,
    is_writable: bool,
}

#[derive(Clone, Debug)]
pub struct SessionId {
    index: usize,
}

impl Client {
    pub fn new() -> Self {
        Client { sessions: Vec::new() }
    }

    pub fn add_session<Conn>(&mut self, session: Session<Conn>) -> Result<SessionId>
        where Conn: Connection
    {
        let index = self.sessions.len();

        self.sessions
            .push(SessionEntry {
                      inner: session.into_generic(),
                      output_queue: Vec::new(),
                      is_writable: false,
                  });

        Ok(SessionId { index: index })
    }

    pub fn run<MsgHandler>(mut self, msg_handler: MsgHandler) -> Result<()>
        where MsgHandler: Fn(&MessageContext, Result<Message>) -> Reaction
    {
        let poll = match mio::Poll::new() {
            Ok(p) => p,
            Err(err) => {
                error!("Failed to construct `mio::Poll`: {} ({:?})", err, err);
                bail!(err)
            }
        };

        let mut events = mio::Events::with_capacity(512);

        for (index, session) in self.sessions.iter().enumerate() {
            poll.register(session.inner.mio_tcp_stream(),
                          mio::Token(index),
                          mio::Ready::readable() | mio::Ready::writable(),
                          mio::PollOpt::edge())?
        }

        loop {
            let _event_qty = poll.poll(&mut events, None)?;

            for event in &events {
                let mio::Token(session_index) = event.token();
                let ref mut session = self.sessions[session_index];

                if event.readiness().is_writable() {
                    session.is_writable = true;
                }

                if session.is_writable {
                    process_writable(session, session_index);
                }

                if event.readiness().is_readable() {
                    process_readable(session, session_index, &msg_handler);
                }
            }
        }

        Ok(())
    }
}

fn process_readable<MsgHandler>(session: &mut SessionEntry,
                                session_index: usize,
                                msg_handler: MsgHandler)
    where MsgHandler: Fn(&MessageContext, Result<Message>) -> Reaction
{
    let msg_ctx = MessageContext { session_id: SessionId { index: session_index } };
    let msg_handler_with_ctx = move |m| msg_handler(&msg_ctx, m);

    loop {
        let reaction = match session.inner.recv() {
            Ok(Some(ref msg)) if msg.raw_command() == "PING" => {
                match msg.raw_message().replacen("I", "O", 1).parse() {
                    Ok(pong) => Reaction::RawMsg(pong),
                    Err(err) => msg_handler_with_ctx(Err(err.into())),
                }
            }
            Ok(Some(msg)) => msg_handler_with_ctx(Ok(msg)),
            Ok(None) => break,
            Err(Error(ErrorKind::Io(ref err), _)) if [io::ErrorKind::WouldBlock,
                                                      io::ErrorKind::TimedOut]
                                                             .contains(&err.kind()) => break,
            Err(err) => msg_handler_with_ctx(Err(err)),
        };

        process_reaction(session, session_index, reaction);
    }
}

fn process_writable(session: &mut SessionEntry, session_index: usize) {
    let mut msgs_consumed = 0;

    for (index, msg) in session.output_queue.iter().enumerate() {
        match session.inner.try_send(msg.clone()) {
            Ok(()) => msgs_consumed += 1,
            Err(Error(ErrorKind::Io(ref err), _)) if [io::ErrorKind::WouldBlock,
                                                      io::ErrorKind::TimedOut]
                                                             .contains(&err.kind()) => {
                session.is_writable = false;
                break;
            }
            Err(err) => {
                msgs_consumed += 1;
                error!("[session {}] Failed to send message {:?} (error: {})",
                       session_index,
                       msg.raw_message(),
                       err)
            }
        }
    }

    session.output_queue.drain(..msgs_consumed);
}

fn process_reaction(session: &mut SessionEntry, session_index: usize, reaction: Reaction) {
    match reaction {
        Reaction::None => {}
        Reaction::RawMsg(msg) => session.send(session_index, msg),
        Reaction::Multi(reactions) => {
            for r in reactions {
                process_reaction(session, session_index, r);
            }
        }
    }
}

impl SessionEntry {
    fn send(&mut self, session_index: usize, msg: Message) {
        match self.inner.try_send(msg.clone()) {
            Ok(()) => {
                // TODO: log the `session_index`.
            }
            Err(Error(ErrorKind::Io(ref err), _)) if [io::ErrorKind::WouldBlock,
                                                      io::ErrorKind::TimedOut]
                                                             .contains(&err.kind()) => {
                trace!("[session {}] Write would block or timed out; enqueueing message for \
                        later transmission: {:?}",
                       session_index,
                       msg.raw_message());
                self.is_writable = false;
                self.output_queue.push(msg);
            }
            Err(err) => {
                error!("[session {}] Failed to send message {:?} (error: {})",
                       session_index,
                       msg.raw_message(),
                       err)
            }
        }
    }
}
