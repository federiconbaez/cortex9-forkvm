//! Percentiles por rango más cercano (sin interpolar) y JSON mínimo.

/// Percentil `p` (0..=100) por rango más cercano sobre una copia ordenada.
pub fn percentile(values: &[u64], p: f64) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    Some(v[rank.clamp(1, v.len()) - 1])
}

/// Mediana, p99 y máximo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    pub n: usize,
    pub p50: u64,
    pub p99: u64,
    pub max: u64,
}

pub fn summarize(values: &[u64]) -> Summary {
    Summary {
        n: values.len(),
        p50: percentile(values, 50.0).unwrap_or(0),
        p99: percentile(values, 99.0).unwrap_or(0),
        max: values.iter().copied().max().unwrap_or(0),
    }
}

/// Escapa una cadena para JSON.
pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&v, 50.0), Some(50));
        assert_eq!(percentile(&v, 99.0), Some(99));
        assert_eq!(percentile(&v, 100.0), Some(100));
        assert_eq!(percentile(&v, 0.0), Some(1));
        assert_eq!(percentile(&[7], 99.0), Some(7));
        assert_eq!(percentile(&[], 50.0), None);
        // Sin ordenar.
        assert_eq!(percentile(&[5, 1, 3], 50.0), Some(3));
    }

    #[test]
    fn small_samples_p99_is_max() {
        let v = [10, 30, 20, 40];
        let s = summarize(&v);
        assert_eq!((s.n, s.p50, s.p99, s.max), (4, 20, 40, 40));
    }

    #[test]
    fn json_escaping() {
        assert_eq!(json_str("a\"b\\c\nd\u{1}"), "\"a\\\"b\\\\c\\nd\\u0001\"");
        assert_eq!(json_str("ñ"), "\"ñ\"");
    }
}
