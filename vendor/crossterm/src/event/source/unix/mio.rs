use std::{collections::VecDeque, io, time::Duration};

use mio::{unix::SourceFd, Events, Interest, Poll, Token};
use signal_hook_mio::v1_0::Signals;

#[cfg(feature = "event-stream")]
use crate::event::sys::Waker;
use crate::event::{
    source::EventSource, sys::unix::parse::parse_event, timeout::PollTimeout, Event, InternalEvent,
};
use crate::terminal::sys::file_descriptor::{tty_fd, FileDesc};

// Tokens to identify file descriptor
const TTY_TOKEN: Token = Token(0);
const SIGNAL_TOKEN: Token = Token(1);
#[cfg(feature = "event-stream")]
const WAKE_TOKEN: Token = Token(2);

// I (@zrzka) wasn't able to read more than 1_022 bytes when testing
// reading on macOS/Linux -> we don't need bigger buffer and 1k of bytes
// is enough.
const TTY_BUFFER_SIZE: usize = 1_024;

pub(crate) struct UnixInternalEventSource {
    poll: Poll,
    events: Events,
    parser: Parser,
    tty_buffer: [u8; TTY_BUFFER_SIZE],
    tty_fd: FileDesc<'static>,
    signals: Signals,
    #[cfg(feature = "event-stream")]
    waker: Waker,
}

impl UnixInternalEventSource {
    pub fn new() -> io::Result<Self> {
        UnixInternalEventSource::from_file_descriptor(tty_fd()?)
    }

    pub(crate) fn from_file_descriptor(input_fd: FileDesc<'static>) -> io::Result<Self> {
        let poll = Poll::new()?;
        let registry = poll.registry();

        let tty_raw_fd = input_fd.raw_fd();
        let mut tty_ev = SourceFd(&tty_raw_fd);
        registry.register(&mut tty_ev, TTY_TOKEN, Interest::READABLE)?;

        let mut signals = Signals::new([signal_hook::consts::SIGWINCH])?;
        registry.register(&mut signals, SIGNAL_TOKEN, Interest::READABLE)?;

        #[cfg(feature = "event-stream")]
        let waker = Waker::new(registry, WAKE_TOKEN)?;

        Ok(UnixInternalEventSource {
            poll,
            events: Events::with_capacity(3),
            parser: Parser::default(),
            tty_buffer: [0u8; TTY_BUFFER_SIZE],
            tty_fd: input_fd,
            signals,
            #[cfg(feature = "event-stream")]
            waker,
        })
    }
}

impl EventSource for UnixInternalEventSource {
    fn try_read(&mut self, timeout: Option<Duration>) -> io::Result<Option<InternalEvent>> {
        if let Some(event) = self.parser.next() {
            return Ok(Some(event));
        }

        let timeout = PollTimeout::new(timeout);

        loop {
            if let Err(e) = self.poll.poll(&mut self.events, timeout.leftover()) {
                // Mio will throw an interrupted error in case of cursor position retrieval. We need to retry until it succeeds.
                // Previous versions of Mio (< 0.7) would automatically retry the poll call if it was interrupted (if EINTR was returned).
                // https://docs.rs/mio/0.7.0/mio/struct.Poll.html#notes
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                } else {
                    return Err(e);
                }
            };

            if self.events.is_empty() {
                // No readiness events = timeout
                return Ok(None);
            }

            // dot-agent-deck patch (see VENDORED.md): handle EVERY token in the
            // batch before returning anything. Upstream returned from inside this
            // loop — on the first parsed key, and on a SIGWINCH — so a token
            // later in the same batch was never handled, and because mio's
            // readiness is edge-triggered it was not reported again: terminal
            // bytes stayed unread until the user typed something else. The
            // parser's queue holds the events in batch order; anything left in
            // it is returned by the next call before it polls.
            let mut deferred_err: Option<io::Error> = None;
            for token in self.events.iter().map(|x| x.token()) {
                match token {
                    TTY_TOKEN => {
                        loop {
                            match self.tty_fd.read(&mut self.tty_buffer) {
                                Ok(read_count) => {
                                    if read_count > 0 {
                                        self.parser.advance(
                                            &self.tty_buffer[..read_count],
                                            read_count == TTY_BUFFER_SIZE,
                                        );
                                    }
                                }
                                Err(e) => {
                                    // No more data to read at the moment. We will receive another event
                                    if e.kind() == io::ErrorKind::WouldBlock {
                                        break;
                                    }
                                    // once more data is available to read.
                                    else if e.kind() == io::ErrorKind::Interrupted {
                                        continue;
                                    }
                                }
                            };

                            // Stop where upstream returned. The terminal
                            // descriptor is blocking, so reading on until
                            // `WouldBlock` would wait for the user's next key.
                            if !self.parser.internal_events.is_empty() {
                                break;
                            }
                        }
                    }
                    SIGNAL_TOKEN => {
                        if self.signals.pending().next() == Some(signal_hook::consts::SIGWINCH) {
                            // TODO Should we remove tput?
                            //
                            // This can take a really long time, because terminal::size can
                            // launch new process (tput) and then it parses its output. It's
                            // not a really long time from the absolute time point of view, but
                            // it's a really long time from the mio, async-std/tokio executor, ...
                            // point of view.
                            match crate::terminal::size() {
                                Ok(new_size) => self.parser.internal_events.push_back(
                                    InternalEvent::Event(Event::Resize(new_size.0, new_size.1)),
                                ),
                                Err(e) => deferred_err = deferred_err.or(Some(e)),
                            }
                        }
                    }
                    #[cfg(feature = "event-stream")]
                    WAKE_TOKEN => {
                        deferred_err = deferred_err.or(Some(std::io::Error::new(
                            std::io::ErrorKind::Interrupted,
                            "Poll operation was woken up by `Waker::wake`",
                        )));
                    }
                    _ => unreachable!("Synchronize Evented handle registration & token handling"),
                }
            }

            if let Some(e) = deferred_err {
                return Err(e);
            }
            if let Some(event) = self.parser.next() {
                return Ok(Some(event));
            }

            // Processing above can take some time, check if timeout expired
            if timeout.elapsed() {
                return Ok(None);
            }
        }
    }

    #[cfg(feature = "event-stream")]
    fn waker(&self) -> Waker {
        self.waker.clone()
    }
}

//
// Following `Parser` structure exists for two reasons:
//
//  * mimic anes Parser interface
//  * move the advancing, parsing, ... stuff out of the `try_read` method
//
#[derive(Debug)]
struct Parser {
    buffer: Vec<u8>,
    internal_events: VecDeque<InternalEvent>,
}

impl Default for Parser {
    fn default() -> Self {
        Parser {
            // This buffer is used for -> 1 <- ANSI escape sequence. Are we
            // aware of any ANSI escape sequence that is bigger? Can we make
            // it smaller?
            //
            // Probably not worth spending more time on this as "there's a plan"
            // to use the anes crate parser.
            buffer: Vec::with_capacity(256),
            // TTY_BUFFER_SIZE is 1_024 bytes. How many ANSI escape sequences can
            // fit? What is an average sequence length? Let's guess here
            // and say that the average ANSI escape sequence length is 8 bytes. Thus
            // the buffer size should be 1024/8=128 to avoid additional allocations
            // when processing large amounts of data.
            //
            // There's no need to make it bigger, because when you look at the `try_read`
            // method implementation, all events are consumed before the next TTY_BUFFER
            // is processed -> events pushed.
            internal_events: VecDeque::with_capacity(128),
        }
    }
}

impl Parser {
    fn advance(&mut self, buffer: &[u8], more: bool) {
        for (idx, byte) in buffer.iter().enumerate() {
            let more = idx + 1 < buffer.len() || more;

            self.buffer.push(*byte);

            match parse_event(&self.buffer, more) {
                Ok(Some(ie)) => {
                    self.internal_events.push_back(ie);
                    self.buffer.clear();
                }
                Ok(None) => {
                    // Event can't be parsed, because we don't have enough bytes for
                    // the current sequence. Keep the buffer and process next bytes.
                }
                Err(_) => {
                    // Event can't be parsed (not enough parameters, parameter is not a number, ...).
                    // Clear the buffer and continue with another sequence.
                    self.buffer.clear();
                }
            }
        }
    }
}

impl Iterator for Parser {
    type Item = InternalEvent;

    fn next(&mut self) -> Option<Self::Item> {
        self.internal_events.pop_front()
    }
}

// dot-agent-deck patch (see VENDORED.md): the readiness-batch reproduction.
#[cfg(all(test, not(feature = "libc")))]
mod tests {
    use std::{io::Write, os::unix::net::UnixStream, time::Duration};

    use rustix::fd::OwnedFd;

    use super::{EventSource, UnixInternalEventSource};
    use crate::{
        event::{Event, InternalEvent, KeyCode},
        terminal::sys::file_descriptor::FileDesc,
    };

    /// Reads until `wanted` is seen or a read comes back empty. A resize may
    /// surface as `Err` here, because there is no terminal to measure.
    fn saw(source: &mut UnixInternalEventSource, wanted: fn(&InternalEvent) -> bool) -> bool {
        for _ in 0..8 {
            match source.try_read(Some(Duration::from_millis(200))) {
                Ok(Some(event)) if wanted(&event) => return true,
                Ok(Some(_)) | Err(_) => continue,
                Ok(None) => return false,
            }
        }
        false
    }

    fn source() -> (UnixInternalEventSource, UnixStream) {
        // Blocking, like a real terminal: a read with nothing left to read
        // would hang the test rather than return `WouldBlock`.
        let (writer, reader) = UnixStream::pair().unwrap();
        let fd = FileDesc::Owned(OwnedFd::from(reader));
        (UnixInternalEventSource::from_file_descriptor(fd).unwrap(), writer)
    }

    /// Both orders run in one test, because a SIGWINCH reaches every live
    /// source in the process and would satisfy a concurrent test's wait.
    #[test]
    fn a_sigwinch_and_input_in_one_batch_are_both_delivered() {
        let is_key = |e: &InternalEvent| {
            matches!(e, InternalEvent::Event(Event::Key(k)) if k.code == KeyCode::Char('a'))
        };
        let is_resize = |e: &InternalEvent| matches!(e, InternalEvent::Event(Event::Resize(..)));

        // SIGWINCH first: upstream returned for the signal and stranded the key.
        let (mut s, mut w) = source();
        signal_hook::low_level::raise(signal_hook::consts::SIGWINCH).unwrap();
        w.write_all(b"a").unwrap();
        assert!(saw(&mut s, is_key), "the key after a SIGWINCH was stranded");

        // Input first: upstream returned the key and lost the SIGWINCH. Without a
        // terminal the resize surfaces as an error, so either counts.
        let (mut s, mut w) = source();
        w.write_all(b"a").unwrap();
        signal_hook::low_level::raise(signal_hook::consts::SIGWINCH).unwrap();
        let mut resized = false;
        for _ in 0..8 {
            match s.try_read(Some(Duration::from_millis(200))) {
                Ok(Some(event)) if is_resize(&event) => resized = true,
                Err(_) => resized = true,
                Ok(Some(_)) => {}
                Ok(None) => break,
            }
        }
        assert!(resized, "the SIGWINCH after a key was lost");
    }
}
