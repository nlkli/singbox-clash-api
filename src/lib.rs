//! Async client for the Clash-compatible REST API of
//! [sing-box](https://sing-box.sagernet.org/).
//!
//! The crate wraps every working endpoint of the controller
//! (`experimental.clash_api`) behind typed async methods and keeps its
//! dependency tree small: `tokio` (timers), `hyper` with its pooled client
//! (plain HTTP/1.1, no TLS), `serde` and `serde_json`.
//!
//! # Quick start
//!
//! ```no_run
//! use std::time::Duration;
//! use singbox_clash_api::{Client, LogLevel};
//!
//! # async fn demo() -> singbox_clash_api::Result<()> {
//! let client = Client::builder("127.0.0.1:9090").secret("s3cret").build()?;
//!
//! // Inspect and control the core.
//! let version = client.version().await?;
//! assert!(version.is_sing_box());
//!
//! let proxies = client.proxies().await?;
//! for group in proxies.groups() {
//!     println!("{} -> {:?}", group.name, group.current());
//! }
//! client.select_proxy("Proxy", "hk-01").await?;
//!
//! let delay = client
//!     .proxy_delay("hk-01", "https://www.gstatic.com/generate_204", Duration::from_secs(5))
//!     .await?;
//! println!("hk-01: {} ms", delay.as_millis());
//!
//! // Stream live data.
//! let mut logs = client.logs(LogLevel::Info).await?;
//! while let Some(event) = logs.next().await {
//!     let event = event?;
//!     println!("[{}] {}", event.level, event.payload);
//! }
//! # Ok(()) }
//! ```
//!
//! # Design notes
//!
//! * **Authentication** uses the `Authorization: Bearer` header on every
//!   request, so the secret never appears in a URL.
//! * **Streams** (`/traffic`, `/memory`, `/logs`) use the server's chunked
//!   HTTP mode. `/connections` is only streamable over WebSocket, so
//!   [`ConnectionsFeed`] polls the snapshot endpoint instead; the data is
//!   identical.
//! * **Stub endpoints** (`/providers/*`, `/script`, `PUT /configs`, ...) do
//!   nothing on sing-box and are deliberately not exposed.
//! * **Errors**: non-2xx answers become [`Error::Api`] with the server's
//!   message; see [`Error::is_unauthorized`] and [`Error::is_not_found`].
//! * Every request must run inside a Tokio runtime.

mod client;
mod encoding;
mod error;
mod stream;
mod types;

pub use client::{Client, ClientBuilder};
pub use error::{Error, Result};
pub use stream::{ConnectionsFeed, EventStream};
pub use types::{
    Configs, Connection, DelayRecord, DnsAnswer, DnsQuestion, DnsRecord, LogEvent, LogLevel,
    Memory, Metadata, Proxies, Proxy, Rule, Snapshot, Traffic, Version,
};
