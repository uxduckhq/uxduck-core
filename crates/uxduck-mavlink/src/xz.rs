//! xz files unpacked within a limit (ADR-0021 P12).
//!
//! PX4's metadata comes from the vehicle, so a packed file is hostile input:
//! a few kilobytes can say they unpack to gigabytes. A file is measured
//! before it is unpacked. Each of its chunks states what it unpacks to, and
//! a file whose chunks add up to more than the limit is refused with nothing
//! unpacked.
//!
//! The same walk that measures then drives the unpacking, chunk by chunk. A
//! chunk unpacks to exactly what it states, from exactly the bytes it
//! states, or the file is refused. So what is unpacked is what was
//! measured. The output is its own dictionary, so memory is the output, at
//! most the limit, and 30 KB of tables. Every step reads a byte or writes
//! one, so the work is in proportion to the file's bytes plus the limit.
//!
//! A measure cannot bind `lzma-rs` this way, which is why it is not used
//! here. It unpacks a whole block in memory before it writes a byte, and it
//! does not hold a chunk to the bytes its header states: what a chunk
//! leaves unread is read as the next chunk's header.
//!
//! This reads what `xz` writes by default: one stream, its blocks packed
//! by LZMA2 alone and checked by CRC32, CRC64 or nothing. Anything else is
//! refused: another filter, a SHA-256 check, a second stream or padding
//! after the first, and any header, size or check that does not agree.

/// How an xz file begins.
const MAGIC: &[u8; 6] = b"\xFD7zXZ\0";

/// Why a packed file was not unpacked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Its chunks say they unpack to more than the limit. Nothing was
    /// unpacked.
    TooLarge,
    /// It is not an xz file this reads, or it is damaged. Why, in words
    /// that follow "the file".
    Unreadable(&'static str),
}

const CUT_SHORT: Refused = Refused::Unreadable("is cut short");
const HEADER: Refused = Refused::Unreadable("has a damaged header");
const DATA: Refused = Refused::Unreadable("has damaged data");
const FILTER: Refused = Refused::Unreadable("is packed with a filter other than LZMA2");
const CHECK: Refused = Refused::Unreadable("has a check of a kind that is not verified here");
const MORE: Refused = Refused::Unreadable("holds more than one stream");

/// Whether these bytes begin as an xz file does.
#[must_use]
pub fn is_xz(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

/// An xz file's contents, when its chunks say they unpack to no more than
/// `limit` bytes. A file that says more is refused before any of it is
/// unpacked.
///
/// # Errors
///
/// [`Refused::TooLarge`] for a file past the limit, and
/// [`Refused::Unreadable`] for one that is not an xz file this reads or
/// that is damaged.
pub fn unpack(bytes: &[u8], limit: usize) -> Result<Vec<u8>, Refused> {
    let total = Walk::new(bytes, limit, None).stream()?.total;
    let walked = Walk::new(bytes, limit, Some(Vec::with_capacity(total))).stream()?;
    Ok(walked.out.unwrap_or_default())
}

/// A place in the file, read forward.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], Refused> {
        let end = self.at.checked_add(count).ok_or(CUT_SHORT)?;
        let taken = self.bytes.get(self.at..end).ok_or(CUT_SHORT)?;
        self.at = end;
        Ok(taken)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Refused> {
        self.take(N)?.try_into().map_err(|_| CUT_SHORT)
    }

    fn byte(&mut self) -> Result<u8, Refused> {
        Ok(u8::from_le_bytes(self.array()?))
    }

    /// A chunk's size as it is stored: big-endian, and one less than it is.
    fn size(&mut self) -> Result<usize, Refused> {
        Ok(usize::from(u16::from_be_bytes(self.array()?)) + 1)
    }

    fn u32(&mut self) -> Result<u32, Refused> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    /// The format's own integer: seven bits a byte, least first, nine
    /// bytes at most, and no longer than it needs to be.
    fn integer(&mut self) -> Result<u64, Refused> {
        let mut value = 0;
        for place in 0..9 {
            let byte = self.byte()?;
            if byte == 0 && place != 0 {
                return Err(HEADER);
            }
            value |= u64::from(byte & 0x7F) << (place * 7);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(HEADER)
    }

    /// The zero bytes that bring what began at `from` to a multiple of
    /// four.
    fn padding(&mut self, from: usize) -> Result<(), Refused> {
        while !self.at.saturating_sub(from).is_multiple_of(4) {
            if self.byte()? != 0 {
                return Err(HEADER);
            }
        }
        Ok(())
    }
}

/// How a stream's blocks are checked.
#[derive(Clone, Copy)]
enum Check {
    None,
    Crc32,
    Crc64,
}

impl Check {
    fn len(self) -> usize {
        match self {
            Self::None => 0,
            Self::Crc32 => 4,
            Self::Crc64 => 8,
        }
    }

    fn holds(self, data: &[u8], field: &[u8]) -> bool {
        match self {
            Self::None => true,
            Self::Crc32 => field == crc32(data).to_le_bytes(),
            Self::Crc64 => field == crc64(data).to_le_bytes(),
        }
    }
}

/// What a block's header says of it.
struct Block {
    /// How far back its chunks may reach.
    dictionary: u32,
    packed: Option<u64>,
    unpacked: Option<u64>,
}

impl Block {
    /// A block's header without its first byte, which is its length, and
    /// without its check.
    fn read(fields: &[u8]) -> Result<Self, Refused> {
        let mut at = Cursor {
            bytes: fields,
            at: 0,
        };
        let flags = at.byte()?;
        if flags & 0x3C != 0 {
            return Err(HEADER);
        }
        if flags & 0x03 != 0 {
            return Err(FILTER);
        }
        let packed = (flags & 0x40 != 0).then(|| at.integer()).transpose()?;
        let unpacked = (flags & 0x80 != 0).then(|| at.integer()).transpose()?;
        if at.integer()? != 0x21 {
            return Err(FILTER);
        }
        if at.integer()? != 1 {
            return Err(HEADER);
        }
        let dictionary = match at.byte()? {
            size @ 0..=39 => (2 | u32::from(size & 1)) << (size / 2 + 11),
            40 => u32::MAX,
            _ => return Err(HEADER),
        };
        if fields
            .get(at.at..)
            .ok_or(HEADER)?
            .iter()
            .any(|byte| *byte != 0)
        {
            return Err(HEADER);
        }
        Ok(Self {
            dictionary,
            packed,
            unpacked,
        })
    }
}

/// One pass over a file's stream.
struct Walk<'a> {
    input: Cursor<'a>,
    limit: usize,
    /// What the chunks walked so far say they unpack to.
    total: usize,
    /// Where the file is unpacked to. A walk without it only measures.
    out: Option<Vec<u8>>,
    lzma: Option<Lzma>,
}

impl<'a> Walk<'a> {
    fn new(bytes: &'a [u8], limit: usize, out: Option<Vec<u8>>) -> Self {
        Self {
            input: Cursor { bytes, at: 0 },
            limit,
            total: 0,
            out,
            lzma: None,
        }
    }

    /// The whole stream: its header, its blocks, its index and its footer,
    /// and nothing after.
    fn stream(mut self) -> Result<Self, Refused> {
        if self.input.take(MAGIC.len()).ok() != Some(MAGIC.as_slice()) {
            return Err(Refused::Unreadable("is not an xz file"));
        }
        let flags: [u8; 2] = self.input.array()?;
        if self.input.u32()? != crc32(&flags) {
            return Err(HEADER);
        }
        let check = match flags {
            [0, 0] => Check::None,
            [0, 1] => Check::Crc32,
            [0, 4] => Check::Crc64,
            [0, kind] if kind < 16 => return Err(CHECK),
            _ => return Err(HEADER),
        };
        let mut blocks = 0;
        let mut sizes = SIZES;
        let index_at = loop {
            let block_at = self.input.at;
            let length = self.input.byte()?;
            if length == 0 {
                break block_at;
            }
            let (unpadded, unpacked) = self.block(block_at, length, check)?;
            sizes = pair(sizes, unpadded, unpacked);
            blocks += 1;
        };
        self.index(index_at, blocks, sizes)?;
        let index_len = self.input.at - index_at;
        let crc = self.input.u32()?;
        let rest: [u8; 6] = self.input.array()?;
        let [a, b, c, d, first, second] = rest;
        let backward = (u64::from(u32::from_le_bytes([a, b, c, d])) + 1) * 4;
        if crc != crc32(&rest)
            || backward != wide(index_len)
            || [first, second] != flags
            || self.input.take(2)? != b"YZ"
        {
            return Err(HEADER);
        }
        if self.input.at != self.input.bytes.len() {
            return Err(MORE);
        }
        Ok(self)
    }

    /// One block, whose header begins at `block_at` with `length`. Gives
    /// its size without its padding and what it unpacks to, which the
    /// index states again.
    fn block(&mut self, block_at: usize, length: u8, check: Check) -> Result<(u64, u64), Refused> {
        // Its length is stored in fours, less one, and counts its first
        // byte and its four-byte check.
        let fields = self.input.take(usize::from(length) * 4 - 1)?;
        let all = self
            .input
            .bytes
            .get(block_at..self.input.at)
            .ok_or(CUT_SHORT)?;
        if self.input.u32()? != crc32(all) {
            return Err(HEADER);
        }
        let block = Block::read(fields).map_err(|refused| match refused {
            Refused::Unreadable("is cut short") => HEADER,
            other => other,
        })?;
        let data_at = self.input.at;
        let before = self.total;
        self.chunks(&block)?;
        let packed = wide(self.input.at - data_at);
        let unpacked = wide(self.total - before);
        if block.packed.is_some_and(|stated| stated != packed)
            || block.unpacked.is_some_and(|stated| stated != unpacked)
        {
            return Err(HEADER);
        }
        self.input.padding(data_at)?;
        let field = self.input.take(check.len())?;
        if let Some(out) = &self.out
            && !check.holds(out.get(before..).ok_or(DATA)?, field)
        {
            return Err(DATA);
        }
        Ok((
            wide(data_at - block_at) + packed + wide(check.len()),
            unpacked,
        ))
    }

    /// A block's chunks, to the zero byte that ends them.
    fn chunks(&mut self, block: &Block) -> Result<(), Refused> {
        // A block begins with nothing to reach back to and no properties.
        let mut dictionary_at = None;
        let mut has_properties = false;
        loop {
            let control = self.input.byte()?;
            if control == 0 {
                return Ok(());
            }
            if control >= 0xE0 || control == 1 {
                dictionary_at = Some(self.total);
                has_properties = false;
            }
            let from = dictionary_at.ok_or(DATA)?;
            if control <= 2 {
                let size = self.input.size()?;
                let data = self.input.take(size)?;
                self.add(size)?;
                if let Some(out) = &mut self.out {
                    out.extend_from_slice(data);
                }
                continue;
            }
            if control < 0x80 {
                return Err(DATA);
            }
            let unpacked = (usize::from(control & 0x1F) << 16) + self.input.size()?;
            let packed = self.input.size()?;
            let properties = (control >= 0xC0).then(|| self.input.byte()).transpose()?;
            has_properties |= properties.is_some();
            if !has_properties {
                return Err(DATA);
            }
            let payload = self.input.take(packed)?;
            self.add(unpacked)?;
            if let Some(out) = &mut self.out {
                let lzma = self.lzma.get_or_insert_with(Lzma::new);
                if let Some(properties) = properties {
                    lzma.set(properties)?;
                }
                if control >= 0xA0 {
                    lzma.reset();
                }
                lzma.chunk(payload, unpacked, out, from, block.dictionary)?;
            }
        }
    }

    /// Counts what a chunk says it unpacks to, and refuses the file when
    /// that takes it past the limit.
    fn add(&mut self, unpacked: usize) -> Result<(), Refused> {
        self.total = self
            .total
            .checked_add(unpacked)
            .filter(|total| *total <= self.limit)
            .ok_or(Refused::TooLarge)?;
        Ok(())
    }

    /// The index, which states each block's sizes again. Its first byte,
    /// at `index_at`, is read.
    fn index(&mut self, index_at: usize, blocks: u64, sizes: u32) -> Result<(), Refused> {
        if self.input.integer()? != blocks {
            return Err(HEADER);
        }
        let mut said = SIZES;
        for _ in 0..blocks {
            let unpadded = self.input.integer()?;
            said = pair(said, unpadded, self.input.integer()?);
        }
        self.input.padding(index_at)?;
        let all = self
            .input
            .bytes
            .get(index_at..self.input.at)
            .ok_or(CUT_SHORT)?;
        if self.input.u32()? != crc32(all) || said != sizes {
            return Err(HEADER);
        }
        Ok(())
    }
}

/// A count of bytes as the format states one.
fn wide(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// The range decoder: the bits of one chunk's bytes.
struct Range<'a> {
    bytes: &'a [u8],
    at: usize,
    width: u32,
    code: u32,
}

impl<'a> Range<'a> {
    /// A chunk's bytes begin with a zero and the first four of the code.
    fn new(bytes: &'a [u8]) -> Result<Self, Refused> {
        let [0, a, b, c, d, ..] = *bytes else {
            return Err(DATA);
        };
        Ok(Self {
            bytes,
            at: 5,
            width: u32::MAX,
            code: u32::from_be_bytes([a, b, c, d]),
        })
    }

    fn normalize(&mut self) -> Result<(), Refused> {
        if self.width < 1 << 24 {
            let byte = *self.bytes.get(self.at).ok_or(DATA)?;
            self.at += 1;
            self.width <<= 8;
            self.code = (self.code << 8) | u32::from(byte);
        }
        Ok(())
    }

    /// One bit, and the chance of it learns from what it was.
    fn bit(&mut self, chance: &mut u16) -> Result<bool, Refused> {
        self.normalize()?;
        let bound = (self.width >> 11) * u32::from(*chance);
        if self.code < bound {
            self.width = bound;
            *chance += (2048 - *chance) >> 5;
            Ok(false)
        } else {
            self.width -= bound;
            self.code -= bound;
            *chance -= *chance >> 5;
            Ok(true)
        }
    }

    /// Bits with even chances, first bit highest.
    fn direct(&mut self, bits: u32) -> Result<u32, Refused> {
        let mut value = 0_u32;
        for _ in 0..bits {
            self.normalize()?;
            self.width >>= 1;
            self.code = self.code.wrapping_sub(self.width);
            let borrowed = 0_u32.wrapping_sub(self.code >> 31);
            self.code = self.code.wrapping_add(self.width & borrowed);
            value = (value << 1) | (borrowed.wrapping_add(1));
        }
        Ok(value)
    }

    /// A number of `bits` bits from a tree of chances, first bit highest.
    fn tree(&mut self, chances: &mut [u16], bits: u32) -> Result<u32, Refused> {
        let mut node = 1_usize;
        for _ in 0..bits {
            let bit = self.bit(chances.get_mut(node).ok_or(DATA)?)?;
            node = (node << 1) | usize::from(bit);
        }
        u32::try_from(node - (1 << bits)).map_err(|_| DATA)
    }

    /// The same, first bit lowest.
    fn reversed(&mut self, chances: &mut [u16], bits: u32) -> Result<u32, Refused> {
        let mut node = 1_usize;
        let mut value = 0;
        for place in 0..bits {
            let bit = self.bit(chances.get_mut(node).ok_or(DATA)?)?;
            node = (node << 1) | usize::from(bit);
            value |= u32::from(bit) << place;
        }
        Ok(value)
    }

    /// A chunk ends with every byte read and nothing left in the code.
    fn ended(&mut self) -> Result<(), Refused> {
        self.normalize()?;
        if self.at == self.bytes.len() && self.code == 0 {
            Ok(())
        } else {
            Err(DATA)
        }
    }
}

/// An even chance.
const EVEN: u16 = 1024;

/// A match's length, less the two that is its least.
struct Length {
    choice: u16,
    choice2: u16,
    low: [[u16; 8]; 16],
    mid: [[u16; 8]; 16],
    high: [u16; 256],
}

impl Length {
    const NEW: Self = Self {
        choice: EVEN,
        choice2: EVEN,
        low: [[EVEN; 8]; 16],
        mid: [[EVEN; 8]; 16],
        high: [EVEN; 256],
    };

    fn read(&mut self, range: &mut Range, place: usize) -> Result<u32, Refused> {
        if !range.bit(&mut self.choice)? {
            return range.tree(self.low.get_mut(place).ok_or(DATA)?, 3);
        }
        if !range.bit(&mut self.choice2)? {
            return Ok(8 + range.tree(self.mid.get_mut(place).ok_or(DATA)?, 3)?);
        }
        Ok(16 + range.tree(&mut self.high, 8)?)
    }
}

/// The LZMA decoder's state, which lasts from chunk to chunk until a chunk
/// resets it.
struct Lzma {
    /// The bits of the last byte, and of the place, that choose a
    /// literal's chances, and the bits of the place that choose a match's.
    last_bits: u32,
    literal_places: usize,
    places: usize,
    /// What the last few steps were, 0 to 11.
    state: usize,
    /// The last four distances, each one less than it is.
    back: [u32; 4],
    is_match: [[u16; 16]; 12],
    is_repeat: [u16; 12],
    is_repeat0: [u16; 12],
    is_repeat1: [u16; 12],
    is_repeat2: [u16; 12],
    is_long: [[u16; 16]; 12],
    slot: [[u16; 64]; 4],
    special: [u16; 115],
    align: [u16; 16],
    matched: Length,
    repeated: Length,
    literal: Vec<u16>,
}

impl Lzma {
    /// Nothing learned, and no room yet for the literals' chances.
    const NEW: Self = Self {
        last_bits: 0,
        literal_places: 0,
        places: 0,
        state: 0,
        back: [0; 4],
        is_match: [[EVEN; 16]; 12],
        is_repeat: [EVEN; 12],
        is_repeat0: [EVEN; 12],
        is_repeat1: [EVEN; 12],
        is_repeat2: [EVEN; 12],
        is_long: [[EVEN; 16]; 12],
        slot: [[EVEN; 64]; 4],
        special: [EVEN; 115],
        align: [EVEN; 16],
        matched: Length::NEW,
        repeated: Length::NEW,
        literal: Vec::new(),
    };

    /// With room for the literals' chances: 0x300 for each of the sixteen
    /// kinds of place a literal can have.
    fn new() -> Self {
        Self {
            literal: vec![EVEN; 0x300 << 4],
            ..Self::NEW
        }
    }

    /// New properties: `(pb * 5 + lp) * 9 + lc`, with `lc + lp` at most 4.
    fn set(&mut self, properties: u8) -> Result<(), Refused> {
        let last_bits = u32::from(properties % 9);
        let literal_place_bits = u32::from(properties / 9 % 5);
        let place_bits = u32::from(properties / 45);
        if place_bits > 4 || last_bits + literal_place_bits > 4 {
            return Err(DATA);
        }
        self.last_bits = last_bits;
        self.literal_places = (1 << literal_place_bits) - 1;
        self.places = (1 << place_bits) - 1;
        Ok(())
    }

    /// Forgets what it learned, and keeps its properties.
    fn reset(&mut self) {
        self.literal.fill(EVEN);
        *self = Self {
            last_bits: self.last_bits,
            literal_places: self.literal_places,
            places: self.places,
            literal: std::mem::take(&mut self.literal),
            ..Self::NEW
        };
    }

    /// Unpacks one chunk: `unpacked` bytes onto `out` from all of
    /// `payload`. The dictionary is `out` from `from` on, and reaches back
    /// `dictionary` bytes at most.
    fn chunk(
        &mut self,
        payload: &[u8],
        unpacked: usize,
        out: &mut Vec<u8>,
        from: usize,
        dictionary: u32,
    ) -> Result<(), Refused> {
        let mut range = Range::new(payload)?;
        let end = out.len().checked_add(unpacked).ok_or(DATA)?;
        while out.len() < end {
            let place = out.len().checked_sub(from).ok_or(DATA)? & self.places;
            let state = self.state;
            if !range.bit(
                self.is_match
                    .get_mut(state)
                    .and_then(|row| row.get_mut(place))
                    .ok_or(DATA)?,
            )? {
                self.literal(&mut range, out, from)?;
                continue;
            }
            let length = if range.bit(self.is_repeat.get_mut(state).ok_or(DATA)?)? {
                let Some(length) = self.repeat(&mut range, place)? else {
                    copy(out, from, dictionary, self.back[0], 1, end)?;
                    continue;
                };
                length
            } else {
                self.back = [0, self.back[0], self.back[1], self.back[2]];
                let length = self.matched.read(&mut range, place)?;
                self.state = if state < 7 { 7 } else { 10 };
                self.back[0] = self.distance(&mut range, length)?;
                length
            };
            let length = usize::try_from(length).map_err(|_| DATA)? + 2;
            copy(out, from, dictionary, self.back[0], length, end)?;
        }
        range.ended()
    }

    /// One byte that is not copied from before.
    fn literal(
        &mut self,
        range: &mut Range,
        out: &mut Vec<u8>,
        from: usize,
    ) -> Result<(), Refused> {
        let written = out.len().checked_sub(from).ok_or(DATA)?;
        let last = if written == 0 {
            0
        } else {
            usize::from(*out.last().ok_or(DATA)?)
        };
        let which =
            ((written & self.literal_places) << self.last_bits) + (last >> (8 - self.last_bits));
        let chances = self
            .literal
            .get_mut(0x300 * which..0x300 * (which + 1))
            .ok_or(DATA)?;
        let mut symbol = 1_usize;
        if self.state >= 7 {
            // After a match, the byte that would have come next guides it.
            let behind = usize::try_from(self.back[0]).map_err(|_| DATA)?;
            let at = out
                .len()
                .checked_sub(behind)
                .and_then(|at| at.checked_sub(1))
                .filter(|at| *at >= from)
                .ok_or(DATA)?;
            let mut guide = usize::from(*out.get(at).ok_or(DATA)?);
            while symbol < 0x100 {
                let expected = (guide >> 7) & 1;
                guide <<= 1;
                let chance = chances
                    .get_mut(((1 + expected) << 8) + symbol)
                    .ok_or(DATA)?;
                let bit = usize::from(range.bit(chance)?);
                symbol = (symbol << 1) | bit;
                if expected != bit {
                    break;
                }
            }
        }
        while symbol < 0x100 {
            let bit = range.bit(chances.get_mut(symbol).ok_or(DATA)?)?;
            symbol = (symbol << 1) | usize::from(bit);
        }
        out.push(u8::try_from(symbol & 0xFF).map_err(|_| DATA)?);
        self.state = match self.state {
            0..=3 => 0,
            4..=9 => self.state - 3,
            _ => self.state - 6,
        };
        Ok(())
    }

    /// A copy from one of the last four distances. Gives its length less
    /// two, or none for the one byte at the last distance.
    fn repeat(&mut self, range: &mut Range, place: usize) -> Result<Option<u32>, Refused> {
        let state = self.state;
        if range.bit(self.is_repeat0.get_mut(state).ok_or(DATA)?)? {
            let which = if !range.bit(self.is_repeat1.get_mut(state).ok_or(DATA)?)? {
                1
            } else if !range.bit(self.is_repeat2.get_mut(state).ok_or(DATA)?)? {
                2
            } else {
                3
            };
            // The one chosen comes to the front, and the others keep
            // their order.
            self.back[..=which].rotate_right(1);
        } else if !range.bit(
            self.is_long
                .get_mut(state)
                .and_then(|row| row.get_mut(place))
                .ok_or(DATA)?,
        )? {
            self.state = if state < 7 { 9 } else { 11 };
            return Ok(None);
        }
        let length = self.repeated.read(range, place)?;
        self.state = if state < 7 { 8 } else { 11 };
        Ok(Some(length))
    }

    /// A new match's distance, less one, for its `length` less two.
    fn distance(&mut self, range: &mut Range, length: u32) -> Result<u32, Refused> {
        let lengths = usize::try_from(length.min(3)).map_err(|_| DATA)?;
        let slot = range.tree(self.slot.get_mut(lengths).ok_or(DATA)?, 6)?;
        if slot < 4 {
            return Ok(slot);
        }
        let bits = (slot >> 1) - 1;
        let base = (2 | (slot & 1)) << bits;
        if slot < 14 {
            let at = usize::try_from(base - slot).map_err(|_| DATA)?;
            return Ok(base + range.reversed(self.special.get_mut(at..).ok_or(DATA)?, bits)?);
        }
        let high = range.direct(bits - 4)? << 4;
        Ok(base + high + range.reversed(&mut self.align, 4)?)
    }
}

/// Copies `length` bytes from `behind + 1` bytes back, which may overlap
/// what it writes. It may reach neither before the dictionary began nor
/// further than the dictionary is long, nor write past the chunk's `end`.
fn copy(
    out: &mut Vec<u8>,
    from: usize,
    dictionary: u32,
    behind: u32,
    length: usize,
    end: usize,
) -> Result<(), Refused> {
    let back = usize::try_from(behind).map_err(|_| DATA)?;
    let written = out.len().checked_sub(from).ok_or(DATA)?;
    if back >= written || behind >= dictionary || length > end.saturating_sub(out.len()) {
        return Err(DATA);
    }
    let start = out.len() - back - 1;
    if back + 1 >= length {
        out.extend_from_within(start..start + length);
    } else {
        for at in start..start + length {
            out.push(*out.get(at).ok_or(DATA)?);
        }
    }
    Ok(())
}

/// Where a list of sizes begins, for [`pair`].
const SIZES: u32 = u32::MAX;

/// Adds a block's two sizes to a running check of a list of them, so the
/// index's list is compared with the blocks' without keeping either.
fn pair(sizes: u32, unpadded: u64, unpacked: u64) -> u32 {
    crc32_more(
        crc32_more(sizes, &unpadded.to_le_bytes()),
        &unpacked.to_le_bytes(),
    )
}

const fn crc32_table() -> [u32; 256] {
    let mut table = [0; 256];
    let mut at = 0;
    let mut value = 0_u32;
    while at < 256 {
        let mut crc = value;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[at] = crc;
        at += 1;
        value += 1;
    }
    table
}

const fn crc64_table() -> [u64; 256] {
    let mut table = [0; 256];
    let mut at = 0;
    let mut value = 0_u64;
    while at < 256 {
        let mut crc = value;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xC96C_5795_D787_0F42
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[at] = crc;
        at += 1;
        value += 1;
    }
    table
}

static CRC32: [u32; 256] = crc32_table();
static CRC64: [u64; 256] = crc64_table();

fn crc32_more(mut crc: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        let [low, ..] = crc.to_le_bytes();
        crc = CRC32[usize::from(low ^ byte)] ^ (crc >> 8);
    }
    crc
}

fn crc32(bytes: &[u8]) -> u32 {
    !crc32_more(u32::MAX, bytes)
}

fn crc64(bytes: &[u8]) -> u64 {
    let mut crc = u64::MAX;
    for byte in bytes {
        let [low, ..] = crc.to_le_bytes();
        crc = CRC64[usize::from(low ^ byte)] ^ (crc >> 8);
    }
    !crc
}

/// `xz -6` of 64 MiB of zeros, in under 10 KB, and the same with the bytes
/// of its first chunk damaged and every header as it was. Both are refused
/// for their size wherever nothing is unpacked before it is measured.
#[cfg(test)]
pub(crate) fn bomb() -> (&'static [u8], Vec<u8>) {
    let zeros = include_bytes!("../fixtures/xz/zeros-64mib.xz");
    // The stream's header is 12 bytes, the block's 12, the chunk's 6.
    let packed = usize::from(u16::from_be_bytes([zeros[27], zeros[28]])) + 1;
    let mut damaged = zeros.to_vec();
    for byte in &mut damaged[30..30 + packed] {
        *byte ^= 0xA5;
    }
    (zeros, damaged)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A camera definition file as it was when these were made, 4476 bytes.
    const PLAIN: &[u8] = include_bytes!("../fixtures/xz/plain.xml");
    /// `xz -9e` of it.
    const PACKED: &[u8] = include_bytes!("../fixtures/xz/plain.xz");
    /// `xz -6` of it 700 times over, 3 MiB: two chunks, the second of
    /// which goes on from the first.
    const REPEATED: &[u8] = include_bytes!("../fixtures/xz/plain-700.xz");
    /// `xz -T2 --block-size=1024 --check=crc32 --lzma2=preset=6,lc=1,lp=3,pb=1`
    /// of it: five blocks whose headers state their sizes.
    const BLOCKS: &[u8] = include_bytes!("../fixtures/xz/plain-blocks-crc32.xz");
    /// `xz --check=sha256` and `xz --x86 --lzma2=preset=6` of it.
    const SHA256: &[u8] = include_bytes!("../fixtures/xz/plain-sha256.xz");
    const X86: &[u8] = include_bytes!("../fixtures/xz/plain-x86.xz");
    const MIB: usize = 1 << 20;

    fn integer(mut value: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        while value >= 0x80 {
            bytes.push(0x80 | u8::try_from(value & 0x7F).unwrap());
            value >>= 7;
        }
        bytes.push(u8::try_from(value).unwrap());
        bytes
    }

    /// A stream of one block around these LZMA2 chunks, with no check,
    /// whose index says the block unpacks to `unpacked` bytes.
    fn stream(chunks: &[u8], unpacked: u64) -> Vec<u8> {
        let mut file = MAGIC.to_vec();
        file.extend([0, 0]);
        file.extend(crc32(&[0, 0]).to_le_bytes());
        let mut header = vec![2, 0, 0x21, 1, 22, 0, 0, 0];
        header.extend(crc32(&header).to_le_bytes());
        file.extend(&header);
        file.extend(chunks);
        file.resize(file.len().next_multiple_of(4), 0);
        let index_at = file.len();
        file.extend([0, 1]);
        file.extend(integer(wide(header.len() + chunks.len())));
        file.extend(integer(unpacked));
        file.resize(file.len().next_multiple_of(4), 0);
        file.extend(crc32(&file[index_at..]).to_le_bytes());
        let mut rest = u32::try_from((file.len() - index_at) / 4 - 1)
            .unwrap()
            .to_le_bytes()
            .to_vec();
        rest.extend([0, 0]);
        file.extend(crc32(&rest).to_le_bytes());
        file.extend(rest);
        file.extend(b"YZ");
        file
    }

    /// The one chunk of [`PACKED`]: its six bytes of header and its bytes.
    fn the_chunk() -> (&'static [u8], &'static [u8]) {
        let (header, rest) = PACKED[24..].split_at(6);
        let packed = usize::from(u16::from_be_bytes([header[3], header[4]])) + 1;
        assert_eq!(rest[packed], 0, "one chunk, then the end");
        (header, &rest[..packed])
    }

    #[test]
    fn what_xz_writes_is_unpacked() {
        assert!(is_xz(PACKED) && !is_xz(PLAIN));
        assert_eq!(unpack(PACKED, MIB).unwrap(), PLAIN);
        assert_eq!(unpack(BLOCKS, MIB).unwrap(), PLAIN);
        assert_eq!(unpack(REPEATED, 4 * MIB).unwrap(), PLAIN.repeat(700));
        let empty = stream(&[0], 0);
        assert_eq!(unpack(&empty, 0).unwrap(), b"");
    }

    #[test]
    fn chunks_that_are_not_packed_are_copied() {
        // What the tests' own packer writes, `lzma-rs`: 64 KiB to a chunk.
        let text = PLAIN.repeat(40);
        let mut packed = Vec::new();
        lzma_rs::xz_compress(&mut text.as_slice(), &mut packed).unwrap();
        assert_eq!(unpack(&packed, MIB).unwrap(), text);

        let two = [&[1, 0, 4][..], b"hello", &[2, 0, 2], b" me", &[0]].concat();
        assert_eq!(unpack(&stream(&two, 8), 8).unwrap(), b"hello me");
        // A block's first chunk begins its dictionary, and a packed chunk
        // needs its properties first.
        let no_start = [&[2, 0, 4][..], b"hello", &[0]].concat();
        assert_eq!(unpack(&stream(&no_start, 5), 8), Err(DATA));
        let no_properties = [&[1, 0, 4][..], b"hello", &[0x80, 0, 0, 0, 5], &[0; 6], &[0]].concat();
        assert_eq!(unpack(&stream(&no_properties, 6), 8), Err(DATA));
        let no_such_chunk = [&[1, 0, 4][..], b"hello", &[0x7F, 0, 0], &[0]].concat();
        assert_eq!(unpack(&stream(&no_such_chunk, 6), 8), Err(DATA));
    }

    #[test]
    fn a_file_is_refused_for_its_size_before_any_of_it_is_unpacked() {
        let (zeros, damaged) = bomb();
        assert_eq!(unpack(zeros, 4 * MIB), Err(Refused::TooLarge));
        assert_eq!(unpack(zeros, 64 * MIB - 1), Err(Refused::TooLarge));

        // Honest headers over a body that is not: the answer is still its
        // size, because the body was not read.
        assert_eq!(unpack(&damaged, 4 * MIB), Err(Refused::TooLarge));
        // With room for all of it the body is read, and the damage found.
        assert_eq!(unpack(&damaged, 64 * MIB), Err(DATA));
    }

    #[test]
    fn the_limit_is_to_the_byte() {
        assert_eq!(unpack(PACKED, PLAIN.len()).unwrap(), PLAIN);
        assert_eq!(unpack(PACKED, PLAIN.len() - 1), Err(Refused::TooLarge));
        assert_eq!(unpack(BLOCKS, PLAIN.len() - 1), Err(Refused::TooLarge));
        assert_eq!(unpack(PACKED, 0), Err(Refused::TooLarge));
    }

    #[test]
    fn a_chunk_is_held_to_the_bytes_it_states() {
        let (header, bytes) = the_chunk();
        let size = wide(PLAIN.len());
        let honest = [header, bytes, &[0]].concat();
        assert_eq!(unpack(&stream(&honest, size), MIB).unwrap(), PLAIN);

        // The chunk says it has ten bytes more than it does. They are a
        // second chunk that no header counted.
        let more = [&[2, 0, 9][..], b"0123456789"].concat();
        let stated = u16::try_from(bytes.len() + more.len() - 1)
            .unwrap()
            .to_be_bytes();
        let hidden = [&header[..3], &stated, &header[5..], bytes, &more, &[0]].concat();
        assert_eq!(unpack(&stream(&hidden, size), MIB), Err(DATA));
        assert_eq!(unpack(&stream(&hidden, size + 10), MIB), Err(HEADER));

        // It says it has a byte fewer, or unpacks to a byte more or fewer.
        let stated = u16::try_from(bytes.len() - 2).unwrap().to_be_bytes();
        let short = [
            &header[..3],
            &stated,
            &header[5..],
            &bytes[..bytes.len() - 1],
            &[0],
        ]
        .concat();
        assert_eq!(unpack(&stream(&short, size), MIB), Err(DATA));
        for other in [PLAIN.len() - 2, PLAIN.len()] {
            let stated = u16::try_from(other).unwrap().to_be_bytes();
            let wrong = [&header[..1], &stated, &header[3..], bytes, &[0]].concat();
            assert_eq!(unpack(&stream(&wrong, wide(other + 1)), MIB), Err(DATA));
        }
    }

    #[test]
    fn what_this_does_not_read_is_refused() {
        assert_eq!(unpack(SHA256, MIB), Err(CHECK));
        assert_eq!(unpack(X86, MIB), Err(FILTER));
        assert_eq!(unpack(&[PACKED, PACKED].concat(), MIB), Err(MORE));
        assert_eq!(unpack(&[PACKED, &[0; 4]].concat(), MIB), Err(MORE));
        assert_eq!(
            unpack(PLAIN, MIB),
            Err(Refused::Unreadable("is not an xz file"))
        );
        assert_eq!(
            unpack(b"", MIB),
            Err(Refused::Unreadable("is not an xz file"))
        );
    }

    #[test]
    fn a_file_cut_short_or_damaged_anywhere_is_refused() {
        // One changed byte is refused wherever it is, with one exception
        // that `xz` itself makes: the lowest bit of the properties of each
        // of the five blocks, which sets how many bits of the last byte
        // choose a literal's chances. Text of seven-bit bytes unpacks the
        // same with one such bit or none.
        for (file, harmless) in [(PACKED, 0), (BLOCKS, 5)] {
            for end in 0..file.len() {
                assert!(unpack(&file[..end], MIB).is_err(), "cut at {end}");
            }
            let mut unpacked = 0;
            for at in 0..file.len() {
                for change in [0x01, 0x80, 0xFF] {
                    let mut damaged = file.to_vec();
                    damaged[at] ^= change;
                    if let Ok(bytes) = unpack(&damaged, MIB) {
                        assert_eq!(bytes, PLAIN, "byte {at} changed by {change}");
                        unpacked += 1;
                    }
                }
            }
            assert_eq!(unpacked, harmless);
        }
    }

    #[test]
    fn damage_never_unpacks_past_the_limit() {
        let mut seed = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = move |below: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            usize::try_from(seed % wide(below)).unwrap()
        };
        // A check that holds whatever is unpacked, so that damage is read
        // as far as it can be: the chunk's own header may then say
        // anything.
        let (header, bytes) = the_chunk();
        let honest = [header, bytes, &[0]].concat();
        for _ in 0..4000 {
            let mut chunks = honest.clone();
            for _ in 0..=next(3) {
                let at = next(chunks.len());
                chunks[at] ^= u8::try_from(next(255) + 1).unwrap();
            }
            for unpacked in [PLAIN.len(), PLAIN.len() + next(4096)] {
                let limit = 2 * PLAIN.len();
                if let Ok(out) = unpack(&stream(&chunks, wide(unpacked)), limit) {
                    assert!(out.len() <= limit);
                }
            }
        }
    }
}
