//! Several vehicles on one source connection can use one destination.
use super::*;
use uxduck_mavlink::sim::{FakeVehicle, gcs_heartbeat_frame};

fn receive_exact(socket: &std::net::UdpSocket, expected: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut bytes = [0u8; 2048];
    loop {
        if let Ok((n, _)) = socket.recv_from(&mut bytes)
            && bytes[..n] == *expected
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "expected forwarded packet did not arrive"
        );
    }
}

#[test]
fn two_vehicle_filters_share_a_destination_without_false_loops_or_duplicate_traffic() {
    let daemon = Daemon::start(&[]);
    let listen = free_udp_port();
    let source = add_link(
        &daemon,
        &json!({"kind":{"type":"udp-listen","host":"127.0.0.1","port":listen}}),
    );
    wait_up(&daemon, source["id"].as_str().unwrap());
    let target = format!("127.0.0.1:{listen}");
    let (a_socket, _) = program();
    let (b_socket, _) = program();
    let mut a = FakeVehicle::new(1, 0xA);
    let mut b = FakeVehicle::new(2, 0xB);
    heard_on(&a_socket, &mut a, listen);
    heard_on(&b_socket, &mut b, listen);
    let list = vehicles(&daemon);
    assert_eq!(list.len(), 2);
    let a_id = list.iter().find(|v| v["systemId"] == 1).unwrap()["id"]
        .as_str()
        .unwrap();
    let b_id = list.iter().find(|v| v["systemId"] == 2).unwrap()["id"]
        .as_str()
        .unwrap();
    let (receiver, port) = program();
    let make = |vid| json!({"kind":{"type":"udp-forward","host":"127.0.0.1","port":port},"vehicleFilter":[vid]});
    let a_forward = add_link(&daemon, &make(a_id));
    let b_forward = add_link(&daemon, &make(b_id));
    let a_forward = a_forward["id"].as_str().unwrap();
    let b_forward = b_forward["id"].as_str().unwrap();
    wait_up(&daemon, a_forward);
    wait_up(&daemon, b_forward);
    let mut bytes = [0u8; 2048];
    let a_frame = a.heartbeat();
    a_socket.send_to(&a_frame, &target).unwrap();
    let (n, a_peer) = receiver.recv_from(&mut bytes).unwrap();
    assert_eq!(bytes[..n], a_frame);
    assert!(receiver.recv_from(&mut bytes).is_err(), "one copy only");
    let b_frame = b.heartbeat();
    b_socket.send_to(&b_frame, &target).unwrap();
    let (n, b_peer) = receiver.recv_from(&mut bytes).unwrap();
    assert_eq!(bytes[..n], b_frame);
    assert!(receiver.recv_from(&mut bytes).is_err(), "one copy only");
    assert_ne!(a_peer, b_peer);
    // One ground station legitimately emits the same heartbeat on both pipes.
    for seq in 240..248 {
        let heartbeat = gcs_heartbeat_frame(255, 190, seq);
        receiver.send_to(&heartbeat, a_peer).unwrap();
        receiver.send_to(&heartbeat, b_peer).unwrap();
        receive_exact(&a_socket, &heartbeat);
        receive_exact(&b_socket, &heartbeat);
    }
    assert_eq!(link_status(&daemon, a_forward)["status"], "up");
    assert_eq!(link_status(&daemon, b_forward)["status"], "up");
    // Real telemetry echoed by the receiver still switches off its source forward.
    for _ in 0..5 {
        let frame = a.heartbeat();
        a_socket.send_to(&frame, &target).unwrap();
        let (n, _) = receiver.recv_from(&mut bytes).unwrap();
        assert_eq!(bytes[..n], frame);
        receiver.send_to(&frame, &target).unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    wait_for("only A's loop shutdown", || {
        link_status(&daemon, a_forward)["status"] == "disabled-by-loop-detection"
    });
    assert_eq!(link_status(&daemon, b_forward)["status"], "up");
    let frame = b.heartbeat();
    b_socket.send_to(&frame, &target).unwrap();
    let (n, _) = receiver.recv_from(&mut bytes).unwrap();
    assert_eq!(bytes[..n], frame);
}
