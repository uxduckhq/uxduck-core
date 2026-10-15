//! Just enough semantic versioning to compare `major.minor.patch`.

/// Parses `1.2.3`, ignoring a pre-release or build suffix.
pub fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(str::parse::<u64>);
    let version = (
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    );
    parts.next().is_none().then_some(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_orders() {
        assert_eq!(parse("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse("1.2.3-beta.1+sha"), Some((1, 2, 3)));
        assert_eq!(parse("1.2"), None);
        assert_eq!(parse("1.2.3.4"), None);
        assert_eq!(parse("x.1.0"), None);
        assert!(parse("0.2.0") > parse("0.1.9"));
    }
}
