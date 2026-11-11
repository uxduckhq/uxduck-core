use super::replay;
use uxduck_mavlink::{decode, sim, tlog};

const COMMAND: &[u8] = &[
    253, 32, 0, 0, 0, 200, 1, 76, 0, 0, 0, 0, 128, 63, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 144, 1, 1, 1, 90, 22,
];

#[tokio::test]
async fn udp_replay_sends_telemetry_but_never_an_unidentified_senders_command() {
    let decoded = decode::decode(COMMAND).unwrap();
    assert_eq!((decoded.system_id, decoded.component_id), (200, 1));
    assert!(matches!(
        decoded.message,
        decode::Message::CommandLong { .. }
    ));
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("synthetic.tlog");
    let heartbeat = sim::heartbeat_frame(1, 1, 0);
    let position = sim::global_position_at_frame((1, 0), (0, 0), (0, 0), 1_000);
    let mut log = Vec::new();
    for frame in [heartbeat.as_slice(), COMMAND, position.as_slice()] {
        tlog::record(&mut log, 1_790_000_000_000_000, frame).unwrap();
    }
    std::fs::write(&path, log).unwrap();
    replay(&path, &socket.local_addr().unwrap().to_string(), 1.0)
        .await
        .unwrap();
    let mut buf = [0; 512];
    for expected in [heartbeat, position] {
        let n = tokio::time::timeout(std::time::Duration::from_secs(1), socket.recv(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n], expected);
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), socket.recv(&mut buf))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn udp_replay_refuses_a_log_with_no_identified_telemetry() {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("synthetic.tlog");
    let mut log = Vec::new();
    tlog::record(&mut log, 1_790_000_000_000_000, COMMAND).unwrap();
    std::fs::write(&path, log).unwrap();
    let result = replay(&path, &socket.local_addr().unwrap().to_string(), 1.0).await;
    assert!(result.is_err(), "a command-only log must not be sent");
    let message = result.unwrap_err().to_string();
    assert!(message.contains("autopilot heartbeat"), "{message}");
    assert!(message.contains("in-app Replay"), "{message}");
    let mut buf = [0; 512];
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), socket.recv(&mut buf))
            .await
            .is_err()
    );
}
