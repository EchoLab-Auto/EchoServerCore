//! A small driver around [`yamux::Connection`].
//!
//! `yamux` 0.14 removed the `Control` handle that older versions exposed: the
//! connection is now polled directly through `poll_new_outbound` /
//! `poll_next_inbound`, which both take `&mut self`. Two independent tasks
//! therefore cannot share it.
//!
//! This module solves that with the usual split: one background task owns the
//! connection and multiplexes three things — outbound-open requests (futures
//! waiting on a `oneshot`), inbound streams (pushed into an `mpsc`), and the
//! connection's own I/O readiness.
//!
//! Note that `yamux` 0.14 speaks the **`futures::io`** traits, not tokio's. Pass
//! `tokio_util::compat::TokioAsyncReadCompatExt::compat()`-wrapped I/O in, and
//! wrap the returned [`Stream`] back with `FuturesAsyncReadCompatExt` to use it
//! with `tokio::io` helpers.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use yamux::{Config, Connection, Mode, Stream};

/// How many outbound-open requests may queue up before callers block.
const OPEN_QUEUE: usize = 128;
/// How many accepted inbound streams may queue before we shed load.
const INBOUND_QUEUE: usize = 256;
/// Per-poll work budget, so a flood of inbound streams cannot starve the
/// outbound side (and vice versa).
const POLL_BUDGET: usize = 32;

/// A cheap, cloneable handle used to open new streams on a connection.
#[derive(Clone)]
pub struct Mux {
    open_tx: mpsc::Sender<oneshot::Sender<Result<Stream, String>>>,
}

impl std::fmt::Debug for Mux {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mux").finish_non_exhaustive()
    }
}

impl Mux {
    /// Spawn the driver for `io` and return a handle plus the inbound-stream
    /// receiver.
    ///
    /// The receiver yields every stream the peer opens. When it is closed, the
    /// connection is gone.
    pub fn spawn<T>(io: T, mode: Mode, cfg: Config, label: impl Into<String>) -> (Mux, mpsc::Receiver<Stream>)
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (open_tx, open_rx) = mpsc::channel(OPEN_QUEUE);
        let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_QUEUE);
        let driver = Driver {
            conn: Connection::new(io, cfg, mode),
            open_rx,
            inbound_tx,
            pending_open: None,
            label: label.into(),
        };
        tokio::spawn(driver);
        (Mux { open_tx }, inbound_rx)
    }

    /// Open a new stream, waiting for the connection to be writable.
    pub async fn open(&self) -> Result<Stream, String> {
        let (tx, rx) = oneshot::channel();
        self.open_tx
            .send(tx)
            .await
            .map_err(|_| "multiplexer is shut down".to_string())?;
        rx.await.map_err(|_| "multiplexer dropped the request".to_string())?
    }

    /// `true` while the underlying connection is still accepting requests.
    pub fn is_alive(&self) -> bool {
        !self.open_tx.is_closed()
    }
}

struct Driver<T> {
    conn: Connection<T>,
    open_rx: mpsc::Receiver<oneshot::Sender<Result<Stream, String>>>,
    inbound_tx: mpsc::Sender<Stream>,
    pending_open: Option<oneshot::Sender<Result<Stream, String>>>,
    label: String,
}

impl<T> Future for Driver<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // `Connection<T>: Unpin` because `T: Unpin`, and every field is Unpin.
        let this = self.get_mut();
        let mut inbound_left = POLL_BUDGET;
        let mut outbound_left = POLL_BUDGET;

        loop {
            let mut progressed = false;

            // ── outbound: satisfy a queued open request ──────────────────
            if let Some(tx) = this.pending_open.take() {
                match this.conn.poll_new_outbound(cx) {
                    Poll::Ready(Ok(stream)) => {
                        // The caller may have gone away; the stream is simply
                        // dropped, which yamux translates into a reset.
                        let _ = tx.send(Ok(stream));
                        outbound_left = outbound_left.saturating_sub(1);
                        progressed = true;
                    }
                    Poll::Ready(Err(e)) => {
                        let _ = tx.send(Err(e.to_string()));
                        outbound_left = outbound_left.saturating_sub(1);
                        progressed = true;
                    }
                    Poll::Pending => this.pending_open = Some(tx),
                }
            } else if outbound_left > 0 {
                // No in-flight request: pick one up. `poll_recv` registers the
                // waker so a later request wakes us even while we sit Pending
                // on the socket.
                match this.open_rx.poll_recv(cx) {
                    Poll::Ready(Some(tx)) => {
                        this.pending_open = Some(tx);
                        progressed = true;
                    }
                    Poll::Ready(None) => {
                        // Every handle was dropped: this connection is orphaned.
                        tracing::debug!(conn = %this.label, "mux handle dropped, closing");
                        return Poll::Ready(());
                    }
                    Poll::Pending => {}
                }
            }

            // ── inbound: hand accepted streams to the owner ──────────────
            if inbound_left > 0 {
                match this.conn.poll_next_inbound(cx) {
                    Poll::Ready(Some(Ok(stream))) => {
                        inbound_left -= 1;
                        progressed = true;
                        if let Err(e) = this.inbound_tx.try_send(stream) {
                            // Backpressure: shedding a stream is better than
                            // buffering without bound.
                            tracing::warn!(
                                conn = %this.label,
                                error = %e,
                                "inbound stream shed (consumer too slow)"
                            );
                        }
                    }
                    Poll::Ready(Some(Err(e))) => {
                        tracing::warn!(conn = %this.label, error = %e, "yamux connection error");
                        return Poll::Ready(());
                    }
                    Poll::Ready(None) => {
                        tracing::debug!(conn = %this.label, "yamux connection closed");
                        return Poll::Ready(());
                    }
                    Poll::Pending => {}
                }
            }

            if !progressed || (outbound_left == 0 && inbound_left == 0) {
                return Poll::Pending;
            }
        }
    }
}

/// Build a [`Config`] tuned for many small, long-lived proxied connections.
pub fn default_config(max_streams: usize) -> Config {
    let mut cfg = Config::default();
    cfg.set_max_num_streams(max_streams.max(16));
    // Keep streams readable after a half-close so an HTTP/1.0-style peer that
    // shuts down its write side still receives our response.
    cfg.set_read_after_close(true);
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

    /// End-to-end: two connections over a duplex pipe, streams both ways.
    #[tokio::test]
    async fn bidir_streams() {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (mux_a, mut in_a) = Mux::spawn(a.compat(), Mode::Client, default_config(64), "a");
        let (mux_b, mut in_b) = Mux::spawn(b.compat(), Mode::Server, default_config(64), "b");

        // a opens → b accepts.
        //
        // The handler gets a *clone* of the handle: dropping the last `Mux` for
        // a connection deliberately tears it down, which is exactly what makes
        // `is_alive()` meaningful.
        let a_handle = mux_a.clone();
        let opener = tokio::spawn(async move {
            let mut s = a_handle.open().await.unwrap().compat();
            s.write_all(b"ping").await.unwrap();
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"pong");
        });

        let mut s = in_b.recv().await.expect("b should accept a stream").compat();
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        s.write_all(b"pong").await.unwrap();
        opener.await.unwrap();

        // b opens → a accepts (yamux is full duplex)
        let mut s2 = mux_b.open().await.unwrap().compat();
        s2.write_all(b"hi").await.unwrap();
        let mut s3 = in_a.recv().await.expect("a should accept a stream").compat();
        let mut buf2 = [0u8; 2];
        s3.read_exact(&mut buf2).await.unwrap();
        assert_eq!(&buf2, b"hi");
    }
}
