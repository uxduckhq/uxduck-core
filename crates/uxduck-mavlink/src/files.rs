//! MAVLink FTP (S121, ADR-0027 F3 and F13): one client per vehicle. Jobs
//! that hold the vehicle's one FTP session (the parameter file, reading and
//! writing a file) run one at a time, in order; requests that need no
//! session (a listing, a removal, a new folder) run beside them, so a
//! listing does not wait for a long download. Like the protocols in
//! `transfers`, it takes what the vehicle said and the time and says what
//! to send; `Session` addresses and frames it. No I/O here.
//!
//! Reads come in bursts (`BurstReadFile`): the vehicle streams the file
//! until its end or its own limit, and pieces lost on the way are read one
//! by one (`ReadFile`) before the next burst. Bytes are reported in order,
//! so a partial copy is always the start of the file and a download
//! resumes from its length.
//!
//! Parameters by file (F13): ArduPilot serves every parameter with its
//! default as `@PARAM/param.pck`, and sets the parameters in a packed file
//! written to `@PARAM/param.pck`. It answers such a write with nothing per
//! parameter, so the file is read back after it.

use std::collections::{BTreeMap, VecDeque};

use crate::transfers::{
    Failure, FtpRequest, Param, Timing, WithDefault, encode_param_upload, ftp, parse_param_pck,
};

/// A parameter file past this is not one.
const PARAM_FILE_MAX: usize = 1 << 20;

/// The most answers ArduPilot sends to one burst request, each numbered one
/// past the one before: the next request is numbered past them all, so a
/// late answer to an old burst is never taken for a new one.
const BURST_ANSWERS: u16 = 2000;

/// One entry of a listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// The size in bytes; `None` for a folder.
    pub size: Option<u64>,
}

impl Entry {
    #[must_use]
    pub const fn is_dir(&self) -> bool {
        self.size.is_none()
    }
}

/// What someone asked of a vehicle's files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    List {
        path: String,
    },
    /// The file's bytes from `from` on: a resumed download starts where its
    /// partial copy ends.
    Read {
        path: String,
        from: u64,
    },
    /// Creates the file, or replaces what is there.
    Write {
        path: String,
        bytes: Vec<u8>,
    },
    Remove {
        path: String,
    },
    MakeDir {
        path: String,
    },
    RemoveDir {
        path: String,
    },
    Rename {
        from: String,
        to: String,
    },
    Crc {
        path: String,
    },
}

impl Op {
    /// Whether it holds the vehicle's FTP session while it runs.
    const fn needs_session(&self) -> bool {
        matches!(self, Self::Read { .. } | Self::Write { .. })
    }
}

/// What the client reports.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The parameter file, read: every parameter with its default. `Err`
    /// says why not, and the parameters come by the list instead.
    Parameters(Result<Vec<WithDefault>, String>),
    /// Parameters went up as one file (order id, parameter), and what the
    /// vehicle held when the file was read back, or why it was not.
    ParamsWritten {
        writes: Vec<(String, Param)>,
        read_back: Result<Vec<WithDefault>, String>,
    },
    /// A listing, whole.
    Listed { id: String, entries: Vec<Entry> },
    /// A read's file was opened: its size as the vehicle gives it.
    Opened { id: String, size: u64 },
    /// The next bytes of a read, always following the ones before.
    Data {
        id: String,
        offset: u64,
        bytes: Vec<u8>,
    },
    /// How much of a write the vehicle has taken.
    Sent { id: String, done: u64, total: u64 },
    /// A file's CRC-32 as the vehicle computed it.
    Crc { id: String, crc: u32 },
    /// A file job ended.
    Done {
        id: String,
        result: Result<(), Failure>,
    },
}

/// What to send and what to report after an answer or a tick.
#[derive(Debug, Default, PartialEq)]
pub struct Output {
    pub send: Vec<FtpRequest>,
    pub events: Vec<Event>,
}

impl Output {
    fn extend(&mut self, other: Self) {
        self.send.extend(other.send);
        self.events.extend(other.events);
    }
}

/// A job on the vehicle's FTP session.
#[derive(Clone, Debug)]
enum Job {
    Params,
    WriteParams { writes: Vec<(String, Param)> },
    File { id: String, op: Op },
}

impl Job {
    fn id(&self) -> Option<&str> {
        match self {
            Self::File { id, .. } => Some(id),
            Self::Params | Self::WriteParams { .. } => None,
        }
    }
}

/// The last request and its retries.
#[derive(Clone, Debug)]
struct Exchange {
    last: FtpRequest,
    attempts: u8,
    sent_us: u64,
}

/// Where a session job is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// `ResetSessions` sent, once per client before its first session.
    Reset,
    /// Opening to read.
    Open,
    Read,
    /// Creating to write.
    Create,
    Write,
}

/// A file being read: what has come in order, and pieces that came early.
#[derive(Clone, Debug, Default)]
struct Reading {
    /// Bytes from the start (or the resume point) that came in order.
    next: u64,
    /// Pieces past a gap, by offset.
    ahead: BTreeMap<u64, Vec<u8>>,
    /// Where the file ends, once the vehicle said so.
    end: Option<u64>,
    /// The parameter file is kept here rather than reported.
    collect: Option<Vec<u8>>,
}

impl Reading {
    /// The furthest byte heard.
    fn furthest(&self) -> u64 {
        self.ahead
            .iter()
            .next_back()
            .map_or(self.next, |(at, bytes)| at + bytes.len() as u64)
            .max(self.next)
    }

    /// Takes a piece; returns the bytes that now follow in order.
    fn take(&mut self, offset: u64, bytes: &[u8]) -> Vec<u8> {
        let end = offset + bytes.len() as u64;
        if end <= self.next || bytes.is_empty() {
            return Vec::new();
        }
        if offset > self.next {
            self.ahead.entry(offset).or_insert_with(|| bytes.to_vec());
            return Vec::new();
        }
        let skip = usize::try_from(self.next - offset).unwrap_or(usize::MAX);
        let mut out = bytes[skip..].to_vec();
        self.next = end;
        // Drain pieces that now follow, dropping any the new bytes cover.
        while let Some((&at, _)) = self.ahead.first_key_value() {
            if at > self.next {
                break;
            }
            let piece = self.ahead.remove(&at).unwrap_or_default();
            let piece_end = at + piece.len() as u64;
            if piece_end > self.next {
                let skip = usize::try_from(self.next - at).unwrap_or(usize::MAX);
                out.extend_from_slice(&piece[skip..]);
                self.next = piece_end;
            }
        }
        out
    }

    fn complete(&self) -> bool {
        self.end.is_some_and(|end| self.next >= end) && self.ahead.is_empty()
    }
}

#[derive(Clone, Debug)]
struct Active {
    job: Job,
    step: Step,
    session: u8,
    /// Whether the vehicle opened a file for this job, so it is closed.
    open: bool,
    exchange: Exchange,
    reading: Reading,
    /// What a write sends, and how much the vehicle has taken.
    upload: Vec<u8>,
    written: usize,
    /// Parameters by file: the file is written and is now read back.
    reading_back: bool,
    /// A refused open was answered by resetting sessions once.
    reset_again: bool,
    /// The number of the burst request being answered.
    burst_seq: Option<u16>,
    /// When the job started: anything received before is not an answer
    /// to it, however its numbers look.
    issued_us: u64,
}

/// A session-less request: a listing, a removal, a folder, a CRC.
#[derive(Clone, Debug)]
struct Quick {
    id: String,
    op: Op,
    exchange: Exchange,
    entries: Vec<Entry>,
    /// Entries listed so far, skipped ones included: the next offset.
    listed: u32,
    issued_us: u64,
}

/// One vehicle's MAVLink FTP.
#[derive(Clone, Debug, Default)]
pub struct FtpClient {
    seq: u16,
    /// The session number the next job asks for. ArduPilot keeps a session
    /// per number; PX4 answers with its own, which is then used.
    next_session: u8,
    /// Sessions reset once, before the first job that opens one, so a
    /// session another ground station left open does not block ours.
    reset_done: bool,
    queue: VecDeque<Job>,
    active: Option<Active>,
    quick_queue: VecDeque<(String, Op)>,
    quick: Option<Quick>,
}

fn data_size(data: &[u8]) -> u8 {
    u8::try_from(data.len()).unwrap_or(u8::MAX)
}

fn read_size() -> u8 {
    u8::try_from(ftp::DATA_MAX).unwrap_or(u8::MAX)
}

/// A refusal that says the file is not there.
fn gone(data: &[u8]) -> bool {
    matches!(data, [ftp::FILE_NOT_FOUND, ..] | [ftp::FAIL_ERRNO, 2, ..])
}

/// A refusal that says something of that name is there.
fn exists(data: &[u8]) -> bool {
    matches!(data, [ftp::FILE_EXISTS, ..] | [ftp::FAIL_ERRNO, 17, ..])
}

/// Reads a listing's entries: `F<name>\t<size>`, `D<name>`, or `S` for one
/// skipped, NUL between them. Returns the entries and how many there were,
/// skipped ones too.
#[must_use]
pub fn parse_listing(data: &[u8]) -> (Vec<Entry>, u32) {
    let mut entries = Vec::new();
    let mut count = 0;
    for raw in data.split(|b| *b == 0).filter(|raw| !raw.is_empty()) {
        count += 1;
        let text = String::from_utf8_lossy(&raw[1..]).into_owned();
        match raw[0] {
            b'F' => {
                let (name, size) = text.split_once('\t').unwrap_or((&text, "0"));
                entries.push(Entry {
                    name: name.to_owned(),
                    size: Some(size.trim().parse().unwrap_or(0)),
                });
            }
            b'D' if text != "." && text != ".." && !text.is_empty() => entries.push(Entry {
                name: text,
                size: None,
            }),
            _ => {}
        }
    }
    (entries, count)
}

impl FtpClient {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether anything is running or waiting.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.active.is_some()
            || self.quick.is_some()
            || !self.queue.is_empty()
            || !self.quick_queue.is_empty()
    }

    /// Whether the parameter file is being read or waits to be.
    #[must_use]
    pub fn reading_parameters(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|a| matches!(a.job, Job::Params))
            || self.queue.iter().any(|job| matches!(job, Job::Params))
    }

    /// Reads the parameter file, before any other job; one already running
    /// starts again.
    pub fn parameters(&mut self, now_us: u64) -> Output {
        self.queue.retain(|job| !matches!(job, Job::Params));
        let mut out = Output::default();
        if self
            .active
            .as_ref()
            .is_some_and(|a| matches!(a.job, Job::Params))
        {
            out.send.extend(self.close_active());
        }
        self.queue.push_front(Job::Params);
        out.extend(self.start_next(now_us));
        out
    }

    /// Sets parameters by writing them as one file, then reads the file
    /// back (F13).
    pub fn write_parameters(&mut self, writes: Vec<(String, Param)>, now_us: u64) -> Output {
        self.queue.push_back(Job::WriteParams { writes });
        self.start_next(now_us)
    }

    /// Queues a file job under `id`, reported by `Event::Done` and the
    /// events before it.
    pub fn file(&mut self, id: &str, op: Op, now_us: u64) -> Output {
        let too_long = match &op {
            Op::Rename { from, to } => from.len() + to.len() + 1 > ftp::DATA_MAX,
            Op::List { path }
            | Op::Read { path, .. }
            | Op::Write { path, .. }
            | Op::Remove { path }
            | Op::MakeDir { path }
            | Op::RemoveDir { path }
            | Op::Crc { path } => path.len() > ftp::DATA_MAX,
        };
        if too_long {
            return Output {
                send: Vec::new(),
                events: vec![Event::Done {
                    id: id.to_owned(),
                    result: Err(Failure::NotSent("the path is too long".into())),
                }],
            };
        }
        if op.needs_session() {
            self.queue.push_back(Job::File {
                id: id.to_owned(),
                op,
            });
            self.start_next(now_us)
        } else {
            self.quick_queue.push_back((id.to_owned(), op));
            self.start_quick(now_us)
        }
    }

    /// Stops a file job, waiting or running, without reporting it. A file
    /// open on the vehicle is closed.
    pub fn cancel(&mut self, id: &str, now_us: u64) -> Output {
        self.queue.retain(|job| job.id() != Some(id));
        self.quick_queue.retain(|(queued, _)| queued != id);
        if self.quick.as_ref().is_some_and(|q| q.id == id) {
            self.quick = None;
        }
        let mut out = self.start_quick(now_us);
        if self.active.as_ref().is_some_and(|a| a.job.id() == Some(id)) {
            out.send.extend(self.close_active());
            out.extend(self.start_next(now_us));
        }
        out
    }

    /// The vehicle restarted: its sessions are gone. A running file job
    /// fails, a parameter write is read back by nobody (the session sends
    /// it again by `PARAM_SET`), and the parameter file is dropped, since
    /// the engine downloads the parameters again after a restart.
    pub fn restarted(&mut self) -> Vec<Event> {
        self.reset_done = false;
        let restarted = || Failure::Rejected("the vehicle restarted".into());
        let mut events = Vec::new();
        let mut jobs: Vec<Job> = self.active.take().map(|a| a.job).into_iter().collect();
        jobs.extend(self.queue.drain(..));
        for job in jobs {
            match job {
                Job::Params => {}
                Job::WriteParams { writes } => events.push(Event::ParamsWritten {
                    writes,
                    read_back: Err("the vehicle restarted".into()),
                }),
                Job::File { id, .. } => events.push(Event::Done {
                    id,
                    result: Err(restarted()),
                }),
            }
        }
        if let Some(quick) = self.quick.take() {
            events.push(Event::Done {
                id: quick.id,
                result: Err(restarted()),
            });
        }
        for (id, _) in self.quick_queue.drain(..) {
            events.push(Event::Done {
                id,
                result: Err(restarted()),
            });
        }
        events
    }

    fn request(
        &mut self,
        session: u8,
        opcode: u8,
        size: u8,
        offset: u32,
        data: Vec<u8>,
    ) -> FtpRequest {
        self.seq = self.seq.wrapping_add(1);
        FtpRequest {
            seq: self.seq,
            session,
            opcode,
            size,
            offset,
            data,
        }
    }

    /// Closes the running job's file, if one is open, and drops the job.
    fn close_active(&mut self) -> Vec<FtpRequest> {
        let Some(active) = self.active.take() else {
            return Vec::new();
        };
        if active.open {
            vec![self.request(active.session, ftp::TERMINATE_SESSION, 0, 0, Vec::new())]
        } else {
            Vec::new()
        }
    }

    /// Starts the next session job when none runs.
    fn start_next(&mut self, now_us: u64) -> Output {
        if self.active.is_some() {
            return Output::default();
        }
        let Some(job) = self.queue.pop_front() else {
            return Output::default();
        };
        let placeholder = Exchange {
            last: FtpRequest {
                seq: 0,
                session: 0,
                opcode: 0,
                size: 0,
                offset: 0,
                data: Vec::new(),
            },
            attempts: 0,
            sent_us: now_us,
        };
        let upload = match &job {
            Job::WriteParams { writes } => {
                let params: Vec<Param> = writes.iter().map(|(_, p)| p.clone()).collect();
                encode_param_upload(&params)
            }
            Job::File {
                op: Op::Write { bytes, .. },
                ..
            } => bytes.clone(),
            _ => Vec::new(),
        };
        let from = match &job {
            Job::File {
                op: Op::Read { from, .. },
                ..
            } => *from,
            _ => 0,
        };
        let collect = matches!(job, Job::Params | Job::WriteParams { .. });
        self.active = Some(Active {
            job,
            step: Step::Reset,
            session: 0,
            open: false,
            exchange: placeholder,
            reading: Reading {
                next: from,
                collect: collect.then(Vec::new),
                ..Reading::default()
            },
            upload,
            written: 0,
            reading_back: false,
            reset_again: false,
            burst_seq: None,
            issued_us: now_us,
        });
        if self.reset_done {
            self.begin(now_us)
        } else {
            let reset = self.request(0, ftp::RESET_SESSIONS, 0, 0, Vec::new());
            self.sent(reset, now_us)
        }
    }

    /// Records a request of the running job and sends it.
    fn sent(&mut self, request: FtpRequest, now_us: u64) -> Output {
        if let Some(active) = self.active.as_mut() {
            active.exchange = Exchange {
                last: request.clone(),
                attempts: 1,
                sent_us: now_us,
            };
            active.step = match request.opcode {
                ftp::RESET_SESSIONS => Step::Reset,
                ftp::OPEN_FILE_RO => Step::Open,
                ftp::CREATE_FILE => Step::Create,
                ftp::WRITE_FILE => Step::Write,
                _ => Step::Read,
            };
        }
        Output {
            send: vec![request],
            events: Vec::new(),
        }
    }

    /// The running job's first request after sessions are reset: open to
    /// read, or create to write.
    fn begin(&mut self, now_us: u64) -> Output {
        let Some(active) = self.active.as_ref() else {
            return Output::default();
        };
        let (opcode, path) = match &active.job {
            Job::Params => (ftp::OPEN_FILE_RO, ftp::PARAM_PATH.to_owned()),
            Job::WriteParams { .. } if active.reading_back => {
                (ftp::OPEN_FILE_RO, ftp::PARAM_PATH.to_owned())
            }
            Job::WriteParams { .. } => (ftp::CREATE_FILE, ftp::PARAM_UPLOAD_PATH.to_owned()),
            Job::File {
                op: Op::Read { path, .. },
                ..
            } => (ftp::OPEN_FILE_RO, path.clone()),
            Job::File {
                op: Op::Write { path, .. },
                ..
            } => (ftp::CREATE_FILE, path.clone()),
            Job::File { .. } => return Output::default(),
        };
        let data = path.into_bytes();
        self.next_session = self.next_session % 200 + 1;
        let session = self.next_session;
        let request = self.request(session, opcode, data_size(&data), 0, data);
        self.sent(request, now_us)
    }

    /// Asks for more of the file: a gap one piece at a time, else a burst
    /// from where the bytes in order end.
    fn read_more(&mut self, now_us: u64) -> Output {
        let Some(active) = self.active.as_ref() else {
            return Output::default();
        };
        let reading = &active.reading;
        let gap = !reading.ahead.is_empty() || reading.end.is_some();
        let opcode = if gap {
            ftp::READ_FILE
        } else {
            ftp::BURST_READ_FILE
        };
        let offset = u32::try_from(reading.next).unwrap_or(u32::MAX);
        let session = active.session;
        let request = self.request(session, opcode, read_size(), offset, Vec::new());
        let burst = (opcode == ftp::BURST_READ_FILE).then_some(request.seq);
        if burst.is_some() {
            self.seq = request.seq.wrapping_add(BURST_ANSWERS);
        }
        if let Some(active) = self.active.as_mut() {
            active.burst_seq = burst;
        }
        self.sent(request, now_us)
    }

    /// The running job ended: close its file, report it, start the next.
    fn finish(&mut self, result: Result<(), Failure>, now_us: u64) -> Output {
        let job = self.active.as_ref().map(|a| a.job.clone());
        let reading = self
            .active
            .as_mut()
            .map(|a| std::mem::take(&mut a.reading))
            .unwrap_or_default();
        let mut out = Output {
            send: self.close_active(),
            events: Vec::new(),
        };
        let parsed = || -> Result<Vec<WithDefault>, String> {
            match &result {
                Ok(()) => parse_param_pck(reading.collect.as_deref().unwrap_or_default()),
                Err(failure) => Err(failure.to_string()),
            }
        };
        match job {
            Some(Job::Params) => out.events.push(Event::Parameters(parsed())),
            Some(Job::WriteParams { writes }) => out.events.push(Event::ParamsWritten {
                writes,
                read_back: parsed(),
            }),
            Some(Job::File { id, .. }) => out.events.push(Event::Done { id, result }),
            None => {}
        }
        out.extend(self.start_next(now_us));
        out
    }

    /// An answer from the vehicle, received at `received_us` and handled
    /// at `now_us`.
    pub fn answer(&mut self, answer: &ftp::Answer, received_us: u64, now_us: u64) -> Output {
        if let Some(quick) = self.quick.as_ref()
            && quick.exchange.last.opcode == answer.req_opcode
        {
            if received_us < quick.issued_us {
                return Output::default();
            }
            return self.quick_answer(answer, now_us);
        }
        let Some(active) = self.active.as_ref() else {
            return Output::default();
        };
        if received_us < active.issued_us {
            return Output::default();
        }
        let last = &active.exchange.last;
        let bursting = answer.req_opcode == ftp::BURST_READ_FILE
            && active.step == Step::Read
            && answer.session == active.session
            && active
                .burst_seq
                .is_some_and(|seq| answer.seq.wrapping_sub(seq).wrapping_sub(1) < BURST_ANSWERS);
        let expected = answer.req_opcode == last.opcode && answer.seq == last.seq.wrapping_add(1);
        if !expected && !bursting {
            return Output::default();
        }
        if answer.opcode == ftp::NAK {
            return self.session_nak(answer, now_us);
        }
        if answer.opcode != ftp::ACK {
            return Output::default();
        }
        match answer.req_opcode {
            ftp::RESET_SESSIONS => {
                self.reset_done = true;
                self.begin(now_us)
            }
            ftp::OPEN_FILE_RO => self.opened(answer, now_us),
            ftp::CREATE_FILE => {
                if let Some(active) = self.active.as_mut() {
                    active.session = answer.session;
                    active.open = true;
                }
                self.write_more(now_us)
            }
            ftp::WRITE_FILE => {
                let mut out = Output::default();
                if let Some(active) = self.active.as_mut() {
                    let len = usize::from(active.exchange.last.size);
                    active.written = (active.written + len).min(active.upload.len());
                    if let Job::File { id, .. } = &active.job {
                        out.events.push(Event::Sent {
                            id: id.clone(),
                            done: active.written as u64,
                            total: active.upload.len() as u64,
                        });
                    }
                }
                out.extend(self.write_more(now_us));
                out
            }
            ftp::READ_FILE | ftp::BURST_READ_FILE => self.read_answer(answer, now_us),
            _ => Output::default(),
        }
    }

    fn opened(&mut self, answer: &ftp::Answer, now_us: u64) -> Output {
        let mut out = Output::default();
        let Some(active) = self.active.as_mut() else {
            return out;
        };
        active.session = answer.session;
        active.open = true;
        // The parameter file's size is only estimated: not reported.
        if let Job::File { id, .. } = &active.job {
            let size = answer
                .data
                .get(..4)
                .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
            out.events.push(Event::Opened {
                id: id.clone(),
                size: u64::from(size),
            });
        }
        out.extend(self.read_more(now_us));
        out
    }

    /// The next piece of a write, or the close once it is all taken.
    fn write_more(&mut self, now_us: u64) -> Output {
        let Some(active) = self.active.as_ref() else {
            return Output::default();
        };
        if active.written >= active.upload.len() {
            if matches!(active.job, Job::WriteParams { .. }) {
                // Written: close it (ArduPilot sets them now), then read
                // the parameters back.
                let close = self.close_active_keep();
                let mut out = Output {
                    send: close,
                    events: Vec::new(),
                };
                out.extend(self.begin(now_us));
                return out;
            }
            return self.finish(Ok(()), now_us);
        }
        let end = (active.written + ftp::DATA_MAX).min(active.upload.len());
        let data = active.upload[active.written..end].to_vec();
        let offset = u32::try_from(active.written).unwrap_or(u32::MAX);
        let session = active.session;
        let request = self.request(session, ftp::WRITE_FILE, data_size(&data), offset, data);
        self.sent(request, now_us)
    }

    /// Closes the running job's file but keeps the job, for the read-back.
    fn close_active_keep(&mut self) -> Vec<FtpRequest> {
        let Some((session, open)) = self.active.as_ref().map(|a| (a.session, a.open)) else {
            return Vec::new();
        };
        if let Some(active) = self.active.as_mut() {
            active.open = false;
            active.reading_back = true;
        }
        if open {
            vec![self.request(session, ftp::TERMINATE_SESSION, 0, 0, Vec::new())]
        } else {
            Vec::new()
        }
    }

    fn read_answer(&mut self, answer: &ftp::Answer, now_us: u64) -> Output {
        let Some(active) = self.active.as_mut() else {
            return Output::default();
        };
        let before = active.reading.next;
        let offset = u64::from(answer.offset);
        if answer.data.is_empty() {
            active.reading.end = Some(active.reading.end.map_or(offset, |end| end.min(offset)));
        }
        let fresh = active.reading.take(offset, &answer.data);
        let mut out = Output::default();
        if !fresh.is_empty() {
            active.exchange.attempts = 1;
            active.exchange.sent_us = now_us;
            if let Some(collect) = active.reading.collect.as_mut() {
                collect.extend_from_slice(&fresh);
                if collect.len() > PARAM_FILE_MAX {
                    return self.finish(
                        Err(Failure::Rejected("the parameter file does not end".into())),
                        now_us,
                    );
                }
            } else if let Job::File { id, .. } = &active.job {
                out.events.push(Event::Data {
                    id: id.clone(),
                    offset: before,
                    bytes: fresh,
                });
            }
        } else if !answer.data.is_empty() {
            // Early: progress all the same.
            active.exchange.sent_us = now_us;
        }
        if active.reading.complete() {
            out.extend(self.finish(Ok(()), now_us));
            return out;
        }
        // A single read answered, or a burst ended: ask for more. In the
        // middle of a burst, wait for the rest.
        let more = answer.req_opcode == ftp::READ_FILE || answer.burst_complete;
        if more {
            out.extend(self.read_more(now_us));
        }
        out
    }

    fn session_nak(&mut self, answer: &ftp::Answer, now_us: u64) -> Output {
        let error = answer.data.first().copied().unwrap_or(ftp::FAIL);
        let Some(active) = self.active.as_mut() else {
            return Output::default();
        };
        match answer.req_opcode {
            // A vehicle that does not reset sessions still serves files.
            ftp::RESET_SESSIONS => {
                self.reset_done = true;
                self.begin(now_us)
            }
            // The end is where the vehicle says, even when the last pieces
            // before it were lost on the way.
            ftp::READ_FILE | ftp::BURST_READ_FILE if error == ftp::EOF => {
                let end = active.reading.furthest().max(u64::from(answer.offset));
                active.reading.end = Some(active.reading.end.map_or(end, |e| e.max(end)));
                if active.reading.complete() {
                    self.finish(Ok(()), now_us)
                } else {
                    self.read_more(now_us)
                }
            }
            // Our last close never arrived, or another station's session
            // is open: reset once and open again.
            ftp::OPEN_FILE_RO | ftp::CREATE_FILE
                if matches!(error, ftp::NO_SESSIONS_AVAILABLE | ftp::FAIL)
                    && !active.reset_again =>
            {
                active.reset_again = true;
                self.reset_done = false;
                let reset = self.request(0, ftp::RESET_SESSIONS, 0, 0, Vec::new());
                self.sent(reset, now_us)
            }
            _ => {
                let words = ftp::error_text(&answer.data);
                self.finish(Err(Failure::Rejected(words)), now_us)
            }
        }
    }

    /// Starts the next session-less request when none runs.
    fn start_quick(&mut self, now_us: u64) -> Output {
        if self.quick.is_some() {
            return Output::default();
        }
        let Some((id, op)) = self.quick_queue.pop_front() else {
            return Output::default();
        };
        let request = self.quick_request(&op, 0);
        self.quick = Some(Quick {
            id,
            op,
            exchange: Exchange {
                last: request.clone(),
                attempts: 1,
                sent_us: now_us,
            },
            entries: Vec::new(),
            listed: 0,
            issued_us: now_us,
        });
        Output {
            send: vec![request],
            events: Vec::new(),
        }
    }

    fn quick_request(&mut self, op: &Op, listed: u32) -> FtpRequest {
        let (opcode, data) = match op {
            Op::List { path } => (ftp::LIST_DIRECTORY, path.clone().into_bytes()),
            Op::Remove { path } => (ftp::REMOVE_FILE, path.clone().into_bytes()),
            Op::MakeDir { path } => (ftp::CREATE_DIRECTORY, path.clone().into_bytes()),
            Op::RemoveDir { path } => (ftp::REMOVE_DIRECTORY, path.clone().into_bytes()),
            Op::Crc { path } => (ftp::CALC_FILE_CRC32, path.clone().into_bytes()),
            Op::Rename { from, to } => {
                let mut data = from.clone().into_bytes();
                data.push(0);
                data.extend_from_slice(to.as_bytes());
                (ftp::RENAME, data)
            }
            Op::Read { .. } | Op::Write { .. } => (ftp::LIST_DIRECTORY, Vec::new()),
        };
        self.request(0, opcode, data_size(&data), listed, data)
    }

    fn quick_answer(&mut self, answer: &ftp::Answer, now_us: u64) -> Output {
        let Some(quick) = self.quick.as_mut() else {
            return Output::default();
        };
        if answer.seq != quick.exchange.last.seq.wrapping_add(1) {
            return Output::default();
        }
        let id = quick.id.clone();
        let mut events = Vec::new();
        let result = match (answer.opcode, &quick.op) {
            (ftp::ACK, Op::List { .. }) => {
                let (entries, count) = parse_listing(&answer.data);
                if count == 0 {
                    events.push(Event::Listed {
                        id: id.clone(),
                        entries: std::mem::take(&mut quick.entries),
                    });
                    Ok(())
                } else {
                    quick.entries.extend(entries);
                    quick.listed += count;
                    let (op, listed) = (quick.op.clone(), quick.listed);
                    let request = self.quick_request(&op, listed);
                    if let Some(quick) = self.quick.as_mut() {
                        quick.exchange = Exchange {
                            last: request.clone(),
                            attempts: 1,
                            sent_us: now_us,
                        };
                    }
                    return Output {
                        send: vec![request],
                        events,
                    };
                }
            }
            (ftp::NAK, Op::List { .. }) if answer.data.first() == Some(&ftp::EOF) => {
                events.push(Event::Listed {
                    id: id.clone(),
                    entries: std::mem::take(&mut quick.entries),
                });
                Ok(())
            }
            (ftp::ACK, Op::Crc { .. }) => {
                let crc = answer
                    .data
                    .get(..4)
                    .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                events.push(Event::Crc {
                    id: id.clone(),
                    crc,
                });
                Ok(())
            }
            (ftp::ACK, _) => Ok(()),
            // Asked again after a lost answer: the first ask did it.
            (ftp::NAK, Op::Remove { .. } | Op::RemoveDir { .. } | Op::Rename { .. })
                if quick.exchange.attempts > 1 && gone(&answer.data) =>
            {
                Ok(())
            }
            (ftp::NAK, Op::MakeDir { .. })
                if quick.exchange.attempts > 1 && exists(&answer.data) =>
            {
                Ok(())
            }
            (ftp::NAK, _) => Err(Failure::Rejected(ftp::error_text(&answer.data))),
            _ => return Output::default(),
        };
        events.push(Event::Done { id, result });
        self.quick = None;
        let mut out = Output {
            send: Vec::new(),
            events,
        };
        out.extend(self.start_quick(now_us));
        out
    }

    /// Sends again after a silence; gives up after every attempt.
    pub fn tick(&mut self, now_us: u64, timing: &Timing) -> Output {
        let mut out = Output::default();
        if let Some(quick) = self.quick.as_mut()
            && now_us.saturating_sub(quick.exchange.sent_us) >= timing.timeout_us
        {
            if quick.exchange.attempts >= timing.attempts {
                let id = quick.id.clone();
                self.quick = None;
                // Nothing to wait for: say so, as the tab and the agent read it.
                out.events.push(Event::Done {
                    id,
                    result: Err(Failure::Rejected(
                        "the vehicle did not answer file transfer".into(),
                    )),
                });
                out.extend(self.start_quick(now_us));
            } else {
                quick.exchange.attempts += 1;
                quick.exchange.sent_us = now_us;
                out.send.push(quick.exchange.last.clone());
            }
        }
        if let Some(active) = self.active.as_mut()
            && now_us.saturating_sub(active.exchange.sent_us) >= timing.timeout_us
        {
            if active.exchange.attempts >= timing.attempts {
                let failure = match active.step {
                    Step::Reset => {
                        Failure::Rejected("the vehicle did not answer file transfer".into())
                    }
                    _ => Failure::Timeout,
                };
                out.extend(self.finish(Err(failure), now_us));
            } else if active.step == Step::Read {
                // A stalled burst: ask again from where the bytes end.
                let attempts = active.exchange.attempts + 1;
                out.extend(self.read_more(now_us));
                if let Some(active) = self.active.as_mut() {
                    active.exchange.attempts = attempts;
                }
            } else {
                active.exchange.attempts += 1;
                active.exchange.sent_us = now_us;
                out.send.push(active.exchange.last.clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests;
