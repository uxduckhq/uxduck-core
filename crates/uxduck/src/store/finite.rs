//! Floats JSON cannot carry. `serde_json` writes a NaN or an infinity as
//! `null`, which no `f64` field reads back: one in the event log would end
//! the log there (it reads as a torn last line), and one in the checkpoint
//! would stop the daemon starting. A PX4 parameter once did (S17).
//!
//! So what is written is checked to read back, and a non-finite float is
//! written as 0 instead, with an error saying so; and what is read gets the
//! same repair, for files written before this guard. The number is lost
//! either way; the rest of the record is kept.

use std::borrow::Cow;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Repairs given up after this many in one record: something else is wrong.
const MAX_REPAIRS: usize = 1_000;

/// Reads `text` as `T`, turning each `null` that a float field refuses into
/// 0. Returns the value, the text it was read from, and how many floats
/// were repaired.
///
/// # Errors
/// When the text does not read as `T` for any other reason.
pub fn read<T: DeserializeOwned>(
    text: &str,
) -> Result<(T, Cow<'_, str>, usize), serde_json::Error> {
    // Repaired `null`s by their offset in `text`. While searching they are
    // padded to their own length, so every offset stays where it was.
    let mut repaired: Vec<usize> = Vec::new();
    let mut padded = Cow::Borrowed(text);
    loop {
        let error = match serde_json::from_str::<T>(&padded) {
            Ok(value) if repaired.is_empty() => return Ok((value, Cow::Borrowed(text), 0)),
            Ok(value) => {
                let compact = with_zeros(text, &repaired, false);
                return Ok((value, Cow::Owned(compact), repaired.len()));
            }
            Err(error) => error,
        };
        let Outcome::Refused(at) = outcome(&padded, &error) else {
            return Err(error);
        };
        let fixed = (repaired.len() < MAX_REPAIRS)
            .then(|| repair::<T>(&padded, at))
            .flatten();
        let Some(nulls) = fixed else {
            return Err(error);
        };
        repaired.extend(nulls);
        padded = Cow::Owned(with_zeros(text, &repaired, true));
    }
}

/// Writes `value` as JSON that reads back as `T`: a non-finite float is
/// written as 0, and the error says what held it.
///
/// # Errors
/// When `value` does not serialize.
pub fn write<T: Serialize + DeserializeOwned>(
    value: &T,
    what: &str,
) -> Result<String, serde_json::Error> {
    let text = serde_json::to_string(value)?;
    // Only a `null` can be a float JSON could not carry.
    if !text.contains("null") {
        return Ok(text);
    }
    match read::<T>(&text) {
        Ok((_, _, 0)) => Ok(text),
        Ok((_, repaired, repairs)) => {
            tracing::error!(
                what,
                repairs,
                code = "store-non-finite",
                "a NaN or infinite number was written as 0, since JSON cannot carry it"
            );
            Ok(repaired.into_owned())
        }
        // It does not read back for another reason; write it as it is and
        // let the reader say so.
        Err(error) => {
            tracing::error!(what, %error, code = "store-unreadable", "written JSON does not read back");
            Ok(text)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Read,
    /// A float field refused a `null`, reported at this byte offset: just
    /// after the `null`, or after the object or list holding it when serde
    /// read that ahead (an internally tagged enum).
    Refused(usize),
    Other,
}

fn outcome(text: &str, error: &serde_json::Error) -> Outcome {
    let message = error.to_string();
    let refused = message.starts_with("invalid type: null, expected f64")
        || message.starts_with("invalid type: null, expected f32");
    if !refused || error.line() == 0 {
        return Outcome::Other;
    }
    let line_start: usize = text
        .split_inclusive('\n')
        .take(error.line() - 1)
        .map(str::len)
        .sum();
    Outcome::Refused(line_start + error.column())
}

fn try_read<T: DeserializeOwned>(text: &str) -> Outcome {
    match serde_json::from_str::<T>(text) {
        Ok(_) => Outcome::Read,
        Err(error) => outcome(text, &error),
    }
}

/// The `null`s to write as 0 to get past the refusal reported at `at`.
fn repair<T: DeserializeOwned>(text: &str, at: usize) -> Option<Vec<usize>> {
    let start = at.checked_sub("null".len())?;
    if text.get(start..at) == Some("null") {
        return Some(vec![start]);
    }
    // Reported after the value serde read ahead: one of its `null`s. Try
    // them in turn, keeping one when the refusal stays or moves on, then
    // put back each that was not needed, so an absent optional value stays
    // absent.
    let scan = Scan::of(text);
    let close = at.checked_sub(1)?;
    let open = scan.open_of(close)?;
    let candidates: Vec<usize> = scan
        .nulls
        .iter()
        .copied()
        .filter(|n| (open..close).contains(n))
        .collect();
    let stuck = Outcome::Refused(at);
    let mut kept: Vec<usize> = Vec::new();
    for candidate in candidates {
        let mut trial = kept.clone();
        trial.push(candidate);
        match try_read::<T>(&with_zeros(text, &trial, true)) {
            Outcome::Other => {}
            result => {
                kept = trial;
                if result != stuck {
                    break;
                }
            }
        }
    }
    let solved = try_read::<T>(&with_zeros(text, &kept, true));
    if solved == stuck || solved == Outcome::Other {
        return None;
    }
    for null in kept.clone() {
        let without: Vec<usize> = kept.iter().copied().filter(|n| *n != null).collect();
        if try_read::<T>(&with_zeros(text, &without, true)) == solved {
            kept = without;
        }
    }
    Some(kept)
}

/// `text` with a `0` for the `null` at each offset; `padded`, with spaces
/// after it, so nothing after it moves.
fn with_zeros(text: &str, nulls: &[usize], padded: bool) -> String {
    let mut sorted = nulls.to_vec();
    sorted.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut from = 0;
    for at in sorted {
        out.push_str(&text[from..at]);
        out.push_str(if padded { "0   " } else { "0" });
        from = at + "null".len();
    }
    out.push_str(&text[from..]);
    out
}

/// Where each `null` and each bracket pair is, outside strings.
struct Scan {
    nulls: Vec<usize>,
    /// (close, open) offsets of each `}` and `]`.
    pairs: Vec<(usize, usize)>,
}

impl Scan {
    fn of(text: &str) -> Self {
        let bytes = text.as_bytes();
        let mut scan = Self {
            nulls: Vec::new(),
            pairs: Vec::new(),
        };
        let mut open = Vec::new();
        let (mut in_string, mut escaped) = (false, false);
        for (i, byte) in bytes.iter().enumerate() {
            if in_string {
                match (escaped, byte) {
                    (true, _) => escaped = false,
                    (false, b'\\') => escaped = true,
                    (false, b'"') => in_string = false,
                    _ => {}
                }
                continue;
            }
            match byte {
                b'"' => in_string = true,
                b'{' | b'[' => open.push(i),
                b'}' | b']' => {
                    if let Some(start) = open.pop() {
                        scan.pairs.push((i, start));
                    }
                }
                b'n' if bytes.get(i..i + 4) == Some(b"null") => scan.nulls.push(i),
                _ => {}
            }
        }
        scan
    }

    fn open_of(&self, close: usize) -> Option<usize> {
        self.pairs
            .iter()
            .find(|(c, _)| *c == close)
            .map(|(_, open)| *open)
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Parameter {
        name: String,
        value: f64,
        #[serde(default)]
        range: Vec<f32>,
        note: Option<String>,
        #[serde(default)]
        limit: Option<f64>,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "camelCase")]
    enum Change {
        ParameterChanged {
            before: Option<f64>,
            parameter: Parameter,
        },
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Event {
        seq: u64,
        change: Change,
    }

    fn parameter(value: f64) -> Parameter {
        Parameter {
            name: "CAL_ACC0_PRIO".into(),
            value,
            range: vec![1.0, f32::INFINITY],
            note: None,
            limit: None,
        }
    }

    #[test]
    fn a_nan_is_written_as_0_and_reads_back() {
        let text = write(&parameter(f64::NAN), "a parameter").unwrap();
        assert_eq!(
            text,
            r#"{"name":"CAL_ACC0_PRIO","value":0,"range":[1.0,0],"note":null,"limit":null}"#
        );
        let back: Parameter = serde_json::from_str(&text).unwrap();
        assert!(back.value.abs() < f64::EPSILON);
    }

    /// Inside an internally tagged enum, serde reports the end of the
    /// enum's object, not the `null`; absent optional values stay absent.
    #[test]
    fn a_nan_inside_a_tagged_enum_is_found_and_the_real_nulls_kept() {
        let event = Event {
            seq: 2,
            change: Change::ParameterChanged {
                before: None,
                parameter: parameter(f64::NAN),
            },
        };
        let text = write(&event, "event 2").unwrap();
        let back: Event = serde_json::from_str(&text).unwrap();
        let Change::ParameterChanged { before, parameter } = back.change;
        assert_eq!(before, None);
        assert_eq!(parameter.note, None);
        assert_eq!(parameter.limit, None);
        assert!(parameter.value.abs() < f64::EPSILON);
        assert_eq!(parameter.range, [1.0, 0.0]);
    }

    #[test]
    fn a_record_with_only_real_nulls_is_written_as_it_is() {
        let fine = parameter(1.5);
        let fine = Parameter {
            name: "a null in a name".into(),
            range: Vec::new(),
            ..fine
        };
        assert_eq!(
            write(&fine, "a parameter").unwrap(),
            serde_json::to_string(&fine).unwrap()
        );
    }

    #[test]
    fn an_old_record_with_nulls_for_floats_is_read_with_them_as_0() {
        // As a log line from before this guard, with text before the nulls
        // that is not ASCII, and pretty-printed.
        let line = r#"{"name":"é—ü null","value":null,"range":[2,null],"note":null}"#;
        let (value, _, repairs) = read::<Parameter>(line).unwrap();
        assert_eq!(repairs, 2);
        assert_eq!(value.name, "é—ü null");
        assert_eq!(value.range, [2.0, 0.0]);
        let pretty = "{\n  \"name\": \"x\",\n  \"value\": null\n}";
        assert_eq!(read::<Parameter>(pretty).unwrap().2, 1);
    }

    #[test]
    fn anything_else_wrong_is_still_an_error() {
        assert!(read::<Parameter>(r#"{"name":null,"value":1}"#).is_err());
        assert!(read::<Parameter>(r#"{"name":"x","value":1"#).is_err());
        assert!(read::<Parameter>(r#"{"name":"x","value":"1"}"#).is_err());
        assert!(read::<Event>(r#"{"seq":1,"change":{"type":"parameterChanged","parameter":{"name":null,"value":null}}}"#).is_err());
    }
}
