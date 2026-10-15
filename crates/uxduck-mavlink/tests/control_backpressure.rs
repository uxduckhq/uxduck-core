//! Control remains writable under receive pressure; replies retain causal order.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "regression tests")]
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;
use uxduck_core::domain::command::Command;
use uxduck_mavlink::links::{self, LinkEvent, Outgoing};
use uxduck_mavlink::session::{CommandOrder, CommandOutcome, Config, Effect, Session};
use uxduck_mavlink::sim::{command_ack_frame, heartbeat_frame};

#[tokio::test]
async fn saturated_inbound_queue_must_not_block_outbound_control() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, mut events) = links::event_channel();
    let link = links::tcp_client(address.ip().to_string(), address.port(), tx);
    let (server, _) = listener.accept().await.unwrap();
    assert_eq!(events.recv().await.unwrap().event, LinkEvent::Up);
    let (mut reader, mut writer) = server.into_split();
    let sender = tokio::spawn(async move {
        writer
            .write_all(&vec![7; 4096 * (links::EVENT_CAPACITY + 32)])
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(10)).await;
    });
    timeout(Duration::from_secs(3), async {
        while events.len() < links::EVENT_CAPACITY {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    link.send(Outgoing {
        peer: None,
        bytes: b"safety command".to_vec(),
    });
    let mut bytes = [0; 14];
    let delivered = timeout(WAIT, reader.read_exact(&mut bytes)).await;
    sender.abort();
    delivered
        .expect("outbound command cannot stall behind telemetry")
        .unwrap();
    assert_eq!(&bytes, b"safety command");
}

#[test]
fn ack_received_before_a_new_order_must_not_complete_that_order() {
    let mut session = Session::new(Config::default());
    let effects = session.ingest("link", "peer", &heartbeat_frame(1, 1, 0), 1_000);
    let id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Vehicle(v) => Some(v.id.clone()),
            _ => None,
        })
        .unwrap();
    let order = |receipt: &str| CommandOrder {
        receipt_id: receipt.into(),
        vehicle_id: id.clone(),
        command: Command::Rtl,
        force: false,
    };
    session.command(order("old"), 2_000);
    let ack = command_ack_frame(1, 1, (255, 190), 1, 20, 0);
    session.ingest("link", "peer", &ack, 3_000);
    // Another response to the OLD command was read at 4,000, but retained
    // in the data queue. A new control order overtakes it at 10,000.
    let stale_ack = command_ack_frame(1, 1, (255, 190), 2, 20, 0);
    session.command(order("new"), 10_000);
    let result = session.ingest_received("link", "peer", &stale_ack, 4_000, 11_000);
    assert!(!result.iter().any(|effect|matches!(effect,Effect::CommandDone{receipt_id,outcome:CommandOutcome::Accepted,..}if receipt_id=="new")),"the old ACK incorrectly accepted the new command: {result:?}");
}

const WAIT: Duration = Duration::from_secs(3);

async fn full(events: &tokio::sync::mpsc::Receiver<links::TimedEvent>) {
    timeout(WAIT, async {
        while events.len() < links::EVENT_CAPACITY {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("receive queue fills");
    tokio::time::sleep(Duration::from_millis(30)).await;
}

#[tokio::test]
async fn a_tcp_server_can_send_with_a_full_receive_queue() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = probe.local_addr().unwrap();
    drop(probe);
    let (tx, mut events) = links::event_channel();
    let link = links::tcp_server(address, tx).await.unwrap();
    assert_eq!(events.recv().await.unwrap().event, LinkEvent::Up);
    let client = tokio::net::TcpStream::connect(address).await.unwrap();
    let peer = client.local_addr().unwrap().to_string();
    let (mut reader, mut writer) = client.into_split();
    let sending = tokio::spawn(async move {
        let _ = writer
            .write_all(&vec![7; 4096 * (links::EVENT_CAPACITY + 32)])
            .await;
        tokio::time::sleep(WAIT).await;
    });
    full(&events).await;
    link.send(Outgoing {
        peer: Some(peer),
        bytes: b"control".to_vec(),
    });
    let mut bytes = [0; 7];
    timeout(WAIT, reader.read_exact(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&bytes, b"control");
    assert_eq!(
        events.len(),
        links::EVENT_CAPACITY,
        "no receive draining needed"
    );
    drop(link);
    sending.abort();
}

#[tokio::test]
async fn a_udp_client_sends_control_without_sending_false_quiet_server_hellos() {
    use std::sync::{Arc, atomic::AtomicU8};
    let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (tx, mut events) = links::event_channel();
    let link = links::udp_client(
        server.local_addr().unwrap(),
        Arc::new(AtomicU8::new(255)),
        tx,
    )
    .await
    .unwrap();
    assert_eq!(events.recv().await.unwrap().event, LinkEvent::Up);
    let mut buffer = [0; 512];
    let (_, client) = timeout(WAIT, server.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    for _ in 0..links::EVENT_CAPACITY + 16 {
        server.send_to(b"telemetry", client).await.unwrap();
    }
    full(&events).await;
    link.send(Outgoing {
        peer: None,
        bytes: b"control heartbeat".to_vec(),
    });
    let (len, _) = timeout(WAIT, server.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buffer[..len], b"control heartbeat");
    // Receipt backpressure must not look like a silent server and cause
    // a read-only hello to take over for a withheld GCS heartbeat.
    assert!(
        timeout(
            links::UDP_CLIENT_QUIET + Duration::from_millis(1100),
            server.recv_from(&mut buffer)
        )
        .await
        .is_err()
    );
    assert_eq!(events.len(), links::EVENT_CAPACITY);
    let detached = link.sender();
    drop(link);
    timeout(WAIT, async {
        while detached.send(Outgoing {
            peer: None,
            bytes: Vec::new(),
        }) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropping a saturated link cancels both futures");
}

#[cfg(unix)]
#[test]
fn a_serial_link_sends_control_and_stops_with_a_full_receive_queue() {
    use serialport::SerialPort;
    use std::io::{Read, Write};
    let (mut device, ours) = serialport::TTYPort::pair().unwrap();
    device.set_timeout(WAIT).unwrap();
    let (tx, mut events) = links::event_channel();
    let link = links::serial(ours.name().unwrap(), 57_600, None, tx);
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        if let Ok(event) = events.try_recv() {
            assert_eq!(event.event, LinkEvent::Up);
            break;
        }
        assert!(std::time::Instant::now() < deadline, "serial port opens");
        std::thread::sleep(Duration::from_millis(1));
    }
    while events.len() < links::EVENT_CAPACITY {
        // The PTY master is blocking on Linux: a bulk write can hang after
        // the receiver stops draining. Feed one byte per read instead.
        device.write_all(&[7]).unwrap();
        assert!(
            std::time::Instant::now() < deadline,
            "serial receive queue fills"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    // Leave one received event waiting behind the full event queue.
    device.write_all(&[7]).unwrap();
    std::thread::sleep(Duration::from_millis(30));
    link.send(Outgoing {
        peer: None,
        bytes: b"control".to_vec(),
    });
    let mut bytes = [0; 7];
    device
        .read_exact(&mut bytes)
        .expect("control writes keep running under receive pressure");
    assert_eq!(&bytes, b"control");
    let detached = link.sender();
    drop(link);
    while detached.send(Outgoing {
        peer: None,
        bytes: Vec::new(),
    }) {
        assert!(
            std::time::Instant::now() < deadline,
            "serial thread stops without draining its event queue"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(ours);
    drop(device);
}

fn live() -> (Session, String) {
    let mut session = Session::new(Config::default());
    let effects = session.ingest("link", "peer", &heartbeat_frame(1, 1, 0), 1_000);
    let id = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Vehicle(v) => Some(v.id.clone()),
            _ => None,
        })
        .unwrap();
    (session, id)
}

fn rtl(id: &str, receipt: &str) -> CommandOrder {
    CommandOrder {
        receipt_id: receipt.into(),
        vehicle_id: id.into(),
        command: Command::Rtl,
        force: false,
    }
}

#[test]
fn retries_keep_the_initial_ack_fence_and_ignore_old_progress() {
    let (mut session, id) = live();
    let start = 10_000;
    session.command(rtl(&id, "retry"), start);
    let retry = start + Config::default().command_timeout_us;
    session.tick(retry);
    let old_progress = command_ack_frame(1, 1, (255, 190), 1, 20, 5);
    for received in [start - 1, start + 1] {
        session.ingest_received("link", "peer", &old_progress, received, retry + 1);
        assert!(
            !session
                .tick(retry + 2)
                .iter()
                .any(|e| matches!(e, Effect::Send { .. })),
            "progress from before the retry cannot move its deadline backwards"
        );
    }
    let ack = command_ack_frame(1, 1, (255, 190), 2, 20, 0);
    let done = session.ingest_received("link", "peer", &ack, start + 1, retry + 3);
    assert!(done.iter().any(|e|matches!(e,Effect::CommandDone{receipt_id,outcome:CommandOutcome::Accepted,..}if receipt_id=="retry")),"a response to the initial attempt remains valid after retry");
}

#[test]
fn a_queued_parameter_value_cannot_finish_a_new_write() {
    use uxduck_mavlink::{sim::param_value_frame, transfers::Param};
    let (mut session, id) = live();
    let param = Param {
        name: "TEST".into(),
        value: 1.0,
        kind: 9,
    };
    session.set_parameter("write", &id, param.clone(), false, 10_000);
    let value = param_value_frame(1, 1, 1, &param.name, param.value, param.kind, 1, 0);
    let old = session.ingest_received("link", "peer", &value, 9_999, 11_000);
    assert!(!old.iter().any(|e| matches!(e, Effect::ParamWritten { .. })));
    let new = session.ingest_received("link", "peer", &value, 10_001, 11_001);
    assert!(
        new.iter().any(
            |e| matches!(e,Effect::ParamWritten{order_id,result:Ok(_),..}if order_id=="write")
        )
    );
}

#[test]
fn old_mission_responses_cannot_finish_new_downloads_or_uploads() {
    use uxduck_mavlink::sim::{Route, mission_ack_frame, mission_count_frame};
    let (mut session, id) = live();
    let route = Route {
        from: (1, 1),
        to: (255, 190),
    };
    session.download_mission(&id, 10_000);
    let empty = mission_count_frame(route, 1, 0);
    assert!(
        session
            .ingest_received("link", "peer", &empty, 9_999, 11_000)
            .is_empty()
    );
    assert!(
        session
            .ingest_received("link", "peer", &empty, 10_001, 11_001)
            .iter()
            .any(|e| matches!(e, Effect::Mission { result: Ok(_), .. }))
    );
    session.upload_mission("upload", &id, Vec::new(), 20_000);
    let ack = mission_ack_frame(route, 2, 0);
    assert!(
        session
            .ingest_received("link", "peer", &ack, 19_999, 21_000)
            .is_empty()
    );
    assert!(session.ingest_received("link","peer",&ack,20_001,21_001).iter().any(|e|matches!(e,Effect::MissionUploaded{order_id,result:Ok(()),..}if order_id=="upload")));
}

#[cfg(unix)]
#[test]
fn a_serial_disconnect_does_not_block_drop_behind_a_full_status_queue() {
    use serialport::SerialPort;
    use std::io::Write;
    let (mut device, ours) = serialport::TTYPort::pair().unwrap();
    device.set_timeout(WAIT).unwrap();
    let (tx, mut events) = links::event_channel();
    let link = links::serial(ours.name().unwrap(), 57_600, None, tx);
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        if let Ok(event) = events.try_recv() {
            assert_eq!(event.event, LinkEvent::Up);
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    while events.len() < links::EVENT_CAPACITY {
        device.write_all(&[7]).unwrap();
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(device);
    link.send(Outgoing {
        peer: None,
        bytes: b"fails on unplug".to_vec(),
    });
    std::thread::sleep(Duration::from_millis(30));
    let detached = link.sender();
    drop(link);
    while detached.send(Outgoing {
        peer: None,
        bytes: Vec::new(),
    }) {
        assert!(
            std::time::Instant::now() < deadline,
            "disconnect status cannot hold the serial thread alive after drop"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn stale_ftp_and_list_replies_cannot_complete_a_new_parameter_download() {
    use uxduck_mavlink::{
        sim::{ftp_frame, param_value_frame},
        transfers::{FtpRequest, ftp},
    };
    let (mut session, id) = live();
    session.download_parameters(&id, 10_000);
    // Sessions are reset first (request 1), then the file opened (2).
    let answer = |seq: u16, opcode: u8, req_opcode: u8, data: Vec<u8>| {
        let mut payload = ftp::payload(&FtpRequest {
            seq,
            session: 0,
            opcode,
            size: u8::try_from(data.len()).unwrap(),
            offset: 0,
            data,
        });
        payload[5] = req_opcode;
        ftp_frame((1, 1), (255, 190), 1, payload)
    };
    let reset = answer(2, ftp::ACK, ftp::RESET_SESSIONS, Vec::new());
    assert!(
        session
            .ingest_received("link", "peer", &reset, 9_999, 11_000)
            .is_empty(),
        "received before it was asked"
    );
    let opening = session.ingest_received("link", "peer", &reset, 10_001, 11_000);
    assert!(opening.iter().any(|e|matches!(e,Effect::Send{bytes,..}if uxduck_mavlink::framing::header(bytes).unwrap().message_id==110)));
    let refused = answer(3, ftp::NAK, ftp::OPEN_FILE_RO, vec![10]);
    let fallback = session.ingest_received("link", "peer", &refused, 11_001, 11_002);
    assert!(fallback.iter().any(|e|matches!(e,Effect::Send{bytes,..}if uxduck_mavlink::framing::header(bytes).unwrap().message_id==21)),"a fresh FTP refusal starts a list download");
    let param = param_value_frame(1, 1, 2, "TEST", 1.0, 9, 1, 0);
    assert!(
        session
            .ingest_received("link", "peer", &param, 11_001, 12_000)
            .is_empty()
    );
    assert!(
        session
            .ingest_received("link", "peer", &param, 11_003, 12_001)
            .iter()
            .any(|e| matches!(e, Effect::Parameters { result: Ok(_), .. }))
    );
}

#[test]
fn a_delayed_valid_ftp_reply_starts_the_next_request_deadline_when_processed() {
    use uxduck_mavlink::sim::FakeVehicle;
    let (mut session, id) = live();
    let sent = session.download_parameters(&id, 10_000);
    let mut fake = FakeVehicle::new(1, 0xABCD);
    let reply = sent
        .iter()
        .find_map(|e| match e {
            Effect::Send { bytes, .. } => fake.answer(bytes).into_iter().next(),
            _ => None,
        })
        .unwrap();
    let processing = 2_000_000;
    let next = session.ingest_received("link", "peer", &reply, 10_001, processing);
    assert!(next.iter().any(|e|matches!(e,Effect::Send{bytes,..}if uxduck_mavlink::framing::header(bytes).unwrap().message_id==110)));
    let tick = session.tick(processing + Config::default().transfers.timeout_us / 2);
    assert!(!tick.iter().any(|e|matches!(e,Effect::Send{bytes,..}if uxduck_mavlink::framing::header(bytes).unwrap().message_id==110)),"a newly sent FTP request cannot already be overdue because its previous reply waited in a queue");
}
