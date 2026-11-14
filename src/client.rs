//! The [`Client`] and its [`ClientBuilder`].

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as HttpClient;
use hyper_util::rt::TokioExecutor;
use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::encoding::encode;
use crate::error::{Error, Result};
use crate::stream::{ConnectionsFeed, EventStream};
use crate::types::{
    null_default, Configs, DnsAnswer, LogEvent, LogLevel, Memory, Proxies, Proxy, Rule, Snapshot,
    Traffic, Version,
};

type Http = HttpClient<HttpConnector, Full<Bytes>>;

/// Default per-request timeout.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default TCP connect timeout.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// `POST /upgrade/ui` is synchronous and downloads an archive.
const UPGRADE_UI_TIMEOUT: Duration = Duration::from_secs(300);
/// Largest `timeout` accepted by `GET /proxies/{name}/delay` (signed 16-bit).
const MAX_PROXY_PROBE_MS: u128 = i16::MAX as u128;
/// Largest `timeout` accepted by `GET /group/{name}/delay` (signed 32-bit).
const MAX_GROUP_PROBE_MS: u128 = i32::MAX as u128;
/// Longest error text kept from a non-JSON error body.
const MAX_ERROR_TEXT: usize = 512;

/// Builder for [`Client`].
///
/// ```
/// use std::time::Duration;
/// use singbox_clash_api::Client;
///
/// let client = Client::builder("127.0.0.1:9090")
///     .secret("my-secret")
///     .timeout(Duration::from_secs(5))
///     .build()?;
/// # Ok::<(), singbox_clash_api::Error>(())
/// ```
#[derive(Clone)]
pub struct ClientBuilder {
    endpoint: String,
    secret: Option<String>,
    timeout: Duration,
    connect_timeout: Duration,
}

impl ClientBuilder {
    fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            secret: None,
            timeout: DEFAULT_TIMEOUT,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        }
    }

    /// Sets the shared secret (`secret` in the sing-box `clash_api` block).
    /// An empty string means no authentication.
    pub fn secret(mut self, secret: impl Into<String>) -> Self {
        self.secret = Some(secret.into());
        self
    }

    /// Sets the timeout for ordinary requests (default 10 s).
    ///
    /// Delay probes extend it by the probe timeout, and
    /// [`Client::upgrade_ui`] uses a fixed generous limit. Streams are not
    /// subject to it.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the TCP connect timeout (default 5 s). It also bounds how long a
    /// stream may take to connect.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Validates the settings and creates the client.
    ///
    /// No connection is made until the first request.
    pub fn build(self) -> Result<Client> {
        if self.timeout.is_zero() || self.connect_timeout.is_zero() {
            return Err(Error::invalid("timeouts must be greater than zero"));
        }
        let authority = parse_authority(&self.endpoint)?;

        let auth = match self.secret.as_deref() {
            None | Some("") => None,
            Some(secret) => {
                let mut value = HeaderValue::from_str(&format!("Bearer {secret}"))
                    .map_err(|_| Error::invalid("secret contains characters not allowed in an HTTP header"))?;
                value.set_sensitive(true);
                Some(value)
            }
        };

        let mut connector = HttpConnector::new();
        connector.set_connect_timeout(Some(self.connect_timeout));
        connector.set_nodelay(true);
        let http: Http = HttpClient::builder(TokioExecutor::new()).build(connector);

        Ok(Client {
            inner: Arc::new(Inner {
                http,
                authority,
                auth,
                timeout: self.timeout,
            }),
        })
    }
}

impl fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientBuilder")
            .field("endpoint", &self.endpoint)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .field("timeout", &self.timeout)
            .field("connect_timeout", &self.connect_timeout)
            .finish()
    }
}

/// Extracts and validates `host:port` from `host:port` or `http://host:port`.
fn parse_authority(endpoint: &str) -> Result<String> {
    let trimmed = endpoint.trim();
    let rest = if let Some(rest) = trimmed.strip_prefix("http://") {
        rest
    } else if trimmed.starts_with("https://") {
        return Err(Error::invalid(
            "https is not supported: the sing-box controller speaks plain HTTP",
        ));
    } else {
        trimmed
    };
    let authority = rest.trim_end_matches('/');
    let bad = || Error::invalid(format!("invalid controller address `{endpoint}`"));
    if authority.is_empty() || authority.contains(['/', '?', '#', '@']) {
        return Err(bad());
    }
    let uri: Uri = format!("http://{authority}").parse().map_err(|_| bad())?;
    // `http::Uri` tolerates an empty host and a non-numeric port; we do not.
    let has_port = authority
        .rfind(':')
        .is_some_and(|i| !authority[i..].contains(']'));
    match uri.authority() {
        Some(parsed)
            if parsed.as_str() == authority
                && !parsed.host().is_empty()
                && (!has_port || parsed.port_u16().is_some()) =>
        {
            Ok(authority.to_owned())
        }
        _ => Err(bad()),
    }
}

/// Asynchronous client for the sing-box Clash API.
///
/// The client is cheap to clone: clones share one connection pool, so create
/// it once and pass clones around.
///
/// ```no_run
/// # async fn demo() -> singbox_clash_api::Result<()> {
/// use singbox_clash_api::Client;
///
/// let client = Client::builder("127.0.0.1:9090").secret("s3cret").build()?;
/// println!("sing-box {}", client.version().await?.number());
/// # Ok(()) }
/// ```
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    http: Http,
    authority: String,
    auth: Option<HeaderValue>,
    timeout: Duration,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("endpoint", &self.inner.authority)
            .field("authenticated", &self.inner.auth.is_some())
            .field("timeout", &self.inner.timeout)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl Client {
    /// Starts a [`ClientBuilder`] for the controller at `endpoint`, given as
    /// `host:port` or `http://host:port` (the `external_controller` value).
    pub fn builder(endpoint: impl Into<String>) -> ClientBuilder {
        ClientBuilder::new(endpoint)
    }

    /// Creates a client without authentication and with default timeouts.
    pub fn new(endpoint: impl Into<String>) -> Result<Self> {
        Self::builder(endpoint).build()
    }
}

// ---------------------------------------------------------------------------
// System
// ---------------------------------------------------------------------------

impl Client {
    /// Liveness and credential check (`GET /`).
    ///
    /// Fails with [`Error::is_unauthorized`] if the secret is wrong.
    pub async fn ping(&self) -> Result<()> {
        self.call(Method::GET, "/", None).await
    }

    /// Server version (`GET /version`).
    ///
    /// Use [`Version::is_sing_box`] to tell sing-box apart from real Clash.
    pub async fn version(&self) -> Result<Version> {
        self.get_json("/version").await
    }

    /// Current mode and log level (`GET /configs`).
    pub async fn configs(&self) -> Result<Configs> {
        self.get_json("/configs").await
    }

    /// Switches the Clash mode (`PATCH /configs`).
    ///
    /// The server does not validate `mode` against [`Configs::mode_list`];
    /// check it first.
    pub async fn set_mode(&self, mode: &str) -> Result<()> {
        if mode.is_empty() {
            return Err(Error::invalid("mode must not be empty"));
        }
        let body = serde_json::to_vec(&serde_json::json!({ "mode": mode }))?;
        self.call(Method::PATCH, "/configs", Some(body)).await
    }
}

// ---------------------------------------------------------------------------
// Proxies and groups
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ProxiesEnvelope {
    proxies: Proxies,
}

#[derive(Deserialize)]
struct GroupsEnvelope {
    #[serde(default, deserialize_with = "null_default")]
    proxies: Vec<Proxy>,
}

#[derive(Deserialize)]
struct DelayEnvelope {
    delay: u64,
}

impl Client {
    /// All outbounds, endpoints and the synthetic `GLOBAL` entry
    /// (`GET /proxies`), in server order.
    pub async fn proxies(&self) -> Result<Proxies> {
        let envelope: ProxiesEnvelope = self.get_json("/proxies").await?;
        Ok(envelope.proxies)
    }

    /// One outbound or group (`GET /proxies/{name}`).
    ///
    /// The synthetic `GLOBAL` entry is not addressable and yields a 404.
    pub async fn proxy(&self, name: &str) -> Result<Proxy> {
        let target = format!("/proxies/{}", segment("proxy name", name)?);
        self.get_json(&target).await
    }

    /// Selects `member` in the Selector group `group` (`PUT /proxies/{group}`).
    ///
    /// The server answers without a body; call [`Client::proxy`] to observe
    /// the new [`Proxy::now`].
    pub async fn select_proxy(&self, group: &str, member: &str) -> Result<()> {
        if member.is_empty() {
            return Err(Error::invalid("member name must not be empty"));
        }
        let target = format!("/proxies/{}", segment("group name", group)?);
        let body = serde_json::to_vec(&serde_json::json!({ "name": member }))?;
        self.call(Method::PUT, &target, Some(body)).await
    }

    /// Measures the latency of one outbound (`GET /proxies/{name}/delay`).
    ///
    /// * `url` must be an `https://` URL (the server silently swaps `http://`
    ///   and empty values for its own default).
    /// * `timeout` must be between 1 ms and 32767 ms.
    ///
    /// A probe that exceeds `timeout` fails with HTTP 504, any other probe
    /// failure with 503. As a side effect the result is stored in
    /// [`Proxy::history`] and URL-test groups may change their selection.
    pub async fn proxy_delay(&self, name: &str, url: &str, timeout: Duration) -> Result<Duration> {
        let name = segment("proxy name", name)?;
        let ms = probe_millis(url, timeout, MAX_PROXY_PROBE_MS)?;
        let target = format!("/proxies/{name}/delay?url={}&timeout={ms}", encode(url));
        let bytes = self
            .execute(Method::GET, &target, None, timeout + self.inner.timeout)
            .await?;
        let reply: DelayEnvelope = serde_json::from_slice(&bytes)?;
        Ok(Duration::from_millis(reply.delay))
    }

    /// All groups (`GET /group`), in configuration order. Excludes `GLOBAL`.
    pub async fn groups(&self) -> Result<Vec<Proxy>> {
        let envelope: GroupsEnvelope = self.get_json("/group").await?;
        Ok(envelope.proxies)
    }

    /// One group (`GET /group/{name}`); 404 if the tag is unknown or not a group.
    pub async fn group(&self, name: &str) -> Result<Proxy> {
        let target = format!("/group/{}", segment("group name", name)?);
        self.get_json(&target).await
    }

    /// Measures the latency of every member of a group in a single call
    /// (`GET /group/{name}/delay`).
    ///
    /// `timeout` must be between 1 ms and `i32::MAX` ms. Members that failed
    /// or did not finish are absent from the map; never assume every member
    /// is a key. Dropping the returned future cancels the probe on the server.
    pub async fn group_delay(
        &self,
        name: &str,
        url: &str,
        timeout: Duration,
    ) -> Result<HashMap<String, Duration>> {
        let name = segment("group name", name)?;
        let ms = probe_millis(url, timeout, MAX_GROUP_PROBE_MS)?;
        let target = format!("/group/{name}/delay?url={}&timeout={ms}", encode(url));
        let bytes = self
            .execute(Method::GET, &target, None, timeout + self.inner.timeout)
            .await?;
        let raw: HashMap<String, u64> = serde_json::from_slice(&bytes)?;
        Ok(raw
            .into_iter()
            .map(|(tag, ms)| (tag, Duration::from_millis(ms)))
            .collect())
    }
}

/// Validates the parameters shared by both delay probes; returns `timeout` in ms.
fn probe_millis(url: &str, timeout: Duration, max_ms: u128) -> Result<u128> {
    let is_https = url
        .get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"));
    if !is_https {
        return Err(Error::invalid("probe url must start with https://"));
    }
    let ms = timeout.as_millis();
    if ms == 0 || ms > max_ms {
        return Err(Error::invalid(format!(
            "probe timeout must be between 1 and {max_ms} milliseconds"
        )));
    }
    Ok(ms)
}

// ---------------------------------------------------------------------------
// Rules, connections, DNS, cache, UI
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RulesEnvelope {
    #[serde(default, deserialize_with = "null_default")]
    rules: Vec<Rule>,
}

impl Client {
    /// Route rules in evaluation order (`GET /rules`). Empty if none exist.
    pub async fn rules(&self) -> Result<Vec<Rule>> {
        let envelope: RulesEnvelope = self.get_json("/rules").await?;
        Ok(envelope.rules)
    }

    /// One snapshot of all tracked connections (`GET /connections`).
    pub async fn connections(&self) -> Result<Snapshot> {
        self.get_json("/connections").await
    }

    /// A periodic feed of connection snapshots; see [`ConnectionsFeed`].
    ///
    /// `interval` must be greater than zero.
    pub fn connections_feed(&self, interval: Duration) -> Result<ConnectionsFeed> {
        ConnectionsFeed::new(self.clone(), interval)
    }

    /// Closes every connection (`DELETE /connections`).
    ///
    /// This also resets sing-box's network state, which is broader than
    /// closing connections and platform dependent.
    pub async fn close_all_connections(&self) -> Result<()> {
        self.call(Method::DELETE, "/connections", None).await
    }

    /// Closes one connection (`DELETE /connections/{id}`).
    ///
    /// The server answers success even for unknown or malformed IDs, so `Ok`
    /// does not prove that anything was closed.
    pub async fn close_connection(&self, id: &str) -> Result<()> {
        let target = format!("/connections/{}", segment("connection id", id)?);
        self.call(Method::DELETE, &target, None).await
    }

    /// Resolves `name` through sing-box's own DNS router (`GET /dns/query`).
    ///
    /// `record_type` is a record type mnemonic in **uppercase** (`"A"`,
    /// `"AAAA"`, `"TXT"`, ...). NXDOMAIN and similar are reported in
    /// [`DnsAnswer::status`], not as an error.
    pub async fn dns_query(&self, name: &str, record_type: &str) -> Result<DnsAnswer> {
        if record_type.is_empty() {
            return Err(Error::invalid("record type must not be empty"));
        }
        let target = format!("/dns/query?name={}&type={}", encode(name), encode(record_type));
        self.get_json(&target).await
    }

    /// Resets persisted FakeIP mappings (`POST /cache/fakeip/flush`).
    ///
    /// Silently does nothing if no cache file is configured. Existing
    /// connections keep their fake addresses.
    pub async fn flush_fakeip_cache(&self) -> Result<()> {
        self.call(Method::POST, "/cache/fakeip/flush", None).await
    }

    /// Clears the in-memory DNS cache (`POST /cache/dns/flush`).
    pub async fn flush_dns_cache(&self) -> Result<()> {
        self.call(Method::POST, "/cache/dns/flush", None).await
    }

    /// Re-downloads the external UI archive (`POST /upgrade/ui`).
    ///
    /// Slow and synchronous (limit: 5 minutes). If unpacking fails on the
    /// server, the UI directory is left empty. Fails with 404 if no
    /// `external_ui` is configured.
    pub async fn upgrade_ui(&self) -> Result<()> {
        self.execute(Method::POST, "/upgrade/ui", None, UPGRADE_UI_TIMEOUT)
            .await
            .map(drop)
    }
}

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

impl Client {
    /// Live upload/download speed in bytes per second (`GET /traffic`).
    ///
    /// The future resolves when the server sends the response headers; the
    /// first event follows about a second later.
    pub async fn traffic(&self) -> Result<EventStream<Traffic>> {
        self.open_stream("/traffic").await
    }

    /// Live memory usage, one event per second (`GET /memory`).
    ///
    /// The first event always reports `inuse == 0`; discard it for numeric use.
    pub async fn memory(&self) -> Result<EventStream<Memory>> {
        self.open_stream("/memory").await
    }

    /// Live log events at `level` or more severe (`GET /logs`).
    ///
    /// The server may send nothing, headers included, until the first log
    /// line is produced, so this future can stay pending on a quiet server;
    /// wrap it in [`tokio::time::timeout`] if that matters. Events below the
    /// core's own `log.level` are never emitted.
    pub async fn logs(&self, level: LogLevel) -> Result<EventStream<LogEvent>> {
        self.open_stream(&format!("/logs?level={}", level.as_str())).await
    }
}

// ---------------------------------------------------------------------------
// Transport internals
// ---------------------------------------------------------------------------

impl Client {
    async fn send(&self, method: Method, target: &str, body: Option<Vec<u8>>) -> Result<Response<Incoming>> {
        let uri: Uri = format!("http://{}{target}", self.inner.authority)
            .parse()
            .map_err(|err| Error::invalid(format!("invalid request target: {err}")))?;
        // The server redirects `GET /` to the UI unless this header is present.
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(CONTENT_TYPE, "application/json");
        if let Some(auth) = &self.inner.auth {
            builder = builder.header(AUTHORIZATION, auth.clone());
        }
        let request = builder
            .body(Full::new(body.map(Bytes::from).unwrap_or_default()))
            .map_err(Error::transport)?;
        self.inner.http.request(request).await.map_err(Error::transport)
    }

    /// Sends a request and returns the body of a 2xx answer.
    async fn execute(
        &self,
        method: Method,
        target: &str,
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<Bytes> {
        let work = async {
            let (parts, body) = self.send(method, target, body).await?.into_parts();
            let bytes = body.collect().await.map_err(Error::transport)?.to_bytes();
            if parts.status.is_success() {
                Ok(bytes)
            } else {
                Err(api_error(parts.status, &bytes))
            }
        };
        tokio::time::timeout(timeout, work)
            .await
            .map_err(|_| Error::Timeout)?
    }

    async fn call(&self, method: Method, target: &str, body: Option<Vec<u8>>) -> Result<()> {
        self.execute(method, target, body, self.inner.timeout)
            .await
            .map(drop)
    }

    async fn get_json<T: DeserializeOwned>(&self, target: &str) -> Result<T> {
        let bytes = self
            .execute(Method::GET, target, None, self.inner.timeout)
            .await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn open_stream<T: DeserializeOwned>(&self, target: &str) -> Result<EventStream<T>> {
        let (parts, body) = self.send(Method::GET, target, None).await?.into_parts();
        if parts.status != StatusCode::OK {
            // Includes the 204 the server sends when log streaming is unavailable.
            let bytes = tokio::time::timeout(self.inner.timeout, body.collect())
                .await
                .map_err(|_| Error::Timeout)?
                .map_err(Error::transport)?
                .to_bytes();
            return Err(api_error(parts.status, &bytes));
        }
        Ok(EventStream::new(body))
    }
}

/// Percent-encodes a path segment and rejects empty values, which would
/// otherwise silently address a different route.
fn segment(what: &str, value: &str) -> Result<String> {
    if value.is_empty() {
        return Err(Error::invalid(format!("{what} must not be empty")));
    }
    Ok(encode(value))
}

/// Converts a non-success answer into [`Error::Api`].
fn api_error(status: StatusCode, body: &[u8]) -> Error {
    #[derive(Deserialize)]
    struct ErrorBody {
        message: String,
    }

    let message = match serde_json::from_slice::<ErrorBody>(body) {
        Ok(parsed) => parsed.message,
        Err(_) => {
            let text = String::from_utf8_lossy(body);
            let text = text.trim();
            if text.is_empty() {
                status.canonical_reason().unwrap_or("unknown error").to_owned()
            } else {
                text.chars().take(MAX_ERROR_TEXT).collect()
            }
        }
    };
    Error::Api {
        status: status.as_u16(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_forms() {
        assert_eq!(parse_authority("127.0.0.1:9090").unwrap(), "127.0.0.1:9090");
        assert_eq!(parse_authority("http://127.0.0.1:9090/").unwrap(), "127.0.0.1:9090");
        assert_eq!(parse_authority("[::1]:9090").unwrap(), "[::1]:9090");
        assert_eq!(parse_authority("localhost").unwrap(), "localhost");
    }

    #[test]
    fn authority_rejections() {
        for bad in ["", "https://h:1", "h:1/path", "user@h:1", ":9090", "h:port"] {
            assert!(parse_authority(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn probe_validation() {
        let ok = Duration::from_secs(5);
        assert_eq!(probe_millis("https://x.test/204", ok, MAX_PROXY_PROBE_MS).unwrap(), 5000);
        assert!(probe_millis("http://x.test", ok, MAX_PROXY_PROBE_MS).is_err());
        assert!(probe_millis("", ok, MAX_PROXY_PROBE_MS).is_err());
        assert!(probe_millis("https://x.test", Duration::ZERO, MAX_PROXY_PROBE_MS).is_err());
        assert!(probe_millis("https://x.test", Duration::from_millis(32768), MAX_PROXY_PROBE_MS).is_err());
        assert!(probe_millis("https://x.test", Duration::from_millis(32767), MAX_PROXY_PROBE_MS).is_ok());
    }

    #[test]
    fn api_error_message_sources() {
        let json = api_error(StatusCode::NOT_FOUND, br#"{"message":"Resource not found"}"#);
        assert!(matches!(&json, Error::Api { status: 404, message } if message == "Resource not found"));
        let text = api_error(StatusCode::METHOD_NOT_ALLOWED, b"  nope \n");
        assert!(matches!(&text, Error::Api { status: 405, message } if message == "nope"));
        let empty = api_error(StatusCode::NOT_FOUND, b"");
        assert!(matches!(&empty, Error::Api { status: 404, message } if message == "Not Found"));
    }
}
