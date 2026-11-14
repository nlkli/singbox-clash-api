# singbox-clash-api

Async Rust client for the Clash-compatible REST API of [sing-box](https://sing-box.sagernet.org/)
(`experimental.clash_api`).

- Typed methods for every working endpoint: version, configs, proxies, groups,
  delay probes, rules, connections, DNS queries, cache flush, UI upgrade.
- Live streams for traffic, memory and logs.
- Tokio-based, with a pooled HTTP/1.1 connection (keep-alive).
- Small dependency tree: `tokio` (timers), `hyper`, `hyper-util`,
  `http-body-util`, `serde`, `serde_json`. No TLS, no WebSocket stack.

## Installation

```toml
[dependencies]
singbox-clash-api = "0.1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

## Usage

```rust
use std::time::Duration;
use singbox_clash_api::{Client, LogLevel};

#[tokio::main]
async fn main() -> singbox_clash_api::Result<()> {
    let client = Client::builder("127.0.0.1:9090").secret("s3cret").build()?;

    println!("sing-box {}", client.version().await?.number());

    // Groups and selection.
    for group in client.groups().await? {
        println!("{}: {:?}", group.name, group.current());
    }
    client.select_proxy("Proxy", "hk-01").await?;

    // Latency probe (https URL, 1..=32767 ms).
    let delay = client
        .proxy_delay("hk-01", "https://www.gstatic.com/generate_204", Duration::from_secs(5))
        .await?;
    println!("{} ms", delay.as_millis());

    // Live traffic.
    let mut traffic = client.traffic().await?;
    while let Some(event) = traffic.next().await {
        let t = event?;
        println!("up {} B/s, down {} B/s", t.up, t.down);
    }
    Ok(())
}
```

## Notes

- `Client` is cheap to clone; clones share the connection pool.
- Streams end when the server drops the connection and are never resumed
  automatically: reconnect in your own loop.
- `/connections` is only streamable over WebSocket on the server, so
  `Client::connections_feed` polls the snapshot endpoint instead.
- Stub endpoints (`/providers/*`, `/script`, `PUT /configs`, `/profile/tracing`)
  do nothing in sing-box and are intentionally not exposed.
- Validation that the server would do badly (empty names, `http://` probe URLs,
  out-of-range timeouts) happens locally and returns `Error::InvalidInput`.

## License

MIT OR Apache-2.0
