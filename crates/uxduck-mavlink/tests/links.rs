//! The UDP client and TCP server transports: a UDP client says hello to a
//! server that is waiting to hear from someone, and a TCP server keeps each
//! connection as its own peer.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "a test")]

use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio::time::timeout;
use uxduck_mavlink::links::{self, LinkEvent, Outgoing};

const WAIT: Duration = Duration::from_secs(3);

async fn next(events: &mut mpsc::Receiver<links::TimedEvent>) -> LinkEvent {
    timeout(WAIT, events.recv())
        .await
        .expect("an event")
        .expect("open")
        .event
}

async fn received(events: &mut mpsc::Receiver<links::TimedEvent>) -> (String, Vec<u8>) {
    loop {
        if let LinkEvent::Received { peer, bytes } = next(events).await {
            return (peer, bytes);
        }
    }
}

#[tokio::test]
async fn a_udp_client_says_hello_and_hears_its_server() {
    // A server that sends only to whoever spoke to it.
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (tx, mut events) = links::event_channel();
    // The hello comes from the ground station id the daemon is set to.
    let gcs = Arc::new(AtomicU8::new(7));
    let link = links::udp_client(server.local_addr().unwrap(), gcs, tx)
        .await
        .unwrap();
    assert_eq!(next(&mut events).await, LinkEvent::Up);

    let mut buffer = [0u8; 512];
    let (len, client) = timeout(WAIT, server.recv_from(&mut buffer))
        .await
        .expect("a hello")
        .unwrap();
    let hello = uxduck_mavlink::framing::header(&buffer[..len]).expect("a MAVLink frame");
    assert_eq!(hello.system_id, 7);

    server.send_to(b"from the server", client).await.unwrap();
    let (peer, bytes) = received(&mut events).await;
    assert_eq!(peer, server.local_addr().unwrap().to_string());
    assert_eq!(bytes, b"from the server");

    // What the link sends reaches the server.
    link.send(Outgoing {
        peer: Some(peer),
        bytes: b"to the server".to_vec(),
    });
    let (len, _) = timeout(WAIT, server.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buffer[..len], b"to the server");
}

#[tokio::test]
async fn a_udp_client_stops_saying_hello_while_it_hears_the_server() {
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (tx, mut events) = links::event_channel();
    let gcs = Arc::new(AtomicU8::new(uxduck_mavlink::GCS_SYSTEM_ID));
    let _link = links::udp_client(server.local_addr().unwrap(), gcs, tx)
        .await
        .unwrap();
    let mut buffer = [0u8; 512];
    let (_, client) = timeout(WAIT, server.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    // The server keeps talking for two seconds; no second hello comes.
    for _ in 0..8 {
        server.send_to(b"telemetry", client).await.unwrap();
        let _ = received(&mut events).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let quiet = timeout(Duration::from_millis(100), server.recv_from(&mut buffer)).await;
    assert!(quiet.is_err(), "no hello while the server is heard");
}

#[tokio::test]
async fn a_tcp_server_keeps_each_connection_as_a_peer() {
    let (tx, mut events) = links::event_channel();
    // A free port, found by binding and letting go.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = probe.local_addr().unwrap();
    drop(probe);
    let link = links::tcp_server(address, tx).await.unwrap();
    assert_eq!(next(&mut events).await, LinkEvent::Up);

    let mut first = TcpStream::connect(address).await.unwrap();
    let mut second = TcpStream::connect(address).await.unwrap();
    first.write_all(b"one").await.unwrap();
    let (first_peer, bytes) = received(&mut events).await;
    assert_eq!(bytes, b"one");
    second.write_all(b"two").await.unwrap();
    let (second_peer, bytes) = received(&mut events).await;
    assert_eq!(bytes, b"two");
    assert_ne!(first_peer, second_peer);
    assert_eq!(first_peer, first.local_addr().unwrap().to_string());

    // To one peer reaches only that one; to no peer reaches both.
    link.send(Outgoing {
        peer: Some(second_peer),
        bytes: b"for two".to_vec(),
    });
    link.send(Outgoing {
        peer: None,
        bytes: b"all".to_vec(),
    });
    let mut buffer = [0u8; 16];
    let len = timeout(WAIT, first.read(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buffer[..len], b"all");
    let mut got = Vec::new();
    while got.len() < b"for twoall".len() {
        let len = timeout(WAIT, second.read(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        got.extend_from_slice(&buffer[..len]);
    }
    assert_eq!(got, b"for twoall");

    // A closed connection is let go; the others carry on.
    drop(first);
    tokio::time::sleep(Duration::from_millis(100)).await;
    second.write_all(b"still").await.unwrap();
    let (_, bytes) = received(&mut events).await;
    assert_eq!(bytes, b"still");
}

#[tokio::test]
async fn a_tcp_server_reports_a_port_in_use() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let (tx, _events) = links::event_channel();
    let error = links::tcp_server(held.local_addr().unwrap(), tx)
        .await
        .expect_err("the port is held");
    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
}

#[tokio::test]
async fn tcp_backpressure_preserves_the_entire_stream() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, mut events) = links::event_channel();
    let _link = links::tcp_client(address.ip().to_string(), address.port(), tx);
    let (mut server, _) = listener.accept().await.unwrap();
    assert_eq!(next(&mut events).await, LinkEvent::Up);
    let expected: Vec<u8> = (0u8..=255)
        .cycle()
        .take(4096 * (links::EVENT_CAPACITY + 16))
        .collect();
    let to_send = expected.clone();
    let writer = tokio::spawn(async move {
        server.write_all(&to_send).await.unwrap();
    });
    timeout(WAIT, async {
        while events.len() < links::EVENT_CAPACITY {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the queue fills without being drained");
    assert_eq!(events.len(), links::EVENT_CAPACITY);
    let mut actual = Vec::new();
    while actual.len() < expected.len() {
        let (_, bytes) = received(&mut events).await;
        actual.extend(bytes);
    }
    writer.await.unwrap();
    assert_eq!(actual, expected, "backpressure must not drop stream chunks");
}
