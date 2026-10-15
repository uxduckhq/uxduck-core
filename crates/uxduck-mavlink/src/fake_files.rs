//! The fake vehicle's storage over MAVLink FTP (S121), answered as
//! ArduPilot answers: an SD card with three logs and a scripts folder, the
//! read-only `@SYS` files, and `@PARAM/param.pck` both ways. Bursts number
//! each answer one past the one before and end with `burst_complete`, or a
//! `NAK` `EOF` at the file's end. For tests and `uxduck dev fake-vehicle`.

use std::collections::{BTreeMap, BTreeSet};

use crate::transfers::{FtpRequest, ftp};

/// What the vehicle's own `@SYS/uarts.txt` says, in ArduPilot's words.
const UARTS: &str = "UARTV1\n\
SERIAL0 OTG1  TX =   23716 RX =    1052 TXBD=  1895 RXBD=    84 FlowCtrl=0\n\
SERIAL1 UART7 TX*=   19870 RX*=     210 TXBD=  1589 RXBD=    16 FlowCtrl=0\n\
SERIAL2 UART5 TX =       0 RX =     513 TXBD=     0 RXBD=    41 FlowCtrl=0\n\
SERIAL3 USART1 TX =      0 RX =   40213 TXBD=     0 RXBD=  3216 FlowCtrl=0\n";

const THREADS: &str = "ThreadsV2\n\
monitor          PRI=183 sp=0x30000560 STACK=344/464\n\
timer            PRI=181 sp=0x30000890 STACK=416/944\n\
rcout            PRI=181 sp=0x30000BC8 STACK=392/944\n\
main             PRI=180 sp=0x2001FB48 STACK=4864/7680\n\
log_io           PRI= 59 sp=0x30020A00 STACK=1024/1440\n";

/// A file open in a session.
#[derive(Clone, Debug)]
enum Open {
    Read { bytes: Vec<u8>, read_size: u8 },
    Write { path: String, bytes: Vec<u8> },
}

/// What the vehicle stores, and its one open file.
#[derive(Clone, Debug)]
pub struct FakeFiles {
    /// Files by path, `/APM/LOGS/00000001.BIN` style.
    pub files: BTreeMap<String, Vec<u8>>,
    /// Folders by path, empty ones included.
    pub dirs: BTreeSet<String>,
    /// The session open, by its number.
    open: Option<(u8, Open)>,
    /// Loses every nth answer, as a lossy radio does (`--drop-ftp`).
    pub drop_every: Option<usize>,
    answers: usize,
    /// The most answers to one burst request (ArduPilot's is 2000).
    pub burst_max: usize,
    /// Reads, single and burst requests, for tests.
    pub reads: u32,
    pub bursts: u32,
    /// Parameter files written to it, for tests.
    pub param_uploads: u32,
}

/// A log's bytes: a `DataFlash` header and a pattern, the same each time.
fn log_bytes(number: u32, len: usize) -> Vec<u8> {
    let mut bytes = vec![0xa3, 0x95, 0x80, 0x80];
    bytes.extend(format!("ArduCopter V4.5.1 log {number}\n").into_bytes());
    let mut state = number.wrapping_mul(2_654_435_761);
    while bytes.len() < len {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        bytes.push(state.to_le_bytes()[0]);
    }
    bytes.truncate(len);
    bytes
}

/// Paths as the fake stores them: one leading slash for the SD card, none
/// for the `@` folders, no trailing slash, `/` the root.
fn normal(path: &str) -> String {
    let trimmed = path.trim().trim_matches('/');
    let mut out = String::new();
    for part in trimmed.split('/').filter(|p| !p.is_empty() && *p != ".") {
        out.push('/');
        out.push_str(part);
    }
    if out.is_empty() {
        return "/".into();
    }
    if out.starts_with("/@") {
        out.remove(0);
    }
    out
}

fn parent(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => parent.into(),
        _ => "/".into(),
    }
}

impl Default for FakeFiles {
    fn default() -> Self {
        let mut files = BTreeMap::new();
        for (number, len) in [(1, 3_000), (2, 20_000), (3, 60_000)] {
            files.insert(format!("/APM/LOGS/{number:08}.BIN"), log_bytes(number, len));
        }
        files.insert("/APM/LOGS/LASTLOG.TXT".into(), b"3\n".to_vec());
        files.insert("@SYS/uarts.txt".into(), UARTS.as_bytes().to_vec());
        files.insert("@SYS/threads.txt".into(), THREADS.as_bytes().to_vec());
        let dirs = ["/", "/APM", "/APM/LOGS", "/APM/scripts", "@SYS", "@PARAM"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        Self {
            files,
            dirs,
            open: None,
            drop_every: None,
            answers: 0,
            burst_max: 2000,
            reads: 0,
            bursts: 0,
            param_uploads: 0,
        }
    }
}

/// What a request changed: a parameter file written to `@PARAM`.
#[derive(Debug, Default)]
pub struct Answered {
    /// The payloads to send, in order.
    pub payloads: Vec<[u8; 251]>,
    /// A `param.pck` someone wrote, to set its parameters.
    pub params_written: Option<Vec<u8>>,
}

impl FakeFiles {
    fn read_only(path: &str) -> bool {
        path.starts_with("@SYS") || path.starts_with("@ROMFS")
    }

    fn listing(&self, dir: &str) -> Vec<String> {
        let prefix = if dir == "/" {
            "/".to_owned()
        } else {
            format!("{dir}/")
        };
        let child = |path: &str| -> Option<String> {
            let rest = path.strip_prefix(&prefix)?;
            (!rest.is_empty() && !rest.contains('/')).then(|| rest.to_owned())
        };
        let mut entries = Vec::new();
        for path in &self.dirs {
            // The root lists the card's folders and, as ArduPilot does, its
            // own `@` folders.
            if let Some(name) = child(path) {
                entries.push(format!("D{name}"));
            } else if dir == "/" && path.starts_with('@') {
                entries.push(format!("D{path}"));
            }
        }
        for (path, bytes) in &self.files {
            if let Some(name) = child(path) {
                entries.push(format!("F{name}\t{}", bytes.len()));
            }
        }
        entries
    }

    /// Answers one request. `param_pck` makes the parameter file when it
    /// is opened.
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per opcode, as the protocol lists them"
    )]
    pub fn answer(
        &mut self,
        request: &ftp::Answer,
        param_pck: impl FnOnce() -> Vec<u8>,
    ) -> Answered {
        let mut out = Answered::default();
        let mut replies: Vec<(FtpRequest, bool)> = Vec::new();
        let reply = |opcode: u8, offset: u32, data: Vec<u8>, seq_after: u16| FtpRequest {
            seq: request.seq.wrapping_add(seq_after),
            session: request.session,
            opcode,
            size: u8::try_from(data.len()).unwrap_or(u8::MAX),
            offset,
            data,
        };
        let ack = |data: Vec<u8>| reply(ftp::ACK, request.offset, data, 1);
        let nak = |error: &[u8]| reply(ftp::NAK, request.offset, error.to_vec(), 1);
        let path = normal(&String::from_utf8_lossy(&request.data));
        let raw = String::from_utf8_lossy(&request.data).into_owned();
        match request.opcode {
            ftp::RESET_SESSIONS => {
                self.open = None;
                replies.push((ack(Vec::new()), false));
            }
            ftp::LIST_DIRECTORY => {
                if self.dirs.contains(&path) {
                    let skip = usize::try_from(request.offset).unwrap_or(usize::MAX);
                    let mut data = Vec::new();
                    for entry in self.listing(&path).into_iter().skip(skip) {
                        if data.len() + entry.len() + 1 > ftp::DATA_MAX {
                            break;
                        }
                        data.extend(entry.into_bytes());
                        data.push(0);
                    }
                    replies.push((
                        if data.is_empty() {
                            nak(&[ftp::EOF])
                        } else {
                            ack(data)
                        },
                        false,
                    ));
                } else {
                    replies.push((nak(&[ftp::FAIL_ERRNO, 2]), false));
                }
            }
            ftp::OPEN_FILE_RO => {
                if self.open.is_some() {
                    replies.push((nak(&[ftp::FAIL]), false));
                } else if raw.starts_with("@PARAM/param.pck") {
                    let bytes = param_pck();
                    // ArduPilot estimates the size, 12 bytes a parameter.
                    let size = u32::try_from(bytes.len() / 2).unwrap_or(u32::MAX);
                    self.open = Some((
                        request.session,
                        Open::Read {
                            bytes,
                            read_size: 0,
                        },
                    ));
                    replies.push((ack(size.to_le_bytes().to_vec()), false));
                } else if let Some(bytes) = self.files.get(&path) {
                    let size = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
                    self.open = Some((
                        request.session,
                        Open::Read {
                            bytes: bytes.clone(),
                            read_size: 0,
                        },
                    ));
                    replies.push((ack(size.to_le_bytes().to_vec()), false));
                } else {
                    replies.push((nak(&[ftp::FAIL_ERRNO, 2]), false));
                }
            }
            ftp::READ_FILE | ftp::BURST_READ_FILE => {
                let burst = request.opcode == ftp::BURST_READ_FILE;
                if burst {
                    self.bursts += 1;
                } else {
                    self.reads += 1;
                }
                match self.open.as_mut() {
                    Some((session, Open::Read { bytes, read_size }))
                        if *session == request.session =>
                    {
                        // The parameter file keeps its first read size.
                        if *read_size == 0 {
                            *read_size = request.size;
                        }
                        let want = usize::from(if request.size == 0 {
                            u8::try_from(ftp::DATA_MAX).unwrap_or(u8::MAX)
                        } else {
                            request.size
                        })
                        .min(ftp::DATA_MAX);
                        let mut at = usize::try_from(request.offset).unwrap_or(usize::MAX);
                        let rounds = if burst { self.burst_max } else { 1 };
                        let mut after = 1;
                        for round in 0..rounds {
                            let offset = u32::try_from(at).unwrap_or(u32::MAX);
                            if at >= bytes.len() {
                                let mut eof = reply(ftp::NAK, offset, vec![ftp::EOF], after);
                                eof.session = request.session;
                                replies.push((eof, burst));
                                break;
                            }
                            let end = (at + want).min(bytes.len());
                            let data = bytes[at..end].to_vec();
                            let last = burst && round + 1 == rounds;
                            replies.push((reply(ftp::ACK, offset, data, after), last));
                            after = after.wrapping_add(1);
                            at = end;
                            if !burst {
                                break;
                            }
                        }
                    }
                    _ => replies.push((nak(&[ftp::INVALID_SESSION]), false)),
                }
            }
            ftp::CREATE_FILE | ftp::OPEN_FILE_WO => {
                let params = raw == ftp::PARAM_UPLOAD_PATH;
                if self.open.is_some() {
                    replies.push((nak(&[ftp::FAIL]), false));
                } else if Self::read_only(&path) {
                    replies.push((nak(&[ftp::FAIL_ERRNO, 30]), false));
                } else if !params && !self.dirs.contains(&parent(&path)) {
                    replies.push((nak(&[ftp::FAIL_ERRNO, 2]), false));
                } else {
                    let target = if params { raw.clone() } else { path.clone() };
                    let bytes = if request.opcode == ftp::OPEN_FILE_WO {
                        self.files.get(&path).cloned().unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    self.open = Some((
                        request.session,
                        Open::Write {
                            path: target,
                            bytes,
                        },
                    ));
                    replies.push((ack(Vec::new()), false));
                }
            }
            ftp::WRITE_FILE => match self.open.as_mut() {
                Some((session, Open::Write { bytes, .. })) if *session == request.session => {
                    let at = usize::try_from(request.offset).unwrap_or(usize::MAX);
                    let len = usize::from(request.size).min(request.data.len());
                    if bytes.len() < at + len {
                        bytes.resize(at + len, 0);
                    }
                    bytes[at..at + len].copy_from_slice(&request.data[..len]);
                    replies.push((ack(Vec::new()), false));
                }
                _ => replies.push((nak(&[ftp::INVALID_SESSION]), false)),
            },
            ftp::TERMINATE_SESSION => {
                if let Some((_, Open::Write { path, bytes })) = self.open.take() {
                    if path == ftp::PARAM_UPLOAD_PATH {
                        self.param_uploads += 1;
                        out.params_written = Some(bytes);
                    } else {
                        self.files.insert(path, bytes);
                    }
                }
                replies.push((ack(Vec::new()), false));
            }
            ftp::REMOVE_FILE => {
                if Self::read_only(&path) {
                    replies.push((nak(&[ftp::FAIL_ERRNO, 30]), false));
                } else if self.files.remove(&path).is_some() {
                    replies.push((ack(Vec::new()), false));
                } else {
                    replies.push((nak(&[ftp::FAIL_ERRNO, 2]), false));
                }
            }
            ftp::CREATE_DIRECTORY => {
                if Self::read_only(&path) || !self.dirs.contains(&parent(&path)) {
                    replies.push((nak(&[ftp::FAIL_ERRNO, 2]), false));
                } else if self.dirs.contains(&path) || self.files.contains_key(&path) {
                    replies.push((nak(&[ftp::FAIL_ERRNO, 17]), false));
                } else {
                    self.dirs.insert(path);
                    replies.push((ack(Vec::new()), false));
                }
            }
            ftp::REMOVE_DIRECTORY => {
                if !self.dirs.contains(&path) || path == "/" || Self::read_only(&path) {
                    replies.push((nak(&[ftp::FAIL_ERRNO, 2]), false));
                } else if !self.listing(&path).is_empty() {
                    replies.push((nak(&[ftp::FAIL_ERRNO, 39]), false));
                } else {
                    self.dirs.remove(&path);
                    replies.push((ack(Vec::new()), false));
                }
            }
            ftp::RENAME => {
                let len = usize::from(request.size).min(request.data.len());
                let text = String::from_utf8_lossy(&request.data[..len]).into_owned();
                let (from, to) = text.split_once('\0').unwrap_or((&text, ""));
                let (from, to) = (normal(from), normal(to));
                if Self::read_only(&from) || Self::read_only(&to) {
                    replies.push((nak(&[ftp::FAIL_ERRNO, 30]), false));
                } else if let Some(bytes) = self.files.remove(&from) {
                    self.files.insert(to, bytes);
                    replies.push((ack(Vec::new()), false));
                } else {
                    replies.push((nak(&[ftp::FAIL_ERRNO, 2]), false));
                }
            }
            ftp::CALC_FILE_CRC32 => match self.files.get(&path) {
                Some(bytes) => replies.push((ack(crc32(bytes).to_le_bytes().to_vec()), false)),
                None => replies.push((nak(&[ftp::FAIL_ERRNO, 2]), false)),
            },
            _ => replies.push((nak(&[ftp::UNKNOWN_COMMAND]), false)),
        }
        for (reply, burst_complete) in replies {
            self.answers += 1;
            if self
                .drop_every
                .is_some_and(|n| n > 0 && self.answers.is_multiple_of(n))
            {
                continue;
            }
            let mut payload = ftp::payload(&reply);
            payload[5] = request.opcode;
            payload[6] = u8::from(burst_complete);
            out.payloads.push(payload);
        }
        out
    }
}

/// CRC-32 (IEEE), for the fake's `CalcFileCRC32`.
#[must_use]
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}
