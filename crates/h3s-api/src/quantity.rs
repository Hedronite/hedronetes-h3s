//! Kubernetes apimachinery-shaped resource quantities.
use std::fmt;

/// A validated resource quantity string (`resource.Quantity` wire form).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quantity {
    raw: String,
}

impl Quantity {
    /// Parse and validate apimachinery quantity syntax.
    pub fn parse(s: &str) -> Option<Self> {
        if !syntax_valid(s) {
            return None;
        }
        Some(Self { raw: s.into() })
    }

    /// CPU semantics: bare numbers are cores; `m`/`u`/`n` scale to millicores.
    pub fn as_milli_cpu(&self) -> Option<i64> {
        scaled(&self.raw, true)
    }

    /// Memory and storage semantics: bare numbers are bytes.
    pub fn as_bytes(&self) -> Option<i64> {
        scaled(&self.raw, false)
    }
}

impl fmt::Display for Quantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

fn syntax_valid(s: &str) -> bool {
    if s.is_empty() || s.starts_with('-') {
        return false;
    }
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (n, suffix) = s.split_at(split);
    // The numeric part is a whole number, optionally with a fraction; the
    // parser never guesses at an empty integer part or a trailing point.
    let (whole, frac) = n.split_once('.').unwrap_or((n, ""));
    let fraction = n.contains('.');
    if whole.is_empty()
        || (fraction && frac.is_empty())
        || frac.len() > 9
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return false;
    }
    matches!(
        suffix,
        "" | "m" | "u" | "n" | "Ki" | "Mi" | "Gi" | "Ti" | "k" | "K" | "M" | "G"
    )
}

/// Exact fixed-point conversion; round fractional units up like Kubernetes.
fn scaled(s: &str, cpu: bool) -> Option<i64> {
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (n, suffix) = s.split_at(split);
    let (whole, frac) = n.split_once('.').unwrap_or((n, ""));
    let scale = 10i128.pow(frac.len() as u32);
    let base = whole
        .parse::<i128>()
        .ok()
        .and_then(|x| x.checked_mul(scale))
        .and_then(|x| {
            x.checked_add(if frac.is_empty() {
                0
            } else {
                frac.parse::<i128>().ok()?
            })
        })?;
    let (mul, div) = match suffix {
        "" => (1, 1),
        "m" => (1, 1000),
        "u" => (1, 1_000_000),
        "n" => (1, 1_000_000_000),
        "Ki" => (1024, 1),
        "Mi" => (1024 * 1024, 1),
        "Gi" => (1024 * 1024 * 1024, 1),
        "Ti" => (1024i128.pow(4), 1),
        "k" | "K" => (1000, 1),
        "M" => (1_000_000, 1),
        "G" => (1_000_000_000, 1),
        _ => return None,
    };
    if cpu && matches!(suffix, "Ki" | "Mi" | "Gi" | "Ti" | "k" | "K" | "M" | "G") {
        return None;
    }
    if !cpu && matches!(suffix, "u" | "n") {
        return None;
    }
    let numerator = base
        .checked_mul(mul)
        .and_then(|x| x.checked_mul(if cpu { 1000 } else { 1 }))?;
    let denominator = scale * div;
    i64::try_from(numerator / denominator + i128::from(numerator % denominator != 0)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_invalid_syntax() {
        for s in ["", "-1", "not-a-number", "1.2.3", "1Wi"] {
            assert!(Quantity::parse(s).is_none(), "{s}");
        }
        assert!(Quantity::parse("100m").is_some());
        assert!(Quantity::parse("64Mi").is_some());
    }

    #[test]
    fn cpu_millicores_round_up_without_float_drift() {
        for (s, expected) in [
            ("100m", 100),
            ("0.1", 100),
            ("0.0001", 1),
            ("1", 1000),
            ("2500m", 2500),
        ] {
            let q = Quantity::parse(s).unwrap();
            assert_eq!(q.as_milli_cpu(), Some(expected), "{s}");
        }
        assert!(Quantity::parse("64Mi").unwrap().as_milli_cpu().is_none());
    }

    #[test]
    fn memory_bytes_round_up_without_float_drift() {
        for (s, expected) in [
            ("64Mi", 67108864),
            ("1.5Gi", 1610612736),
            ("1000m", 1),
            ("100m", 1),
            ("1", 1),
            ("1k", 1000),
        ] {
            let q = Quantity::parse(s).unwrap();
            assert_eq!(q.as_bytes(), Some(expected), "{s}");
        }
        assert!(Quantity::parse("64Mi").unwrap().as_milli_cpu().is_none());
    }

    /// KP-43: golden vectors. Expectations follow apimachinery
    /// `resource.Quantity` with Kubernetes' exact milli-value rounding (up), so
    /// a divergence fails here instead of on a live admission path.
    #[test]
    fn golden_vectors_match_apimachinery_and_kubectl() {
        // CPU: one core is 1000m; m/u/n scale below it.
        for (s, millicores) in [
            ("0", 0),
            ("1", 1000),
            ("2", 2000),
            ("2500m", 2500),
            ("1000m", 1000),
            ("500m", 500),
            ("100m", 100),
            ("1m", 1),
            ("0.5", 500),
            ("1.5", 1500),
            ("0.1", 100),
            ("0.001", 1),
            ("0.0001", 1),
            ("1u", 1),
            ("1n", 1),
        ] {
            let q = Quantity::parse(s).unwrap_or_else(|| panic!("{s} must parse"));
            assert_eq!(q.as_milli_cpu(), Some(millicores), "{s} millicores");
        }
        // Memory and storage: a bare number is bytes; Ki/Mi/Gi scale by 1024.
        for (s, bytes) in [
            ("0", 0),
            ("1", 1),
            ("1k", 1000),
            ("1M", 1_000_000),
            ("1G", 1_000_000_000),
            ("1Ki", 1024),
            ("1Mi", 1_048_576),
            ("64Mi", 67_108_864),
            ("128Mi", 134_217_728),
            ("1Gi", 1_073_741_824),
            ("1.5Gi", 1_610_612_736),
            ("0.5Gi", 536_870_912),
            ("1Ti", 1_099_511_627_776),
            ("1000m", 1),
            ("100m", 1),
            ("1.5", 2),
        ] {
            let q = Quantity::parse(s).unwrap_or_else(|| panic!("{s} must parse"));
            assert_eq!(q.as_bytes(), Some(bytes), "{s} bytes");
        }
        // A quantity converts only in its own dimension.
        assert!(Quantity::parse("64Mi").unwrap().as_milli_cpu().is_none());
        assert!(Quantity::parse("1Gi").unwrap().as_milli_cpu().is_none());
        assert!(Quantity::parse("1k").unwrap().as_milli_cpu().is_none());
    }
    /// Forms this parser refuses rather than approximating: a stated subset of
    /// apimachinery, never a silently different value.
    #[test]
    fn refused_forms_are_a_stated_subset() {
        for s in [
            "", " ", "-1", "+1", "1e3", "1E3", "1.2.3", ".5", "1.", "1Wi", "1ki", "1T", "1E",
            "1Pi", "1Ei", "1Gi ", " 1Gi", "1GiB",
        ] {
            assert!(Quantity::parse(s).is_none(), "{s:?} must be refused");
        }
        // Above the i64 range the value is refused, never truncated.
        assert!(Quantity::parse("18446744073709551616").is_some());
        assert!(Quantity::parse("18446744073709551616")
            .unwrap()
            .as_bytes()
            .is_none());
    }
    #[test]
    fn overflow_returns_none() {
        let q = Quantity::parse("999999999999999999999999999999999999999Gi").unwrap();
        assert!(q.as_bytes().is_none());
        assert!(q.as_milli_cpu().is_none());
    }
}
