//! A wall-clock bound for blocking HTTP calls.
//!
//! ureq 2 turns its `timeout` into socket read and write timeouts, and checks
//! the deadline itself only between reads of the response. That leaves phases
//! no setting reaches: name resolution has no timeout at all, and during the
//! TLS handshake the socket timeout restarts with every byte that arrives, so
//! a peer that keeps a connection barely alive holds the caller forever.
//!
//! The bound here does not depend on the socket. The exchange runs on a
//! helper thread while the caller waits on a channel, so the caller always
//! gets control back. A helper that is still blocked when the wait ends is
//! left behind; it holds one socket and ends with the process.

use std::fmt;
use std::io::{self, Read};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How the work tells the waiting caller it is still getting somewhere.
pub(crate) struct Heartbeat {
    state: Mutex<(Instant, u64)>,
}

impl Heartbeat {
    fn new() -> Self {
        Self {
            state: Mutex::new((Instant::now(), 0)),
        }
    }

    /// Record `amount` more progress (bytes, for a download) and restart the
    /// stall clock.
    pub(crate) fn progress(&self, amount: u64) {
        let mut state = self.state.lock().unwrap_or_else(|lock| lock.into_inner());
        *state = (Instant::now(), state.1.saturating_add(amount));
    }

    fn snapshot(&self) -> (Instant, u64) {
        *self.state.lock().unwrap_or_else(|lock| lock.into_inner())
    }
}

/// The work went a whole `limit` without finishing or reporting progress.
#[derive(Debug)]
pub(crate) struct Stalled {
    pub(crate) limit: Duration,
    /// Everything reported through [`Heartbeat::progress`] before the stall.
    pub(crate) progress: u64,
}

/// Run `work` on a helper thread and wait for it, giving up once it has gone
/// `limit` without finishing or calling [`Heartbeat::progress`].
///
/// Work that never reports progress gets `limit` in total. Work that reports
/// it may run for as long as it keeps doing so.
pub(crate) fn run_with_stall_limit<T, F>(limit: Duration, work: F) -> Result<T, Stalled>
where
    T: Send + 'static,
    F: FnOnce(&Heartbeat) -> T + Send + 'static,
{
    let heartbeat = Arc::new(Heartbeat::new());
    let (sender, receiver) = mpsc::channel();
    let worker = {
        let heartbeat = heartbeat.clone();
        std::thread::spawn(move || {
            // The caller may have stopped waiting; then nobody wants this.
            let _ = sender.send(work(&heartbeat));
        })
    };
    loop {
        let (last_progress, progress) = heartbeat.snapshot();
        let remaining = (last_progress + limit).saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(value) => return Ok(value),
            Err(RecvTimeoutError::Timeout) => {
                if heartbeat.snapshot().0 != last_progress {
                    continue;
                }
                // A result that arrived as the wait ran out still counts.
                if let Ok(value) = receiver.try_recv() {
                    return Ok(value);
                }
                return Err(Stalled { limit, progress });
            }
            Err(RecvTimeoutError::Disconnected) => match worker.join() {
                Err(panic) => std::panic::resume_unwind(panic),
                Ok(()) => unreachable!("the helper thread ended without sending its result"),
            },
        }
    }
}

/// Reports every chunk read from `inner` as progress, so a download counts as
/// stalled only when no bytes arrive.
pub(crate) struct ProgressReader<'a, R> {
    inner: R,
    heartbeat: &'a Heartbeat,
}

impl<'a, R> ProgressReader<'a, R> {
    pub(crate) fn new(inner: R, heartbeat: &'a Heartbeat) -> Self {
        Self { inner, heartbeat }
    }
}

impl<R: Read> Read for ProgressReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if read > 0 {
            self.heartbeat.progress(read as u64);
        }
        Ok(read)
    }
}

/// Whether an I/O failure is a socket timeout. Unix reports an expired
/// `SO_RCVTIMEO` as `WouldBlock`, and ureq does not always translate it.
pub(crate) fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}

/// Whether ureq gave up on a request because one of its own timeouts expired.
pub(crate) fn transport_timed_out(transport: &ureq::Transport) -> bool {
    let mut source = std::error::Error::source(transport);
    while let Some(cause) = source {
        if cause.downcast_ref::<io::Error>().is_some_and(is_timeout) {
            return true;
        }
        source = cause.source();
    }
    false
}

/// A response whose text body has already been read.
pub(crate) struct TextReply {
    pub(crate) status: u16,
    /// ureq reported the status as an error (4xx or 5xx).
    pub(crate) rejected: bool,
    pub(crate) retry_after: Option<String>,
    pub(crate) body: io::Result<String>,
}

/// Why no response came back at all.
#[derive(Debug)]
pub(crate) enum SendError {
    /// The whole exchange did not finish within the limit.
    TimedOut {
        url: String,
        limit: Duration,
    },
    Transport(Box<ureq::Transport>),
}

impl fmt::Display for SendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut { url, limit } => write!(
                formatter,
                "{url}: the request timed out: no complete response within {}",
                seconds(*limit)
            ),
            Self::Transport(transport) => transport.fmt(formatter),
        }
    }
}

/// Send `request` and read its text body, all within `limit`. For API calls
/// with small replies: nothing counts as progress, so `limit` covers name
/// resolution, connecting, the TLS handshake, the request and the whole
/// response.
pub(crate) fn send_for_text(
    request: ureq::Request,
    body: Option<&str>,
    limit: Duration,
) -> Result<TextReply, SendError> {
    let url = request.url().to_string();
    let body = body.map(str::to_owned);
    let exchange = run_with_stall_limit(limit, move |_| {
        let result = match &body {
            Some(body) => request.send_string(body),
            None => request.call(),
        };
        let (response, rejected) = match result {
            Ok(response) => (response, false),
            Err(ureq::Error::Status(_, response)) => (response, true),
            Err(ureq::Error::Transport(transport)) => return Err(Box::new(transport)),
        };
        Ok(TextReply {
            status: response.status(),
            rejected,
            retry_after: response.header("retry-after").map(str::to_owned),
            body: response.into_string(),
        })
    });
    match exchange {
        Ok(Ok(reply)) => Ok(reply),
        Ok(Err(transport)) => Err(SendError::Transport(transport)),
        Err(Stalled { limit, .. }) => Err(SendError::TimedOut { url, limit }),
    }
}

/// A limit as people read it: `120s`, or `1.5s` for the short limits tests use.
pub(crate) fn seconds(limit: Duration) -> String {
    if limit.subsec_millis() == 0 {
        format!("{}s", limit.as_secs())
    } else {
        format!("{:.1}s", limit.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};

    const LIMIT: Duration = Duration::from_millis(500);
    const NEVER: Duration = Duration::from_secs(60);

    #[test]
    fn work_that_finishes_in_time_returns_its_value() {
        let value = run_with_stall_limit(NEVER, |_| 7).unwrap();
        assert_eq!(value, 7);
    }

    #[test]
    fn work_that_never_returns_is_abandoned_at_the_limit() {
        // What a blocked name lookup looks like: no socket to time out on.
        let started = Instant::now();
        let stalled = run_with_stall_limit(LIMIT, |_| std::thread::sleep(NEVER)).unwrap_err();
        assert_eq!(stalled.limit, LIMIT);
        assert_eq!(stalled.progress, 0);
        assert!(started.elapsed() >= LIMIT);
        assert!(started.elapsed() < LIMIT * 10, "{:?}", started.elapsed());
    }

    #[test]
    fn progress_extends_the_wait_and_its_end_restarts_the_clock() {
        let started = Instant::now();
        let stalled = run_with_stall_limit(LIMIT, |heartbeat| {
            for _ in 0..20 {
                std::thread::sleep(LIMIT / 20);
                heartbeat.progress(4);
            }
            std::thread::sleep(NEVER);
        })
        .unwrap_err();
        assert_eq!(stalled.progress, 80);
        // One limit of steady progress, then one full limit of silence.
        assert!(started.elapsed() >= LIMIT * 2, "{:?}", started.elapsed());
    }

    #[test]
    fn a_panic_in_the_work_reaches_the_caller() {
        let caught = std::panic::catch_unwind(|| {
            run_with_stall_limit(NEVER, |_| -> u8 { panic!("the work broke") })
        });
        let panic = caught.expect_err("the panic must not be swallowed");
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"the work broke"));
    }

    fn loopback_server(serve: impl Fn(TcpStream) + Send + Sync + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let serve = Arc::new(serve);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let serve = serve.clone();
                std::thread::spawn(move || serve(stream));
            }
        });
        address.to_string()
    }

    /// The agent every forge client builds: a total timeout equal to the
    /// limit handed to `send_for_text`.
    fn forge_request(url: &str) -> ureq::Request {
        ureq::AgentBuilder::new()
            .timeout_connect(LIMIT)
            .timeout(LIMIT)
            .build()
            .get(url)
    }

    #[test]
    fn a_reply_carries_status_headers_and_body() {
        let address = loopback_server(|mut stream| {
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(
                b"HTTP/1.1 429 Too Many Requests\r\nretry-after: 7\r\ncontent-length: 4\r\n\
                  connection: close\r\n\r\nslow",
            );
        });
        let reply = send_for_text(forge_request(&format!("http://{address}/x")), None, NEVER)
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(reply.status, 429);
        assert!(reply.rejected);
        assert_eq!(reply.retry_after.as_deref(), Some("7"));
        assert_eq!(reply.body.unwrap(), "slow");
    }

    #[test]
    fn a_tls_handshake_that_only_trickles_times_out() {
        let address = loopback_server(|mut stream| {
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            // The header of a 16000-byte TLS handshake record and the start
            // of a ServerHello of matching length, then a byte every 100 ms:
            // each one restarts ureq's socket timeout.
            let _ = stream.write_all(&[0x16, 0x03, 0x03, 0x3e, 0x80, 0x02, 0x00, 0x3e, 0x7c]);
            while stream.write_all(&[0]).is_ok() {
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let url = format!("https://{address}/repos/demo");
        let started = Instant::now();
        let error = match send_for_text(forge_request(&url), None, LIMIT) {
            Ok(reply) => panic!("a stalled handshake answered HTTP {}", reply.status),
            Err(error) => error,
        };
        assert!(
            matches!(error, SendError::TimedOut { .. }),
            "ureq cannot see this stall, so the wall-clock bound must: {error}"
        );
        let message = error.to_string();
        assert!(message.contains("timed out"), "{message}");
        assert!(message.contains(&url), "{message}");
        assert!(started.elapsed() < LIMIT * 10, "{:?}", started.elapsed());
    }

    #[test]
    fn a_silent_server_is_reported_as_a_timeout_either_way() {
        // ureq's own socket timeout and the wall-clock bound expire together
        // here; whichever wins, the message must say the request timed out.
        let address = loopback_server(|stream| {
            std::thread::sleep(NEVER);
            drop(stream);
        });
        for scheme in ["http", "https"] {
            let url = format!("{scheme}://{address}/repos/demo");
            let error = match send_for_text(forge_request(&url), None, LIMIT) {
                Ok(reply) => panic!("a silent server answered HTTP {}", reply.status),
                Err(error) => error,
            };
            let timed_out = match &error {
                SendError::TimedOut { .. } => true,
                SendError::Transport(transport) => transport_timed_out(transport),
            };
            assert!(timed_out, "{error}");
            assert!(error.to_string().contains(&url), "{error}");
        }
    }

    #[test]
    fn limits_read_as_seconds() {
        assert_eq!(seconds(Duration::from_secs(120)), "120s");
        assert_eq!(seconds(Duration::from_millis(1500)), "1.5s");
    }
}
