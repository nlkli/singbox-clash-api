//! Data types returned by the API.
//!
//! All structs are tolerant: unknown JSON fields are ignored, missing fields
//! fall back to defaults, and `null` collections become empty ones.

use std::fmt;
use std::slice;

use serde::de::{Deserializer, MapAccess, Visitor};
use serde::{Serialize, Deserialize};

/// Deserializes `null` as `T::default()` (Go encodes nil slices as `null`).
pub(crate) fn null_default<'de, D, T>(de: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(de)?.unwrap_or_default())
}

/// Response of `GET /version`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Version {
    /// Full version string, e.g. `"sing-box 1.12.0"`.
    pub version: String,
    /// Always `true` on sing-box (makes Clash dashboards enable extras).
    pub premium: bool,
    /// Always `true` on sing-box.
    pub meta: bool,
}

impl Version {
    /// `true` if the server identifies itself as sing-box rather than Clash.
    pub fn is_sing_box(&self) -> bool {
        self.version.starts_with("sing-box ")
    }

    /// The bare build version without the `sing-box ` prefix.
    pub fn number(&self) -> &str {
        self.version.strip_prefix("sing-box ").unwrap_or(&self.version)
    }
}

/// The meaningful part of `GET /configs`.
///
/// The server also reports ports, `allow-lan`, `ipv6` and `tun`, but those are
/// constants and are intentionally not exposed.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Configs {
    /// Current Clash mode, e.g. `"rule"`.
    pub mode: String,
    /// Selectable modes, from the server configuration.
    #[serde(rename = "mode-list", deserialize_with = "null_default")]
    pub mode_list: Vec<String>,
    /// Core log level, normalized by the server.
    #[serde(rename = "log-level")]
    pub log_level: String,
}

/// Minimum severity for [`Client::logs`](crate::Client::logs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LogLevel {
    /// `trace`
    Trace,
    /// `debug`
    Debug,
    /// `info` (server default)
    #[default]
    Info,
    /// `warn`
    Warn,
    /// `error`
    Error,
    /// `fatal`
    Fatal,
    /// `panic`
    Panic,
}

impl LogLevel {
    /// The wire representation of the level.
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
            LogLevel::Fatal => "fatal",
            LogLevel::Panic => "panic",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One event of the `/logs` stream.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LogEvent {
    /// Level name of the event (`"info"`, `"warn"`, ...).
    #[serde(rename = "type")]
    pub level: String,
    /// Formatted log message.
    pub payload: String,
}

/// One event of the `/traffic` stream: bytes transferred since the previous
/// tick (about one second), not cumulative totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Traffic {
    /// Uploaded bytes since the previous event.
    pub up: u64,
    /// Downloaded bytes since the previous event.
    pub down: u64,
}

/// One event of the `/memory` stream.
///
/// The first event of every stream always has `inuse == 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Memory {
    /// Go runtime memory in bytes (not the process RSS).
    pub inuse: u64,
    /// Always `0`.
    pub oslimit: u64,
}

/// Latest URL-test result of an outbound.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DelayRecord {
    /// RFC 3339 timestamp with nanoseconds.
    pub time: String,
    /// Latency in milliseconds.
    pub delay: u64,
}

/// An outbound or a group of outbounds.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Proxy {
    /// Display name of the outbound type (`"Selector"`, `"Shadowsocks"`, ...).
    /// Do not use it to detect groups; see [`Proxy::is_group`].
    #[serde(rename = "type")]
    pub kind: String,
    /// Outbound tag.
    pub name: String,
    /// Whether the outbound supports UDP.
    pub udp: bool,
    /// Zero or one entry with the latest URL-test result.
    #[serde(deserialize_with = "null_default")]
    pub history: Vec<DelayRecord>,
    /// Groups only: the selected member; `Some("")` if none.
    pub now: Option<String>,
    /// Groups only: member tags in configuration order.
    pub all: Option<Vec<String>>,
}

impl Proxy {
    /// `true` if this entry is a group (it carries a member list).
    pub fn is_group(&self) -> bool {
        self.all.is_some()
    }

    /// Group members; empty for plain outbounds.
    pub fn members(&self) -> &[String] {
        self.all.as_deref().unwrap_or(&[])
    }

    /// The currently selected member of a group, if any.
    pub fn current(&self) -> Option<&str> {
        self.now.as_deref().filter(|s| !s.is_empty())
    }

    /// The most recent measured delay in milliseconds, if any.
    pub fn last_delay(&self) -> Option<u64> {
        self.history.last().map(|h| h.delay)
    }
}

/// Insertion-ordered collection of proxies from `GET /proxies`.
///
/// The server emits `GLOBAL` first, then outbounds in configuration order,
/// then endpoints; this type preserves that order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Proxies {
    entries: Vec<(String, Proxy)>,
}

impl Proxies {
    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` if there are no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Looks a proxy up by tag (linear scan).
    pub fn get(&self, name: &str) -> Option<&Proxy> {
        self.entries.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    /// Iterates over `(tag, proxy)` pairs in server order.
    pub fn iter(&self) -> slice::Iter<'_, (String, Proxy)> {
        self.entries.iter()
    }

    /// Iterates over the groups only.
    pub fn groups(&self) -> impl Iterator<Item = &Proxy> {
        self.entries.iter().map(|(_, p)| p).filter(|p| p.is_group())
    }
}

impl<'a> IntoIterator for &'a Proxies {
    type Item = &'a (String, Proxy);
    type IntoIter = slice::Iter<'a, (String, Proxy)>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl IntoIterator for Proxies {
    type Item = (String, Proxy);
    type IntoIter = std::vec::IntoIter<(String, Proxy)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl<'de> Deserialize<'de> for Proxies {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct OrderedMap;

        impl<'de> Visitor<'de> for OrderedMap {
            type Value = Proxies;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a map of proxy tags to proxies")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Proxies, A::Error> {
                let mut entries = Vec::with_capacity(map.size_hint().unwrap_or(0).min(4096));
                while let Some(entry) = map.next_entry::<String, Proxy>()? {
                    entries.push(entry);
                }
                Ok(Proxies { entries })
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Proxies, E> {
                Ok(Proxies::default())
            }
        }

        de.deserialize_map(OrderedMap)
    }
}

/// One routing rule from `GET /rules`.
///
/// Both strings are display text produced by sing-box; do not parse them as
/// Clash `TYPE,payload,proxy` triples.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Rule {
    /// Rule type name.
    #[serde(rename = "type")]
    pub kind: String,
    /// Textual description of the rule's matchers.
    pub payload: String,
    /// Textual description of the rule's action (not always an outbound tag).
    pub proxy: String,
}

/// Connection metadata.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Metadata {
    /// Transport, e.g. `"tcp"` or `"udp"`.
    pub network: String,
    /// `<inbound type>/<inbound tag>`, or just the type for untagged inbounds.
    #[serde(rename = "type")]
    pub kind: String,
    /// Source address; empty when unknown.
    #[serde(rename = "sourceIP")]
    pub source_ip: String,
    /// Destination address; empty for an unresolved domain destination.
    #[serde(rename = "destinationIP")]
    pub destination_ip: String,
    /// Source port (a string on the wire).
    #[serde(rename = "sourcePort")]
    pub source_port: String,
    /// Destination port (a string on the wire).
    #[serde(rename = "destinationPort")]
    pub destination_port: String,
    /// Sniffed or routed domain, possibly empty.
    pub host: String,
    /// Always `"normal"`.
    #[serde(rename = "dnsMode")]
    pub dns_mode: String,
    /// Process path (with a user suffix), Android package, or user ID.
    #[serde(rename = "processPath")]
    pub process_path: String,
}

/// A tracked connection.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Connection {
    /// UUID; pass it to [`Client::close_connection`](crate::Client::close_connection).
    pub id: String,
    /// Addresses, ports, host and process information.
    pub metadata: Metadata,
    /// Bytes uploaded so far.
    pub upload: u64,
    /// Bytes downloaded so far.
    pub download: u64,
    /// RFC 3339 creation time with nanoseconds.
    pub start: String,
    /// Outbound tags the connection passed through.
    #[serde(deserialize_with = "null_default")]
    pub chains: Vec<String>,
    /// `"<rule payload> => <action>"`, or `"final"` if no rule matched.
    pub rule: String,
    /// Always empty.
    #[serde(rename = "rulePayload")]
    pub rule_payload: String,
}

/// Snapshot of all tracked connections.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    /// Cumulative downloaded bytes since sing-box started (includes closed
    /// connections).
    #[serde(rename = "downloadTotal")]
    pub download_total: u64,
    /// Cumulative uploaded bytes since sing-box started.
    #[serde(rename = "uploadTotal")]
    pub upload_total: u64,
    /// Same measure as [`Memory::inuse`].
    pub memory: u64,
    /// Live connections; empty if there are none.
    #[serde(deserialize_with = "null_default")]
    pub connections: Vec<Connection>,
}

/// The question section of a DNS answer.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DnsQuestion {
    /// Fully qualified name.
    #[serde(rename = "Name")]
    pub name: String,
    /// Numeric query type.
    #[serde(rename = "Qtype")]
    pub qtype: u16,
    /// Numeric query class.
    #[serde(rename = "Qclass")]
    pub qclass: u16,
}

/// A resource record in a DNS answer.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DnsRecord {
    /// Owner name.
    pub name: String,
    /// Numeric RR type.
    #[serde(rename = "type")]
    pub kind: u16,
    /// Time to live in seconds.
    #[serde(rename = "TTL")]
    pub ttl: u32,
    /// Record body in zone-file text form (without name, TTL, class, type).
    pub data: String,
}

/// Response of `GET /dns/query`.
///
/// A DNS-level failure such as NXDOMAIN is still a successful HTTP response;
/// inspect [`DnsAnswer::status`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DnsAnswer {
    /// DNS response code (0 NOERROR, 2 SERVFAIL, 3 NXDOMAIN).
    #[serde(rename = "Status")]
    pub status: u32,
    /// Echo of the question.
    #[serde(rename = "Question", deserialize_with = "null_default")]
    pub question: Vec<DnsQuestion>,
    /// Always `"internal"`.
    #[serde(rename = "Server")]
    pub server: String,
    /// Truncated (`TC`) flag.
    #[serde(rename = "TC")]
    pub truncated: bool,
    /// Recursion desired (`RD`) flag.
    #[serde(rename = "RD")]
    pub recursion_desired: bool,
    /// Recursion available (`RA`) flag.
    #[serde(rename = "RA")]
    pub recursion_available: bool,
    /// Authenticated data (`AD`) flag.
    #[serde(rename = "AD")]
    pub authenticated_data: bool,
    /// Checking disabled (`CD`) flag.
    #[serde(rename = "CD")]
    pub checking_disabled: bool,
    /// Answer section; empty if omitted by the server.
    #[serde(rename = "Answer", deserialize_with = "null_default")]
    pub answer: Vec<DnsRecord>,
    /// Authority section; empty if omitted by the server.
    #[serde(rename = "Authority", deserialize_with = "null_default")]
    pub authority: Vec<DnsRecord>,
    /// Additional section; empty if omitted by the server.
    #[serde(rename = "Additional", deserialize_with = "null_default")]
    pub additional: Vec<DnsRecord>,
}
