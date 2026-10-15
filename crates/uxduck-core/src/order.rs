//! Fractional order keys: strings that sort in the order the user placed
//! things, so moving one item writes one key and never renumbers its
//! neighbours. After the `fractional-indexing` algorithm, without its
//! integer part.

const DIGITS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

fn value(digit: u8) -> usize {
    DIGITS.iter().position(|d| *d == digit).unwrap_or(0)
}

/// A key strictly between `before` and `after`, either of which may be
/// missing (the start or end of the list). Keys never end in `0`, which is
/// what keeps a gap between any two of them.
///
/// # Panics
/// Never for keys this function made; with `before >= after` the result is
/// not between them.
#[must_use]
pub fn key_between(before: Option<&str>, after: Option<&str>) -> String {
    match (before, after) {
        // Appending is the common case; step the first digit rather than
        // halving, so keys grow by one character per 61 appends.
        (Some(before), None) => key_after(before.as_bytes()),
        _ => midpoint(before.unwrap_or("").as_bytes(), after.map(str::as_bytes)),
    }
}

fn key_after(a: &[u8]) -> String {
    let Some(first) = a.first() else {
        return char::from(DIGITS[DIGITS.len() / 2]).to_string();
    };
    let digit = value(*first);
    if digit + 1 < DIGITS.len() {
        char::from(DIGITS[digit + 1]).to_string()
    } else {
        char::from(*first).to_string() + &key_after(&a[1..])
    }
}

fn midpoint(a: &[u8], b: Option<&[u8]>) -> String {
    if let Some(b) = b {
        let mut n = 0;
        while n < b.len() && a.get(n).copied().unwrap_or(b'0') == b[n] {
            n += 1;
        }
        if n > 0 {
            let prefix = String::from_utf8_lossy(&b[..n]).into_owned();
            let rest = if n <= a.len() { &a[n..] } else { &[][..] };
            return prefix + &midpoint(rest, Some(&b[n..]));
        }
    }
    let low = a.first().map_or(0, |d| value(*d));
    let high = b
        .and_then(|b| b.first())
        .map_or(DIGITS.len(), |d| value(*d));
    if high - low > 1 {
        return char::from(DIGITS[usize::midpoint(low, high)]).to_string();
    }
    match b {
        Some(b) if b.len() > 1 => char::from(b[0]).to_string(),
        _ => {
            let head = char::from(DIGITS[low]).to_string();
            head + &midpoint(a.get(1..).unwrap_or(&[]), None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_fall_between_their_neighbours() {
        let first = key_between(None, None);
        let after = key_between(Some(&first), None);
        let before = key_between(None, Some(&first));
        let middle = key_between(Some(&first), Some(&after));
        assert!(
            before < first && first < middle && middle < after,
            "{before} {first} {middle} {after}"
        );
        for key in [&first, &after, &before, &middle] {
            assert!(!key.ends_with('0'), "{key}");
        }
    }

    #[test]
    fn many_inserts_anywhere_stay_ordered_and_distinct() {
        // A deterministic scatter of positions, including the ends.
        let mut keys: Vec<String> = Vec::new();
        let mut seed = 7u64;
        for _ in 0..2000 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let at = usize::try_from(seed >> 33).unwrap_or(0) % (keys.len() + 1);
            let before = at.checked_sub(1).map(|i| keys[i].as_str());
            let after = keys.get(at).map(String::as_str);
            let key = key_between(before, after);
            if let Some(b) = before {
                assert!(b < key.as_str(), "{b} !< {key}");
            }
            if let Some(a) = after {
                assert!(key.as_str() < a, "{key} !< {a}");
            }
            keys.insert(at, key);
        }
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted, keys);
        assert!(keys.iter().map(String::len).max().unwrap_or(0) < 40);
    }

    #[test]
    fn appending_again_and_again_grows_slowly() {
        let mut last = key_between(None, None);
        for _ in 0..500 {
            last = key_between(Some(&last), None);
        }
        assert!(last.len() < 20, "{last}");
    }
}
