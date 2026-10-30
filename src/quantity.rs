//! Kubernetes quantities ("250m", "1.5Gi", "1e3") as numbers.
//!
//! The fuzz targets in `fuzz/` build this file on its own: keep it to std, regex and serde.

/// A Kubernetes quantity, split as (number, power of ten, binary multiplier): a
/// decimal number with an optional exponent (`1e3`, `5E-2`), then an SI suffix
/// (`n u m k M G T P E`) or a binary one (`Ki Mi Gi Ti Pi Ei`). None when the
/// number does not parse; an unknown suffix counts as none.
fn split_quantity(q: &str) -> Option<(f64, i32, f64)> {
    let q = q.trim();
    let end = q
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || c == '.' || (i == 0 && (c == '+' || c == '-'))))
        .map_or(q.len(), |(i, _)| i);
    let (mut num, mut suffix) = q.split_at(end);
    // `e`/`E` and a signed integer is a decimal exponent; a lone `E` is exa.
    if let Some(exp) = suffix.strip_prefix(['e', 'E']) {
        let digits = exp.strip_prefix(['+', '-']).unwrap_or(exp);
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            (num, suffix) = (q, "");
        }
    }
    let n: f64 = num.parse().ok()?;
    Some(match suffix {
        "n" => (n, -9, 1.0),
        "u" => (n, -6, 1.0),
        "m" => (n, -3, 1.0),
        "k" | "K" => (n, 3, 1.0),
        "M" => (n, 6, 1.0),
        "G" => (n, 9, 1.0),
        "T" => (n, 12, 1.0),
        "P" => (n, 15, 1.0),
        "E" => (n, 18, 1.0),
        "Ki" => (n, 0, 1024f64),
        "Mi" => (n, 0, 1024f64.powi(2)),
        "Gi" => (n, 0, 1024f64.powi(3)),
        "Ti" => (n, 0, 1024f64.powi(4)),
        "Pi" => (n, 0, 1024f64.powi(5)),
        "Ei" => (n, 0, 1024f64.powi(6)),
        _ => (n, 0, 1.0),
    })
}

/// The quantity times 10^`shift` (dividing for negative powers keeps "250m" exact).
fn quantity_scaled(q: &str, shift: i32) -> Option<f64> {
    let (n, exp, binary) = split_quantity(q)?;
    let exp = exp + shift;
    let n = n * binary;
    Some(if exp >= 0 { n * 10f64.powi(exp) } else { n / 10f64.powi(-exp) })
}

/// CPU quantity to millicores: "250m", "1", "123456n", "12u", "2k", "1e3".
pub fn parse_cpu(q: &str) -> f64 {
    quantity_scaled(q, 3).unwrap_or(0.0)
}

/// Whether two quantities are the same amount, written differently ("1000m" and "1",
/// "1Gi" and "1024Mi"). False when either does not parse.
pub fn same_amount(a: &str, b: &str) -> bool {
    match (quantity_scaled(a, 0), quantity_scaled(b, 0)) {
        (Some(x), Some(y)) => (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0),
        _ => false,
    }
}

/// Memory quantity to bytes: "128Mi", "1Gi", "500M", "12345Ki", "1000", "1e9", "1Pi".
pub fn parse_bytes(q: &str) -> f64 {
    quantity_scaled(q, 0).unwrap_or(0.0)
}
