/// Parses speeds like `500K`, `2M`, `1.5G`, `2MiB/s`, or plain bytes into bytes
/// per second. Units are binary (1K = 1024).
pub fn parse_rate(value: &str) -> Result<u64, String> {
    let invalid = || format!("`{value}` is not a speed (try 500K, 2M, or 1.5G)");
    let trimmed = value.trim();
    let trimmed = trimmed.strip_suffix("/s").unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix(['B', 'b']).unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix(['i', 'I']).unwrap_or(trimmed);

    let split = trimmed
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split);
    let multiplier: u64 = match unit.to_ascii_uppercase().as_str() {
        "" => 1,
        "K" => 1 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        _ => return Err(invalid()),
    };
    let number: f64 = number.trim().parse().map_err(|_| invalid())?;
    if !number.is_finite() || number <= 0.0 {
        return Err(invalid());
    }
    Ok((number * multiplier as f64).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_forms() {
        assert_eq!(parse_rate("1000"), Ok(1000));
        assert_eq!(parse_rate("500K"), Ok(500 * 1024));
        assert_eq!(parse_rate("2M"), Ok(2 * 1024 * 1024));
        assert_eq!(parse_rate("1.5G"), Ok(1_610_612_736));
        assert_eq!(parse_rate("2MiB/s"), Ok(2 * 1024 * 1024));
        assert_eq!(parse_rate("64kb"), Ok(64 * 1024));
    }

    #[test]
    fn rejects_nonsense() {
        assert!(parse_rate("fast").is_err());
        assert!(parse_rate("0").is_err());
        assert!(parse_rate("-1M").is_err());
        assert!(parse_rate("5T").is_err());
    }
}
