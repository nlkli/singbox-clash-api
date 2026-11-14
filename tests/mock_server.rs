//! End-to-end tests against a tiny in-process HTTP/1.1 server.

use std::time::Duration;

use singbox_clash_api::{Client, Error, LogLevel};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// Serves one canned raw HTTP response per accepted connection and reports
/// the request head (request line + headers) of each connection.
async fn serve(responses: Vec<Vec<u8>>) -> (String, mpsc::UnboundedReceiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        for response in responses {
            let (mut sock, _) = listener.accept().await.unwrap();
            let head = read_head(&mut sock).await;
            let _ = tx.send(head);
            sock.write_all(&response).await.unwrap();
            let _ = sock.shutdown().await;
        }
    });
    (addr, rx)
}

async fn read_head(sock: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = sock.read(&mut chunk).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn http(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

#[tokio::test]
async fn version_sends_bearer_and_parses() {
    let (addr, mut heads) = serve(vec![http(
        "200 OK",
        r#"{"version":"sing-box 1.12.0","premium":true,"meta":true}"#,
    )])
    .await;
    let client = Client::builder(&addr).secret("s3cret").build().unwrap();
    let v = client.version().await.unwrap();
    assert!(v.is_sing_box());
    assert_eq!(v.number(), "1.12.0");

    let head = heads.recv().await.unwrap().to_ascii_lowercase();
    assert!(head.starts_with("get /version http/1.1"));
    assert!(head.contains("authorization: bearer s3cret"));
    assert!(head.contains("content-type: application/json"));
}

#[tokio::test]
async fn unauthorized_maps_to_api_error() {
    let (addr, _) = serve(vec![http("401 Unauthorized", r#"{"message":"Unauthorized"}"#)]).await;
    let err = Client::new(&addr).unwrap().ping().await.unwrap_err();
    assert!(err.is_unauthorized());
    assert!(matches!(err, Error::Api { status: 401, ref message } if message == "Unauthorized"));
}

#[tokio::test]
async fn proxies_keep_server_order_and_detect_groups() {
    let body = r#"{"proxies":{
        "GLOBAL":{"type":"Fallback","name":"GLOBAL","udp":true,"history":[],"now":"Proxy","all":["Proxy","a"]},
        "2":{"type":"Shadowsocks","name":"2","udp":true,"history":[{"time":"t","delay":42}]},
        "Proxy":{"type":"Selector","name":"Proxy","udp":true,"history":null,"now":"","all":["2"]},
        "direct":{"type":"Direct","name":"direct","udp":true,"history":[]}}}"#;
    let (addr, _) = serve(vec![http("200 OK", body)]).await;
    let proxies = Client::new(&addr).unwrap().proxies().await.unwrap();

    let order: Vec<&str> = proxies.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(order, ["GLOBAL", "2", "Proxy", "direct"]);
    assert_eq!(proxies.groups().count(), 2);
    assert_eq!(proxies.get("2").unwrap().last_delay(), Some(42));
    let sel = proxies.get("Proxy").unwrap();
    assert!(sel.is_group() && sel.current().is_none() && sel.history.is_empty());
}

#[tokio::test]
async fn null_collections_become_empty() {
    let (addr, _) = serve(vec![
        http("200 OK", r#"{"rules":null}"#),
        http("200 OK", r#"{"downloadTotal":5,"uploadTotal":1,"memory":9,"connections":null}"#),
    ])
    .await;
    let client = Client::new(&addr).unwrap();
    assert!(client.rules().await.unwrap().is_empty());
    let snap = client.connections().await.unwrap();
    assert_eq!((snap.download_total, snap.upload_total), (5, 1));
    assert!(snap.connections.is_empty());
}

#[tokio::test]
async fn select_proxy_encodes_path_and_sends_body() {
    let (addr, mut heads) = serve(vec![b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_vec()]).await;
    Client::new(&addr).unwrap().select_proxy("a/b c", "hk-01").await.unwrap();
    let head = heads.recv().await.unwrap();
    assert!(head.starts_with("PUT /proxies/a%2Fb%20c HTTP/1.1"), "{head}");
}

#[tokio::test]
async fn delay_builds_query_and_returns_duration() {
    let (addr, mut heads) = serve(vec![http("200 OK", r#"{"delay":123}"#)]).await;
    let d = Client::new(&addr)
        .unwrap()
        .proxy_delay("hk", "https://x.test/204", Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(d, Duration::from_millis(123));
    let head = heads.recv().await.unwrap();
    assert!(
        head.starts_with("GET /proxies/hk/delay?url=https%3A%2F%2Fx.test%2F204&timeout=5000 HTTP/1.1"),
        "{head}"
    );
}

#[tokio::test]
async fn group_delay_maps_to_durations() {
    let (addr, _) = serve(vec![http("200 OK", r#"{"a":10,"b":20}"#)]).await;
    let map = Client::new(&addr)
        .unwrap()
        .group_delay("g", "https://x.test", Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(map["a"], Duration::from_millis(10));
    assert_eq!(map.len(), 2);
}

#[tokio::test]
async fn invalid_input_never_hits_the_network() {
    // Nothing listens here; validation must fail before any connection.
    let client = Client::new("127.0.0.1:1").unwrap();
    assert!(matches!(client.proxy("").await, Err(Error::InvalidInput(_))));
    assert!(matches!(client.set_mode("").await, Err(Error::InvalidInput(_))));
    assert!(matches!(
        client.proxy_delay("a", "http://x", Duration::from_secs(1)).await,
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        client.connections_feed(Duration::ZERO),
        Err(Error::InvalidInput(_))
    ));
}

#[tokio::test]
async fn dns_query_decodes_optional_sections() {
    let body = r#"{"Status":3,"Question":[{"Name":"x.test.","Qtype":1,"Qclass":1}],"Server":"internal",
        "TC":false,"RD":true,"RA":true,"AD":false,"CD":false}"#;
    let (addr, mut heads) = serve(vec![http("200 OK", body)]).await;
    let ans = Client::new(&addr).unwrap().dns_query("x.test", "A").await.unwrap();
    assert_eq!(ans.status, 3);
    assert_eq!(ans.question[0].name, "x.test.");
    assert!(ans.recursion_desired && ans.answer.is_empty());
    assert!(heads.recv().await.unwrap().starts_with("GET /dns/query?name=x.test&type=A "));
}

#[tokio::test]
async fn traffic_stream_handles_split_chunks_and_missing_newlines() {
    // Chunked body whose chunk boundaries fall inside JSON values.
    let chunks = ["{\"up\":1,\"do", "wn\":2}\n{\"up\":3,\"down\":4}{\"up\":5,", "\"down\":6}\n"];
    let mut raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    for c in chunks {
        raw.extend_from_slice(format!("{:x}\r\n{c}\r\n", c.len()).as_bytes());
    }
    raw.extend_from_slice(b"0\r\n\r\n");
    let (addr, _) = serve(vec![raw]).await;

    let mut stream = Client::new(&addr).unwrap().traffic().await.unwrap();
    let mut seen = Vec::new();
    while let Some(ev) = stream.next().await {
        let t = ev.unwrap();
        seen.push((t.up, t.down));
    }
    assert_eq!(seen, [(1, 2), (3, 4), (5, 6)]);
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn truncated_stream_reports_error_then_ends() {
    let mut raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    let c = "{\"up\":1,\"down\":2}\n";
    raw.extend_from_slice(format!("{:x}\r\n{c}\r\n", c.len()).as_bytes());
    // Connection closes without the terminating zero chunk.
    let (addr, _) = serve(vec![raw]).await;

    let mut stream = Client::new(&addr).unwrap().traffic().await.unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap().up, 1);
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn logs_error_status_and_204() {
    let (addr, mut heads) = serve(vec![
        http("400 Bad Request", r#"{"message":"Body invalid"}"#),
        b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_vec(),
    ])
    .await;
    let client = Client::new(&addr).unwrap();
    let err = client.logs(LogLevel::Warn).await.unwrap_err();
    assert_eq!(err.status(), Some(400));
    assert!(heads.recv().await.unwrap().starts_with("GET /logs?level=warn "));
    assert_eq!(client.logs(LogLevel::Info).await.unwrap_err().status(), Some(204));
}

#[tokio::test]
async fn request_timeout_is_reported() {
    // Accept but never answer.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let (_sock, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let client = Client::builder(&addr).timeout(Duration::from_millis(150)).build().unwrap();
    assert!(matches!(client.version().await, Err(Error::Timeout)));
}

#[tokio::test]
async fn connections_feed_polls_immediately_then_periodically() {
    let body = r#"{"downloadTotal":1,"uploadTotal":2,"memory":3,"connections":[]}"#;
    let (addr, _) = serve(vec![http("200 OK", body), http("200 OK", body)]).await;
    let mut feed = Client::new(&addr).unwrap().connections_feed(Duration::from_millis(50)).unwrap();
    let started = std::time::Instant::now();
    feed.next().await.unwrap();
    assert!(started.elapsed() < Duration::from_millis(40), "first snapshot must be immediate");
    feed.next().await.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(45));
}

#[test]
fn builder_rejects_bad_input_and_hides_secret() {
    assert!(Client::new("https://h:1").is_err());
    assert!(Client::builder("h:1").secret("bad\nsecret").build().is_err());
    assert!(Client::builder("h:1").timeout(Duration::ZERO).build().is_err());
    let dbg = format!("{:?}", Client::builder("h:1").secret("topsecret"));
    assert!(!dbg.contains("topsecret"));
}
