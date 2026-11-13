//! `repr(float)` of CPython 3: the shortest decimal string that parses back to
//! the same double, in fixed notation while the decimal exponent is within
//! `-4..16` and in `d.ddde±XX` notation otherwise.

/// `repr(value)`; `None` for NaN and infinities, which strict JSON has no
/// spelling for (`json.dumps(..., allow_nan=False)` raises).
#[must_use]
pub fn float_repr(value: f64) -> Option<String> {
    if !value.is_finite() {
        return None;
    }
    if value == 0.0 {
        return Some(
            if value.is_sign_negative() {
                "-0.0"
            } else {
                "0.0"
            }
            .into(),
        );
    }
    // Rust's `{:e}` prints the shortest round-trip digits (the same digit
    // string as David Gay's mode 0, which CPython uses): `d[.ddd]e<exp>`.
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e')?;
    let exponent: i32 = exponent.parse().ok()?;
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    // Value = 0.d1d2... * 10^point.
    let point = exponent + 1;
    let mut text = String::with_capacity(digits.len() + 8);
    if value < 0.0 {
        text.push('-');
    }
    if point <= -4 || point > 16 {
        text.push_str(&digits[..1]);
        if digits.len() > 1 {
            text.push('.');
            text.push_str(&digits[1..]);
        }
        text.push('e');
        text.push(if exponent < 0 { '-' } else { '+' });
        text.push_str(&format!("{:02}", exponent.abs()));
    } else if point <= 0 {
        text.push_str("0.");
        for _ in 0..-point {
            text.push('0');
        }
        text.push_str(&digits);
    } else {
        let point = point as usize;
        if digits.len() <= point {
            text.push_str(&digits);
            for _ in digits.len()..point {
                text.push('0');
            }
            text.push_str(".0");
        } else {
            text.push_str(&digits[..point]);
            text.push('.');
            text.push_str(&digits[point..]);
        }
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::float_repr;

    #[test]
    fn notation_switches_where_cpython_switches() {
        for (value, text) in [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (0.1, "0.1"),
            (-2.5, "-2.5"),
            (1e-4, "0.0001"),
            (1e-5, "1e-05"),
            (1.5e-5, "1.5e-05"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1.2345e16, "1.2345e+16"),
            (123456789012345680.0, "1.2345678901234568e+17"),
            (9999999999999998.0, "9999999999999998.0"),
            (1e22, "1e+22"),
            (1e100, "1e+100"),
            (5e-324, "5e-324"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (0.30000000000000004, "0.30000000000000004"),
            (49.636384244388736, "49.636384244388736"),
            (2.2250738585072014e-308, "2.2250738585072014e-308"),
            (100.0, "100.0"),
            (0.001, "0.001"),
            (123.456, "123.456"),
        ] {
            assert_eq!(float_repr(value).as_deref(), Some(text));
        }
        assert_eq!(float_repr(f64::NAN), None);
        assert_eq!(float_repr(f64::INFINITY), None);
    }
}
