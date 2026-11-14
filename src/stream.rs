//! Streaming endpoints: `/logs`, `/traffic`, `/memory` and `/connections`.

use std::marker::PhantomData;
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::body::Incoming;
use serde::de::DeserializeOwned;
use tokio::time::{Interval, MissedTickBehavior};

use crate::client::Client;
use crate::error::{Error, Result};
use crate::types::Snapshot;

/// A live stream of JSON events delivered over a chunked HTTP response.
///
/// Obtained from [`Client::traffic`], [`Client::memory`] and
/// [`Client::logs`]. Call [`next`](Self::next) in a loop:
///
/// ```no_run
/// # async fn demo(client: singbox_clash_api::Client) -> singbox_clash_api::Result<()> {
/// let mut traffic = client.traffic().await?;
/// while let Some(event) = traffic.next().await {
///     let t = event?;
///     println!("up {} B/s, down {} B/s", t.up, t.down);
/// }
/// # Ok(()) }
/// ```
///
/// The server never sends keep-alives or close frames and ends a stream by
/// dropping the TCP connection, so a stream can end with an `Err` followed by
/// `None`. Reconnecting is the caller's responsibility. Dropping the stream
/// closes the connection.
#[derive(Debug)]
pub struct EventStream<T> {
    body: Incoming,
    buf: Vec<u8>,
    finished: bool,
    _event: PhantomData<fn() -> T>,
}

impl<T: DeserializeOwned> EventStream<T> {
    pub(crate) fn new(body: Incoming) -> Self {
        Self {
            body,
            buf: Vec::new(),
            finished: false,
            _event: PhantomData,
        }
    }

    /// Waits for the next event.
    ///
    /// Returns `None` once the stream is over. After an `Err` the stream is
    /// finished and subsequent calls return `None`.
    ///
    /// This method is cancel safe: dropping the returned future (for example
    /// inside `tokio::select!`) never loses data.
    pub async fn next(&mut self) -> Option<Result<T>> {
        loop {
            match self.decode_buffered() {
                Ok(Some(event)) => return Some(Ok(event)),
                Ok(None) => {}
                Err(err) => {
                    self.finished = true;
                    self.buf.clear();
                    return Some(Err(err));
                }
            }
            if self.finished {
                return None;
            }
            match self.body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        self.buf.extend_from_slice(&data);
                    }
                }
                Some(Err(err)) => {
                    self.finished = true;
                    return Some(Err(Error::transport(err)));
                }
                None => {
                    self.finished = true;
                    if self.buf.iter().any(|b| !b.is_ascii_whitespace()) {
                        self.buf.clear();
                        return Some(Err(Error::transport("stream ended in the middle of a message")));
                    }
                    return None;
                }
            }
        }
    }

    /// Extracts one complete JSON value from the buffer, if present.
    ///
    /// Values may be separated by newlines or by nothing at all; partial
    /// values stay buffered until more data arrives.
    fn decode_buffered(&mut self) -> Result<Option<T>> {
        let (event, consumed) = {
            let mut values = serde_json::Deserializer::from_slice(&self.buf).into_iter::<T>();
            match values.next() {
                None => (None, self.buf.len()),
                Some(Ok(value)) => {
                    let consumed = values.byte_offset();
                    (Some(value), consumed)
                }
                Some(Err(err)) if err.is_eof() => return Ok(None),
                Some(Err(err)) => return Err(Error::Decode(err)),
            }
        };
        self.buf.drain(..consumed);
        Ok(event)
    }
}

/// A periodic feed of connection [`Snapshot`]s.
///
/// The server only streams `/connections` over WebSocket. To keep the crate
/// free of a WebSocket stack, this type polls the plain HTTP snapshot at a
/// fixed interval, which yields exactly the same data. The first snapshot is
/// returned immediately.
///
/// ```no_run
/// # async fn demo(client: singbox_clash_api::Client) -> singbox_clash_api::Result<()> {
/// use std::time::Duration;
/// let mut feed = client.connections_feed(Duration::from_secs(1))?;
/// loop {
///     let snapshot = feed.next().await?;
///     println!("{} connections", snapshot.connections.len());
/// }
/// # }
/// ```
#[derive(Debug)]
pub struct ConnectionsFeed {
    client: Client,
    period: Duration,
    // Created lazily so that `connections_feed` also works outside a runtime.
    ticker: Option<Interval>,
}

impl ConnectionsFeed {
    pub(crate) fn new(client: Client, period: Duration) -> Result<Self> {
        if period.is_zero() {
            return Err(Error::invalid("interval must be greater than zero"));
        }
        Ok(Self {
            client,
            period,
            ticker: None,
        })
    }

    /// Waits for the next tick and fetches a snapshot.
    ///
    /// An error does not end the feed; call `next` again to retry on the next
    /// tick.
    pub async fn next(&mut self) -> Result<Snapshot> {
        let period = self.period;
        let ticker = self.ticker.get_or_insert_with(|| {
            let mut ticker = tokio::time::interval(period);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            ticker
        });
        ticker.tick().await;
        self.client.connections().await
    }
}
