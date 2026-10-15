use super::*;
use crate::fake_files::{FakeFiles, crc32};
use crate::transfers::{Param, encode_param_pck};

/// Short timeouts so lost answers cost little.
fn timing() -> Timing {
    Timing {
        timeout_us: 100_000,
        attempts: 4,
        ..Timing::default()
    }
}

/// A vehicle's FTP behind a link: requests go in, answers come back.
struct Bench {
    client: FtpClient,
    fake: FakeFiles,
    params: Vec<(Param, Option<f32>)>,
    now_us: u64,
    events: Vec<Event>,
    /// Every request sent, for counting.
    sent: Vec<FtpRequest>,
}

impl Bench {
    fn new() -> Self {
        let params = [
            ("ATC_RAT_RLL_P", 0.135, 9),
            ("BATT_CAPACITY", 3300.0, 6),
            ("FRAME_CLASS", 1.0, 2),
            ("WPNAV_SPEED", 1000.0, 9),
        ]
        .iter()
        .map(|(name, value, kind)| {
            (
                Param {
                    name: (*name).into(),
                    value: *value,
                    kind: *kind,
                },
                Some(*value),
            )
        })
        .collect();
        Self {
            client: FtpClient::new(),
            fake: FakeFiles::default(),
            params,
            now_us: 0,
            events: Vec::new(),
            sent: Vec::new(),
        }
    }

    /// Runs requests and answers until nothing is left to do.
    fn run(&mut self, first: Output) -> Vec<Event> {
        let mut pending = first.send;
        self.events.extend(first.events);
        for _ in 0..100_000 {
            if pending.is_empty() {
                if !self.client.busy() {
                    break;
                }
                self.now_us += timing().timeout_us;
                let out = self.client.tick(self.now_us, &timing());
                pending = out.send;
                self.events.extend(out.events);
                continue;
            }
            let mut next = Vec::new();
            for request in pending.drain(..) {
                self.sent.push(request.clone());
                let as_sent = ftp::read(&ftp::payload(&request));
                let params = self.params.clone();
                let answered = self.fake.answer(&as_sent, || encode_param_pck(&params));
                if let Some(file) = answered.params_written {
                    for (param, _) in parse_param_pck(&file).unwrap() {
                        if let Some(held) =
                            self.params.iter_mut().find(|(p, _)| p.name == param.name)
                        {
                            held.0.value = param.value;
                        }
                    }
                }
                for payload in answered.payloads {
                    self.now_us += 1_000;
                    let out = self
                        .client
                        .answer(&ftp::read(&payload), self.now_us, self.now_us);
                    next.extend(out.send);
                    self.events.extend(out.events);
                }
            }
            pending = next;
        }
        std::mem::take(&mut self.events)
    }

    fn file(&mut self, id: &str, op: Op) -> Vec<Event> {
        let out = self.client.file(id, op, self.now_us);
        self.run(out)
    }

    fn read(&mut self, path: &str, from: u64) -> (Vec<u8>, Vec<Event>) {
        let events = self.file(
            "r",
            Op::Read {
                path: path.into(),
                from,
            },
        );
        let mut bytes = Vec::new();
        for event in &events {
            if let Event::Data {
                offset,
                bytes: more,
                ..
            } = event
            {
                assert_eq!(*offset, from + bytes.len() as u64, "in order");
                bytes.extend_from_slice(more);
            }
        }
        (bytes, events)
    }

    fn opcodes(&self, opcode: u8) -> usize {
        self.sent.iter().filter(|r| r.opcode == opcode).count()
    }
}

fn done(events: &[Event]) -> &Result<(), Failure> {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::Done { result, .. } => Some(result),
            _ => None,
        })
        .expect("a Done event")
}

fn listed(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .find_map(|e| match e {
            Event::Listed { entries, .. } => Some(
                entries
                    .iter()
                    .map(|e| {
                        if e.is_dir() {
                            format!("{}/", e.name)
                        } else {
                            e.name.clone()
                        }
                    })
                    .collect(),
            ),
            _ => None,
        })
        .expect("a listing")
}

#[test]
fn a_listing_reads_files_folders_and_skips() {
    let (entries, count) = parse_listing(b"F00000001.BIN\t3000\0Dscripts\0D.\0D..\0S\0");
    assert_eq!(count, 5);
    assert_eq!(
        entries,
        [
            Entry {
                name: "00000001.BIN".into(),
                size: Some(3000)
            },
            Entry {
                name: "scripts".into(),
                size: None
            },
        ]
    );
}

#[test]
fn a_folder_is_listed_page_by_page() {
    let mut bench = Bench::new();
    for n in 4..40 {
        bench
            .fake
            .files
            .insert(format!("/APM/LOGS/{n:08}.BIN"), vec![1; n]);
    }
    let events = bench.file(
        "l",
        Op::List {
            path: "/APM/LOGS".into(),
        },
    );
    assert_eq!(done(&events), &Ok(()));
    let names = listed(&events);
    assert_eq!(names.len(), 40, "39 logs and LASTLOG.TXT");
    assert!(bench.opcodes(ftp::LIST_DIRECTORY) > 2, "more than one page");
    let root = bench.file("root", Op::List { path: "/".into() });
    assert_eq!(listed(&root), ["APM/", "@PARAM/", "@SYS/"]);
    let sys = bench.file(
        "sys",
        Op::List {
            path: "@SYS".into(),
        },
    );
    assert_eq!(listed(&sys), ["threads.txt", "uarts.txt"]);
    let missing = bench.file(
        "m",
        Op::List {
            path: "/NOPE".into(),
        },
    );
    assert_eq!(
        done(&missing),
        &Err(Failure::Rejected("there is no such file or folder".into()))
    );
}

#[test]
fn a_log_comes_down_in_bursts_whole_and_in_order() {
    let mut bench = Bench::new();
    let (bytes, events) = bench.read("/APM/LOGS/00000003.BIN", 0);
    assert_eq!(done(&events), &Ok(()));
    assert_eq!(bytes, bench.fake.files["/APM/LOGS/00000003.BIN"]);
    assert!(matches!(events[0], Event::Opened { size: 60_000, .. }));
    assert_eq!(bench.fake.bursts, 1, "one burst brings 60 KB");
    assert_eq!(bench.fake.reads, 0);
    assert_eq!(bench.opcodes(ftp::RESET_SESSIONS), 1);
    assert_eq!(bench.opcodes(ftp::TERMINATE_SESSION), 1, "closed after");
    // Sessions are reset once per vehicle, not per job.
    let _ = bench.read("/APM/LOGS/00000001.BIN", 0);
    assert_eq!(bench.opcodes(ftp::RESET_SESSIONS), 1);
}

#[test]
fn bursts_end_at_the_vehicles_limit_and_go_on() {
    let mut bench = Bench::new();
    bench.fake.burst_max = 20;
    let (bytes, events) = bench.read("/APM/LOGS/00000003.BIN", 0);
    assert_eq!(done(&events), &Ok(()));
    assert_eq!(bytes.len(), 60_000);
    assert_eq!(bench.fake.bursts, 60_000_u32.div_ceil(20 * 239));
}

#[test]
fn a_lossy_link_loses_pieces_that_are_read_again() {
    let mut bench = Bench::new();
    bench.fake.drop_every = Some(7);
    bench.fake.burst_max = 50;
    let (bytes, events) = bench.read("/APM/LOGS/00000003.BIN", 0);
    assert_eq!(done(&events), &Ok(()));
    assert_eq!(bytes, bench.fake.files["/APM/LOGS/00000003.BIN"]);
    assert!(bench.fake.reads > 0, "gaps read singly");
}

#[test]
fn the_end_comes_from_the_vehicle_even_when_the_last_pieces_are_lost() {
    let mut bench = Bench::new();
    // Reset and open are answers 1 and 2; 3000 bytes are 13 pieces, 3 to
    // 15, then the EOF. Lose the 15th answer: the last piece.
    bench.fake.drop_every = Some(15);
    let (bytes, events) = bench.read("/APM/LOGS/00000001.BIN", 0);
    assert_eq!(done(&events), &Ok(()));
    assert_eq!(bytes, bench.fake.files["/APM/LOGS/00000001.BIN"]);
    assert_eq!(bench.fake.reads, 1, "the last piece, read again");
}

#[test]
fn a_download_resumes_from_its_partial_copy() {
    let mut bench = Bench::new();
    let (bytes, events) = bench.read("/APM/LOGS/00000002.BIN", 12_345);
    assert_eq!(done(&events), &Ok(()));
    assert_eq!(bytes, bench.fake.files["/APM/LOGS/00000002.BIN"][12_345..]);
}

#[test]
fn a_missing_file_says_so_and_frees_the_session() {
    let mut bench = Bench::new();
    let (_, events) = bench.read("/APM/LOGS/99.BIN", 0);
    assert_eq!(
        done(&events),
        &Err(Failure::Rejected("there is no such file or folder".into()))
    );
    let (bytes, _) = bench.read("/APM/LOGS/00000001.BIN", 0);
    assert_eq!(bytes.len(), 3_000);
}

#[test]
fn a_script_goes_up_and_is_listed_and_read_back() {
    let mut bench = Bench::new();
    let script: Vec<u8> = b"function update() return update, 1000 end\n"
        .iter()
        .copied()
        .cycle()
        .take(700)
        .collect();
    let events = bench.file(
        "w",
        Op::Write {
            path: "/APM/scripts/hello.lua".into(),
            bytes: script.clone(),
        },
    );
    assert_eq!(done(&events), &Ok(()));
    let sent: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            Event::Sent {
                done, total: 700, ..
            } => Some(*done),
            _ => None,
        })
        .collect();
    assert_eq!(sent, [239, 478, 700]);
    assert_eq!(bench.fake.files["/APM/scripts/hello.lua"], script);
    let listing = bench.file(
        "l",
        Op::List {
            path: "/APM/scripts".into(),
        },
    );
    assert_eq!(listed(&listing), ["hello.lua"]);
    // Written again over itself: replaced, not appended to.
    bench.file(
        "w2",
        Op::Write {
            path: "/APM/scripts/hello.lua".into(),
            bytes: b"-- short\n".to_vec(),
        },
    );
    assert_eq!(bench.fake.files["/APM/scripts/hello.lua"], b"-- short\n");
}

#[test]
fn the_system_folder_is_read_only() {
    let mut bench = Bench::new();
    let (bytes, _) = bench.read("@SYS/uarts.txt", 0);
    assert!(String::from_utf8(bytes).unwrap().contains("SERIAL2"));
    let write = bench.file(
        "w",
        Op::Write {
            path: "@SYS/uarts.txt".into(),
            bytes: b"x".to_vec(),
        },
    );
    assert_eq!(
        done(&write),
        &Err(Failure::Rejected(
            "the vehicle's storage is read-only".into()
        ))
    );
    let remove = bench.file(
        "rm",
        Op::Remove {
            path: "@SYS/uarts.txt".into(),
        },
    );
    assert!(done(&remove).is_err());
}

#[test]
fn folders_removals_renames_and_crc() {
    let mut bench = Bench::new();
    assert_eq!(
        done(&bench.file(
            "mk",
            Op::MakeDir {
                path: "/APM/old".into()
            }
        )),
        &Ok(())
    );
    assert_eq!(
        done(&bench.file(
            "mv",
            Op::Rename {
                from: "/APM/LOGS/00000001.BIN".into(),
                to: "/APM/old/00000001.BIN".into()
            }
        )),
        &Ok(())
    );
    assert_eq!(
        done(&bench.file(
            "rmdir",
            Op::RemoveDir {
                path: "/APM/old".into()
            }
        )),
        &Err(Failure::Rejected("the folder is not empty".into()))
    );
    let crc = bench.file(
        "crc",
        Op::Crc {
            path: "/APM/old/00000001.BIN".into(),
        },
    );
    let expected = crc32(&bench.fake.files["/APM/old/00000001.BIN"]);
    assert!(crc.contains(&Event::Crc {
        id: "crc".into(),
        crc: expected
    }));
    assert_eq!(
        done(&bench.file(
            "rm",
            Op::Remove {
                path: "/APM/old/00000001.BIN".into()
            }
        )),
        &Ok(())
    );
    assert_eq!(
        done(&bench.file(
            "rmdir",
            Op::RemoveDir {
                path: "/APM/old".into()
            }
        )),
        &Ok(())
    );
    assert!(!bench.fake.dirs.contains("/APM/old"));
}

#[test]
fn a_listing_does_not_wait_for_a_download() {
    let mut bench = Bench::new();
    bench.fake.burst_max = 10;
    let mut out = bench.client.file(
        "big",
        Op::Read {
            path: "/APM/LOGS/00000003.BIN".into(),
            from: 0,
        },
        0,
    );
    out.extend(bench.client.file(
        "l",
        Op::List {
            path: "/APM".into(),
        },
        0,
    ));
    let events = bench.run(out);
    let listed_at = events
        .iter()
        .position(|e| matches!(e, Event::Listed { .. }))
        .unwrap();
    let done_at = events
        .iter()
        .position(|e| matches!(e, Event::Done { id, .. } if id == "big"))
        .unwrap();
    assert!(listed_at < done_at, "the listing came first");
}

#[test]
fn a_cancelled_download_closes_its_file_and_the_next_job_runs() {
    let mut bench = Bench::new();
    let start = bench.client.file(
        "big",
        Op::Read {
            path: "/APM/LOGS/00000003.BIN".into(),
            from: 0,
        },
        0,
    );
    // Reset, open: the file is open on the vehicle.
    let mut pending = start.send;
    for _ in 0..2 {
        let request = pending.remove(0);
        let answered = bench
            .fake
            .answer(&ftp::read(&ftp::payload(&request)), Vec::new);
        pending.extend(
            bench
                .client
                .answer(&ftp::read(&answered.payloads[0]), 1, 1)
                .send,
        );
    }
    let cancelled = bench.client.cancel("big", 2);
    assert_eq!(cancelled.send[0].opcode, ftp::TERMINATE_SESSION);
    assert!(cancelled.events.is_empty(), "not reported");
    bench.run(cancelled);
    let (bytes, _) = bench.read("/APM/LOGS/00000001.BIN", 0);
    assert_eq!(bytes.len(), 3_000, "the session is free");
}

#[test]
fn late_answers_to_an_old_burst_are_not_taken_for_a_new_file() {
    let mut bench = Bench::new();
    let (_, _) = bench.read("/APM/LOGS/00000001.BIN", 0);
    // An answer to the first file's burst, arriving during the second.
    let stale = bench
        .sent
        .iter()
        .find(|r| r.opcode == ftp::BURST_READ_FILE)
        .cloned()
        .unwrap();
    let out = bench.client.file(
        "next",
        Op::Read {
            path: "/APM/LOGS/00000002.BIN".into(),
            from: 0,
        },
        bench.now_us,
    );
    let mut late = ftp::read(&ftp::payload(&FtpRequest {
        seq: stale.seq.wrapping_add(3),
        session: stale.session,
        opcode: ftp::ACK,
        size: 3,
        offset: 0,
        data: vec![9, 9, 9],
    }));
    late.req_opcode = ftp::BURST_READ_FILE;
    let mut events = bench
        .client
        .answer(&late, bench.now_us, bench.now_us)
        .events;
    events.extend(bench.run(out));
    let mut bytes = Vec::new();
    for event in &events {
        if let Event::Data { bytes: more, .. } = event {
            bytes.extend_from_slice(more);
        }
    }
    assert_eq!(bytes, bench.fake.files["/APM/LOGS/00000002.BIN"]);
}

#[test]
fn parameters_come_by_file_and_go_up_as_one() {
    let mut bench = Bench::new();
    let out = bench.client.parameters(0);
    let events = bench.run(out);
    let [Event::Parameters(Ok(all))] = events.as_slice() else {
        panic!("{events:?}")
    };
    assert_eq!(all.len(), 4);
    assert!(!bench.client.busy());

    let writes = vec![
        (
            "o1".to_owned(),
            Param {
                name: "BATT_CAPACITY".into(),
                value: 5200.0,
                kind: 6,
            },
        ),
        (
            "o2".to_owned(),
            Param {
                name: "WPNAV_SPEED".into(),
                value: 750.0,
                kind: 9,
            },
        ),
        (
            "o3".to_owned(),
            Param {
                name: "NOT_THERE".into(),
                value: 1.0,
                kind: 9,
            },
        ),
    ];
    let out = bench.client.write_parameters(writes.clone(), bench.now_us);
    let events = bench.run(out);
    let [
        Event::ParamsWritten {
            writes: back,
            read_back: Ok(held),
        },
    ] = events.as_slice()
    else {
        panic!("{events:?}")
    };
    assert_eq!(back, &writes);
    let value = |name: &str| {
        held.iter()
            .find(|(p, _)| p.name == name)
            .map(|(p, _)| p.value)
    };
    assert_eq!(value("BATT_CAPACITY"), Some(5200.0));
    assert_eq!(value("WPNAV_SPEED"), Some(750.0));
    assert_eq!(value("NOT_THERE"), None, "skipped, as ArduPilot does");
    assert_eq!(bench.opcodes(ftp::CREATE_FILE), 1);
    let created = bench
        .sent
        .iter()
        .find(|r| r.opcode == ftp::CREATE_FILE)
        .unwrap();
    assert_eq!(created.data, ftp::PARAM_UPLOAD_PATH.as_bytes());
}

#[test]
fn parameters_survive_a_lossy_link_both_ways() {
    let mut bench = Bench::new();
    bench.fake.drop_every = Some(7);
    let out = bench.client.parameters(0);
    assert!(matches!(
        bench.run(out).as_slice(),
        [Event::Parameters(Ok(_))]
    ));
    let writes: Vec<(String, Param)> = (0..4)
        .map(|i| {
            (
                format!("o{i}"),
                Param {
                    name: bench.params[i].0.name.clone(),
                    value: 7.0,
                    kind: bench.params[i].0.kind,
                },
            )
        })
        .collect();
    let out = bench.client.write_parameters(writes, bench.now_us);
    let events = bench.run(out);
    let [
        Event::ParamsWritten {
            read_back: Ok(held),
            ..
        },
    ] = events.as_slice()
    else {
        panic!("{events:?}")
    };
    assert!(
        held.iter()
            .all(|(p, _)| (p.value - 7.0).abs() < f32::EPSILON)
    );
}

#[test]
fn a_vehicle_without_ftp_gives_the_parameters_up_to_the_list() {
    let mut client = FtpClient::new();
    let out = client.parameters(0);
    assert_eq!(out.send[0].opcode, ftp::RESET_SESSIONS);
    let mut sends = 1;
    let mut events = Vec::new();
    for step in 1..=10 {
        let out = client.tick(step * 100_000, &timing());
        sends += out.send.len();
        events.extend(out.events);
    }
    assert_eq!(sends, 4, "sent four times");
    assert!(
        matches!(events.as_slice(), [Event::Parameters(Err(why))] if why.contains("did not answer")),
        "{events:?}"
    );
    assert!(!client.busy());
}

#[test]
fn a_restart_fails_the_running_job_and_hands_back_parameter_writes() {
    let mut client = FtpClient::new();
    let _ = client.file(
        "big",
        Op::Read {
            path: "/APM/LOGS/00000003.BIN".into(),
            from: 0,
        },
        0,
    );
    let _ = client.write_parameters(
        vec![(
            "o1".into(),
            Param {
                name: "A".into(),
                value: 1.0,
                kind: 9,
            },
        )],
        0,
    );
    let events = client.restarted();
    assert!(matches!(&events[0], Event::Done { id, result: Err(_) } if id == "big"));
    assert!(matches!(
        &events[1],
        Event::ParamsWritten {
            read_back: Err(_),
            ..
        }
    ));
    assert!(!client.busy());
}

#[test]
fn a_path_too_long_is_never_sent() {
    let mut client = FtpClient::new();
    let out = client.file(
        "l",
        Op::List {
            path: "a".repeat(300),
        },
        0,
    );
    assert!(out.send.is_empty());
    assert!(matches!(
        &out.events[..],
        [Event::Done {
            result: Err(Failure::NotSent(_)),
            ..
        }]
    ));
}

#[test]
fn a_removal_whose_answer_was_lost_is_done_when_asked_again() {
    let mut bench = Bench::new();
    // Every answer lost once: the second ask hears "no such file".
    for (op, check) in [
        (
            Op::Remove {
                path: "/APM/LOGS/00000001.BIN".into(),
            },
            "removed",
        ),
        (
            Op::MakeDir {
                path: "/APM/new".into(),
            },
            "made",
        ),
    ] {
        bench.fake.drop_every = Some(1);
        let out = bench.client.file("x", op, bench.now_us);
        // The first answer is lost; then nothing is lost.
        let first = out.send;
        for request in &first {
            let _ = bench
                .fake
                .answer(&ftp::read(&ftp::payload(request)), Vec::new);
        }
        bench.fake.drop_every = None;
        let events = bench.run(Output::default());
        assert_eq!(done(&events), &Ok(()), "{check}");
    }
    assert!(!bench.fake.files.contains_key("/APM/LOGS/00000001.BIN"));
    assert!(bench.fake.dirs.contains("/APM/new"));
}
