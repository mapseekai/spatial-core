use super::{ColumnSchema, PostgresType};
use crate::postgis::{PostGisError, Result};
use chrono::Timelike;
use gdal::vector::FieldValue;
use std::fmt::Write;

/// Text COPY escaping is separate from SQL quoting and type conversion.
pub(crate) fn push_text(row: &mut Vec<u8>, text: &str, limit: usize) -> Result<()> {
    if text.len() > limit.saturating_sub(row.len())
        || text
            .bytes()
            .filter(|b| matches!(b, b'\\' | b'\n' | b'\r' | b'\t'))
            .count()
            > limit.saturating_sub(row.len() + text.len())
    {
        return Err(PostGisError::ResourceLimit(
            "encoded attributes exceed record budget".into(),
        ));
    }
    for byte in text.bytes() {
        match byte {
            0 => return Err(PostGisError::InvalidInput("text contains NUL".into())),
            b'\\' => row.extend_from_slice(b"\\\\"),
            b'\n' => row.extend_from_slice(b"\\n"),
            b'\r' => row.extend_from_slice(b"\\r"),
            b'\t' => row.extend_from_slice(b"\\t"),
            other => row.push(other),
        }
    }
    Ok(())
}

fn invalid(column: &ColumnSchema, reason: &str) -> PostGisError {
    PostGisError::InvalidInput(format!("column {}: {reason}", column.name.as_str()))
}

pub(crate) fn push_field(
    row: &mut Vec<u8>,
    value: Option<FieldValue>,
    column: &ColumnSchema,
    limit: usize,
) -> Result<()> {
    let Some(value) = value else {
        if !column.nullable {
            return Err(invalid(column, "NULL is not allowed"));
        }
        row.extend_from_slice(b"\\N");
        return Ok(());
    };
    let text = match value {
        FieldValue::StringValue(text) => {
            if matches!(
                column.data_type,
                PostgresType::Real | PostgresType::DoublePrecision
            ) {
                validate_float_text(&text, column)?;
            }
            text
        }
        FieldValue::IntegerValue(value) => value.to_string(),
        FieldValue::Integer64Value(value) => {
            if matches!(
                column.data_type,
                PostgresType::Real | PostgresType::DoublePrecision
            ) && (value as f64) as i128 != i128::from(value)
            {
                return Err(invalid(
                    column,
                    "integer cannot be represented exactly as a float",
                ));
            }
            value.to_string()
        }
        FieldValue::RealValue(value)
            if matches!(
                column.data_type,
                PostgresType::Real | PostgresType::DoublePrecision
            ) =>
        {
            value.to_string()
        }
        FieldValue::DateValue(value) if column.data_type == PostgresType::Date => {
            value.format("%Y-%m-%d").to_string()
        }
        FieldValue::IntegerListValue(values) if column.data_type == PostgresType::IntegerArray => {
            let mut array = String::from("{");
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    array.push(',');
                }
                write!(&mut array, "{value}").expect("writing to String cannot fail");
                if array.len() > limit.saturating_sub(row.len()) {
                    return Err(PostGisError::ResourceLimit(
                        "array exceeds record budget".into(),
                    ));
                }
            }
            array.push('}');
            array
        }
        _ => {
            return Err(invalid(
                column,
                "source value cannot be converted without loss",
            ));
        }
    };
    validate_text(&text, column)?;
    push_text(row, &text, limit)
}

fn validate_text(text: &str, column: &ColumnSchema) -> Result<()> {
    let fail = || invalid(column, "value is invalid or would lose precision");
    match &column.data_type {
        PostgresType::SmallInt => {
            text.parse::<i16>().map_err(|_| fail())?;
        }
        PostgresType::Integer => {
            text.parse::<i32>().map_err(|_| fail())?;
        }
        PostgresType::BigInt => {
            text.parse::<i64>().map_err(|_| fail())?;
        }
        PostgresType::Real => {
            let value = text.parse::<f64>().map_err(|_| fail())?;
            // f32 inputs read by GDAL round-trip through f64 exactly.
            if value.is_finite() && f64::from(value as f32) != value {
                return Err(fail());
            }
        }
        PostgresType::DoublePrecision => {
            text.parse::<f64>().map_err(|_| fail())?;
        }
        PostgresType::Numeric => {
            decimal_shape(text).ok_or_else(fail)?;
        }
        PostgresType::Decimal { precision, scale } => {
            if let Some((integer_digits, required_scale)) = decimal_shape(text).ok_or_else(fail)?
                && (integer_digits > i64::from(*precision) - i64::from(*scale)
                    || required_scale > i64::from(*scale))
            {
                return Err(fail());
            }
        }
        PostgresType::Varchar(n) | PostgresType::Char(n) => {
            if text.chars().count() > *n as usize {
                return Err(fail());
            }
        }
        PostgresType::Boolean => {
            if !matches!(text, "true" | "false" | "t" | "f" | "1" | "0") {
                return Err(fail());
            }
        }
        PostgresType::Date => {
            chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").map_err(|_| fail())?;
        }
        PostgresType::Timestamp(precision) => {
            let value = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")
                .map_err(|_| fail())?;
            if value.nanosecond() % 10u32.pow(9 - u32::from(*precision)) != 0 {
                return Err(fail());
            }
        }
        PostgresType::TimestampTz(precision) => {
            let value = chrono::DateTime::parse_from_rfc3339(text).map_err(|_| fail())?;
            if value.nanosecond() % 10u32.pow(9 - u32::from(*precision)) != 0 {
                return Err(fail());
            }
        }
        // The database validates JSON without forcing its numbers through f64.
        PostgresType::Json | PostgresType::Jsonb => (),
        PostgresType::IntegerArray => {
            let inner = text
                .strip_prefix('{')
                .and_then(|s| s.strip_suffix('}'))
                .ok_or_else(fail)?;
            if !inner.is_empty() {
                for number in inner.split(',') {
                    number.parse::<i32>().map_err(|_| fail())?;
                }
            }
        }
        PostgresType::Text => (),
    }
    Ok(())
}

fn validate_float_text(text: &str, column: &ColumnSchema) -> Result<()> {
    let fail = || {
        invalid(
            column,
            "text cannot be represented exactly as a float; use numeric",
        )
    };
    let value = text.parse::<f64>().map_err(|_| fail())?;
    let value = if column.data_type == PostgresType::Real {
        f64::from(value as f32)
    } else {
        value
    };
    if !value.is_finite() {
        let special = text.strip_prefix(['+', '-']).unwrap_or(text);
        return if special.eq_ignore_ascii_case("nan")
            || special.eq_ignore_ascii_case("inf")
            || special.eq_ignore_ascii_case("infinity")
        {
            Ok(())
        } else {
            Err(fail())
        };
    }
    // 1074 fractional digits represent every finite f64 exactly, including subnormals.
    let exact = format!("{value:.1074}");
    let shape = decimal_shape(text).ok_or_else(fail)?;
    if shape != decimal_shape(&exact).ok_or_else(fail)? {
        return Err(fail());
    }
    if let Some((integer_digits, scale)) = shape {
        let count = (integer_digits + scale) as usize;
        if !significant_digits(text)
            .take(count)
            .eq(significant_digits(&exact).take(count))
        {
            return Err(fail());
        }
    }
    Ok(())
}

fn significant_digits(text: &str) -> impl Iterator<Item = u8> + '_ {
    text.split(['e', 'E'])
        .next()
        .unwrap_or_default()
        .bytes()
        .filter(u8::is_ascii_digit)
        .skip_while(|byte| *byte == b'0')
}

/// Decimal shape without f64, rounding, or allocating exponent-sized strings.
/// Some(None) is zero; Some(Some(..)) gives leading position and required scale.
fn decimal_shape(text: &str) -> Option<Option<(i64, i64)>> {
    let text = text.strip_prefix(['+', '-']).unwrap_or(text);
    let (mantissa, exponent) = match text.find(['e', 'E']) {
        Some(index) => (&text[..index], text[index + 1..].parse::<i32>().ok()?),
        None => (text, 0),
    };
    let mut digits = 0i64;
    let mut decimal = None;
    let mut first = None;
    let mut last = 0;
    for byte in mantissa.bytes() {
        if byte == b'.' && decimal.is_none() {
            decimal = Some(digits);
            continue;
        }
        if !byte.is_ascii_digit() {
            return None;
        }
        if byte != b'0' {
            first.get_or_insert(digits);
            last = digits;
        }
        digits += 1;
    }
    if digits == 0 {
        return None;
    }
    let point = decimal.unwrap_or(digits) + i64::from(exponent);
    let Some(first) = first else {
        return Some(None);
    };
    let integer_digits = point - first;
    let scale = last + 1 - point;
    if integer_digits > 131_072 || scale > 16_383 {
        return None;
    }
    Some(Some((integer_digits, scale)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn textual_floats_require_exact_binary_values() -> Result<()> {
        for (kind, accepted, rejected) in [
            (
                PostgresType::DoublePrecision,
                vec![
                    "9007199254740992",
                    "9007199254740994",
                    "-0",
                    "5e-1",
                    "1.2500",
                    "NaN",
                    "-Infinity",
                    "0.1000000000000000055511151231257827021181583404541015625",
                ],
                vec![
                    "9007199254740993",
                    "-9007199254740993",
                    "1.0000000000000001",
                    "0.1",
                    "1e309",
                    "1e-400",
                ],
            ),
            (
                PostgresType::Real,
                vec!["16777216", "16777218", "0.5"],
                vec!["16777217", "1.00000001", "0.1", "1e39", "1e-50"],
            ),
        ] {
            let column = ColumnSchema::new("value", kind)?;
            for text in accepted {
                push_field(
                    &mut Vec::new(),
                    Some(FieldValue::StringValue(text.into())),
                    &column,
                    4096,
                )?;
            }
            for text in rejected {
                let mut row = Vec::new();
                assert!(
                    push_field(
                        &mut row,
                        Some(FieldValue::StringValue(text.into())),
                        &column,
                        4096
                    )
                    .is_err(),
                    "{text}"
                );
                assert!(row.is_empty());
            }
        }
        let double = ColumnSchema::new("value", PostgresType::DoublePrecision)?;
        for value in [f64::from_bits(1), f64::MAX, -f64::MIN_POSITIVE, 0.1] {
            let exact = format!("{value:.1074}");
            push_field(
                &mut Vec::new(),
                Some(FieldValue::StringValue(exact)),
                &double,
                4096,
            )?;
            // Native OGR Real is already a binary float, so its identity conversion stays valid.
            push_field(
                &mut Vec::new(),
                Some(FieldValue::RealValue(value)),
                &double,
                4096,
            )?;
        }
        Ok(())
    }

    #[test]
    fn copy_text_preserves_null_marker_and_control_characters() -> Result<()> {
        let mut encoded = Vec::new();
        push_text(&mut encoded, "\\N\tline\n\\", usize::MAX)?;
        assert_eq!(encoded, b"\\\\N\\tline\\n\\\\");
        assert!(push_text(&mut Vec::new(), "\0", usize::MAX).is_err());
        let mut bounded = vec![1];
        assert!(matches!(
            push_text(&mut bounded, "\\\\", 4),
            Err(PostGisError::ResourceLimit(_))
        ));
        assert_eq!(bounded, [1]);
        Ok(())
    }
    #[test]
    fn decimals_reject_rounding_without_fixed_precision_arithmetic() -> Result<()> {
        let column = ColumnSchema::new(
            "value",
            PostgresType::Decimal {
                precision: 30,
                scale: 12,
            },
        )?;
        assert!(validate_text("123456789012345678.123456789012", &column).is_ok());
        assert!(validate_text("123456789012345678.1234567890121", &column).is_err());
        let negative = ColumnSchema::new(
            "value",
            PostgresType::Decimal {
                precision: 2,
                scale: -2,
            },
        )?;
        assert!(validate_text("1200", &negative).is_ok());
        assert!(validate_text("1230", &negative).is_err());
        assert!(validate_text("1.2e3", &negative).is_ok());
        assert!(validate_text("NaN", &negative).is_err());
        assert!(validate_text("0", &negative).is_ok());
        Ok(())
    }
    #[test]
    fn values_cannot_be_silently_truncated_or_reinterpreted() -> Result<()> {
        assert!(
            push_field(
                &mut Vec::new(),
                Some(FieldValue::Integer64Value(9007199254740993)),
                &ColumnSchema::new("v", PostgresType::DoublePrecision)?,
                usize::MAX
            )
            .is_err()
        );
        assert!(validate_text("abcd", &ColumnSchema::new("v", PostgresType::Varchar(3))?).is_err());
        assert!(validate_text("1.5", &ColumnSchema::new("v", PostgresType::Integer)?).is_err());
        assert!(
            validate_text(
                "2026-01-01T12:00:00",
                &ColumnSchema::new("v", PostgresType::TimestampTz(6))?
            )
            .is_err()
        );
        assert!(
            validate_text(
                "2026-01-01T12:00:00.1234+08:00",
                &ColumnSchema::new("v", PostgresType::TimestampTz(3))?
            )
            .is_err()
        );
        Ok(())
    }
}
