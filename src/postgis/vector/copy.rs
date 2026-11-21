use super::{ColumnSchema, PostgresType};
use crate::postgis::{PostGisError, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Timelike, Utc};
use gdal::vector::FieldValue;

const INT4_OID: u32 = 23;
const JSONB_BINARY_VERSION: u8 = 1;
const NUMERIC_POS: u16 = 0x0000;
const NUMERIC_NEG: u16 = 0x4000;

fn pg_epoch_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(2000, 1, 1).expect("valid PostgreSQL epoch date")
}

fn pg_epoch_naive() -> NaiveDateTime {
    pg_epoch_date()
        .and_hms_opt(0, 0, 0)
        .expect("valid PostgreSQL epoch timestamp")
}

fn pg_epoch_utc() -> DateTime<Utc> {
    DateTime::from_naive_utc_and_offset(pg_epoch_naive(), Utc)
}

fn budget_exceeded() -> PostGisError {
    PostGisError::ResourceLimit("encoded attributes exceed record budget".into())
}

fn invalid(column: &ColumnSchema, reason: &str) -> PostGisError {
    PostGisError::InvalidInput(format!("column {}: {reason}", column.name.as_str()))
}

fn reserve_field(row: &mut Vec<u8>, len: usize, limit: usize) -> Result<()> {
    let encoded_len = i32::try_from(len).map_err(|_| budget_exceeded())?;
    if 4usize
        .checked_add(len)
        .and_then(|n| row.len().checked_add(n))
        .is_none_or(|total| total > limit)
    {
        return Err(budget_exceeded());
    }
    row.extend_from_slice(&encoded_len.to_be_bytes());
    Ok(())
}

/// Encode a single binary COPY field payload (`i32` length prefix + raw bytes).
pub(crate) fn push_bytes_field(row: &mut Vec<u8>, bytes: &[u8], limit: usize) -> Result<()> {
    reserve_field(row, bytes.len(), limit)?;
    row.extend_from_slice(bytes);
    Ok(())
}

/// Encode a binary COPY NULL marker (`-1i32`).
pub(crate) fn push_null_field(row: &mut Vec<u8>, limit: usize) -> Result<()> {
    if row.len().checked_add(4).is_none_or(|total| total > limit) {
        return Err(budget_exceeded());
    }
    row.extend_from_slice(&(-1i32).to_be_bytes());
    Ok(())
}

fn push_utf8(row: &mut Vec<u8>, text: &str, limit: usize) -> Result<()> {
    if text.as_bytes().contains(&0) {
        return Err(PostGisError::InvalidInput("text contains NUL".into()));
    }
    push_bytes_field(row, text.as_bytes(), limit)
}

fn push_jsonb(row: &mut Vec<u8>, text: &str, limit: usize) -> Result<()> {
    if text.as_bytes().contains(&0) {
        return Err(PostGisError::InvalidInput("text contains NUL".into()));
    }
    let payload_len = text.len().checked_add(1).ok_or_else(budget_exceeded)?;
    reserve_field(row, payload_len, limit)?;
    row.push(JSONB_BINARY_VERSION);
    row.extend_from_slice(text.as_bytes());
    Ok(())
}

fn push_bool(row: &mut Vec<u8>, value: bool, limit: usize) -> Result<()> {
    push_bytes_field(row, &[u8::from(value)], limit)
}

fn push_date(
    row: &mut Vec<u8>,
    date: NaiveDate,
    column: &ColumnSchema,
    limit: usize,
) -> Result<()> {
    let days = i32::try_from((date - pg_epoch_date()).num_days())
        .map_err(|_| invalid(column, "date is outside PostgreSQL range"))?;
    push_bytes_field(row, &days.to_be_bytes(), limit)
}

fn push_int32_array(row: &mut Vec<u8>, values: &[i32], limit: usize) -> Result<()> {
    if values.is_empty() {
        reserve_field(row, 12, limit)?;
        row.extend_from_slice(&0i32.to_be_bytes()); // ndim
        row.extend_from_slice(&0i32.to_be_bytes()); // has_null
        row.extend_from_slice(&INT4_OID.to_be_bytes());
        return Ok(());
    }
    let count = i32::try_from(values.len()).map_err(|_| budget_exceeded())?;
    let payload_len = values
        .len()
        .checked_mul(8)
        .and_then(|n| n.checked_add(20))
        .ok_or_else(budget_exceeded)?;
    reserve_field(row, payload_len, limit)?;
    row.extend_from_slice(&1i32.to_be_bytes()); // ndim = 1
    row.extend_from_slice(&0i32.to_be_bytes()); // has_null = 0
    row.extend_from_slice(&INT4_OID.to_be_bytes());
    row.extend_from_slice(&count.to_be_bytes()); // dim_len
    row.extend_from_slice(&1i32.to_be_bytes()); // lower_bound = 1
    for &value in values {
        row.extend_from_slice(&4i32.to_be_bytes());
        row.extend_from_slice(&value.to_be_bytes());
    }
    Ok(())
}

/// Encode one attribute value into PostgreSQL `COPY ... FORMAT binary` wire format.
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
        return push_null_field(row, limit);
    };
    let fail = || invalid(column, "value is invalid or would lose precision");
    match value {
        FieldValue::StringValue(text) => push_str_field(row, &text, column, limit),
        FieldValue::IntegerValue(v) => match &column.data_type {
            PostgresType::SmallInt => {
                let narrowed = i16::try_from(v).map_err(|_| fail())?;
                push_bytes_field(row, &narrowed.to_be_bytes(), limit)
            }
            PostgresType::Integer => push_bytes_field(row, &v.to_be_bytes(), limit),
            PostgresType::BigInt => push_bytes_field(row, &i64::from(v).to_be_bytes(), limit),
            PostgresType::Real => {
                if (v as f32) as i64 != i64::from(v) {
                    return Err(fail());
                }
                push_bytes_field(row, &(v as f32).to_bits().to_be_bytes(), limit)
            }
            PostgresType::DoublePrecision => {
                push_bytes_field(row, &f64::from(v).to_bits().to_be_bytes(), limit)
            }
            PostgresType::Boolean => match v {
                0 => push_bool(row, false, limit),
                1 => push_bool(row, true, limit),
                _ => Err(fail()),
            },
            PostgresType::Numeric
            | PostgresType::Decimal { .. }
            | PostgresType::Text
            | PostgresType::Varchar(_)
            | PostgresType::Char(_)
            | PostgresType::Json
            | PostgresType::Jsonb => push_str_field(row, &v.to_string(), column, limit),
            _ => Err(fail()),
        },
        FieldValue::Integer64Value(v) => match &column.data_type {
            PostgresType::SmallInt => {
                let narrowed = i16::try_from(v).map_err(|_| fail())?;
                push_bytes_field(row, &narrowed.to_be_bytes(), limit)
            }
            PostgresType::Integer => {
                let narrowed = i32::try_from(v).map_err(|_| fail())?;
                push_bytes_field(row, &narrowed.to_be_bytes(), limit)
            }
            PostgresType::BigInt => push_bytes_field(row, &v.to_be_bytes(), limit),
            PostgresType::Real => {
                if (v as f32) as i128 != i128::from(v) {
                    return Err(invalid(
                        column,
                        "integer cannot be represented exactly as a float",
                    ));
                }
                push_bytes_field(row, &(v as f32).to_bits().to_be_bytes(), limit)
            }
            PostgresType::DoublePrecision => {
                if (v as f64) as i128 != i128::from(v) {
                    return Err(invalid(
                        column,
                        "integer cannot be represented exactly as a float",
                    ));
                }
                push_bytes_field(row, &(v as f64).to_bits().to_be_bytes(), limit)
            }
            PostgresType::Boolean => match v {
                0 => push_bool(row, false, limit),
                1 => push_bool(row, true, limit),
                _ => Err(fail()),
            },
            PostgresType::Numeric
            | PostgresType::Decimal { .. }
            | PostgresType::Text
            | PostgresType::Varchar(_)
            | PostgresType::Char(_)
            | PostgresType::Json
            | PostgresType::Jsonb => push_str_field(row, &v.to_string(), column, limit),
            _ => Err(fail()),
        },
        FieldValue::RealValue(v) => match &column.data_type {
            PostgresType::Real => {
                if v.is_finite() && f64::from(v as f32) != v {
                    return Err(fail());
                }
                push_bytes_field(row, &(v as f32).to_bits().to_be_bytes(), limit)
            }
            PostgresType::DoublePrecision => {
                push_bytes_field(row, &v.to_bits().to_be_bytes(), limit)
            }
            _ => Err(invalid(
                column,
                "source value cannot be converted without loss",
            )),
        },
        FieldValue::DateValue(date) if column.data_type == PostgresType::Date => {
            push_date(row, date, column, limit)
        }
        FieldValue::IntegerListValue(values) if column.data_type == PostgresType::IntegerArray => {
            push_int32_array(row, &values, limit)
        }
        _ => Err(invalid(
            column,
            "source value cannot be converted without loss",
        )),
    }
}

fn push_str_field(
    row: &mut Vec<u8>,
    text: &str,
    column: &ColumnSchema,
    limit: usize,
) -> Result<()> {
    let fail = || invalid(column, "value is invalid or would lose precision");
    match &column.data_type {
        PostgresType::SmallInt => {
            let value = text.parse::<i16>().map_err(|_| fail())?;
            push_bytes_field(row, &value.to_be_bytes(), limit)
        }
        PostgresType::Integer => {
            let value = text.parse::<i32>().map_err(|_| fail())?;
            push_bytes_field(row, &value.to_be_bytes(), limit)
        }
        PostgresType::BigInt => {
            let value = text.parse::<i64>().map_err(|_| fail())?;
            push_bytes_field(row, &value.to_be_bytes(), limit)
        }
        PostgresType::Real => {
            let value = validate_float_text(text, column)?;
            push_bytes_field(row, &(value as f32).to_bits().to_be_bytes(), limit)
        }
        PostgresType::DoublePrecision => {
            let value = validate_float_text(text, column)?;
            push_bytes_field(row, &value.to_bits().to_be_bytes(), limit)
        }
        PostgresType::Numeric => push_numeric(row, text, None, column, limit),
        PostgresType::Decimal { precision, scale } => {
            push_numeric(row, text, Some((*precision, *scale)), column, limit)
        }
        PostgresType::Text => push_utf8(row, text, limit),
        PostgresType::Varchar(n) | PostgresType::Char(n) => {
            if text.chars().count() > *n as usize {
                return Err(fail());
            }
            push_utf8(row, text, limit)
        }
        PostgresType::Boolean => match text {
            "true" | "t" | "1" => push_bool(row, true, limit),
            "false" | "f" | "0" => push_bool(row, false, limit),
            _ => Err(fail()),
        },
        PostgresType::Date => {
            let date = NaiveDate::parse_from_str(text, "%Y-%m-%d").map_err(|_| fail())?;
            push_date(row, date, column, limit)
        }
        PostgresType::Timestamp(precision) => {
            let value =
                NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f").map_err(|_| fail())?;
            if value.nanosecond() >= 1_000_000_000
                || value.nanosecond() % 10u32.pow(9 - u32::from(*precision)) != 0
            {
                return Err(fail());
            }
            let micros = (value - pg_epoch_naive())
                .num_microseconds()
                .ok_or_else(fail)?;
            push_bytes_field(row, &micros.to_be_bytes(), limit)
        }
        PostgresType::TimestampTz(precision) => {
            let value = DateTime::parse_from_rfc3339(text).map_err(|_| fail())?;
            if value.nanosecond() >= 1_000_000_000
                || value.nanosecond() % 10u32.pow(9 - u32::from(*precision)) != 0
            {
                return Err(fail());
            }
            let micros = (value.with_timezone(&Utc) - pg_epoch_utc())
                .num_microseconds()
                .ok_or_else(fail)?;
            push_bytes_field(row, &micros.to_be_bytes(), limit)
        }
        // The database validates JSON without forcing its numbers through f64.
        PostgresType::Json => push_utf8(row, text, limit),
        PostgresType::Jsonb => push_jsonb(row, text, limit),
        PostgresType::IntegerArray => {
            let inner = text
                .strip_prefix('{')
                .and_then(|s| s.strip_suffix('}'))
                .ok_or_else(fail)?;
            if inner.is_empty() {
                return push_int32_array(row, &[], limit);
            }
            let mut values = Vec::new();
            for number in inner.split(',') {
                values.push(number.parse::<i32>().map_err(|_| fail())?);
                if values
                    .len()
                    .checked_mul(8)
                    .and_then(|n| n.checked_add(24))
                    .and_then(|n| row.len().checked_add(n))
                    .is_none_or(|total| total > limit)
                {
                    return Err(PostGisError::ResourceLimit(
                        "array exceeds record budget".into(),
                    ));
                }
            }
            push_int32_array(row, &values, limit)
        }
    }
}

fn push_numeric(
    row: &mut Vec<u8>,
    text: &str,
    constraint: Option<(u16, i16)>,
    column: &ColumnSchema,
    limit: usize,
) -> Result<()> {
    let fail = || invalid(column, "value is invalid or would lose precision");
    let parsed = parse_decimal(text).ok_or_else(fail)?;
    if let Some((precision, scale)) = constraint
        && let Some((integer_digits, required_scale)) = parsed.shape
        && (integer_digits > i64::from(precision) - i64::from(scale)
            || required_scale > i64::from(scale))
    {
        return Err(fail());
    }
    let Some((first, last)) = parsed.nonzero_bounds else {
        let dscale = match constraint {
            Some((_, scale)) => parsed.explicit_scale.clamp(0, i64::from(scale).max(0)) as u16,
            None => parsed.explicit_scale.clamp(0, 16_383) as u16,
        };
        reserve_field(row, 8, limit)?;
        row.extend_from_slice(&0u16.to_be_bytes()); // ndigits
        row.extend_from_slice(&0i16.to_be_bytes()); // weight
        row.extend_from_slice(&NUMERIC_POS.to_be_bytes());
        row.extend_from_slice(&dscale.to_be_bytes());
        return Ok(());
    };
    let required_scale = parsed.shape.expect("non-zero shape").1;
    let dscale = match constraint {
        Some((_, scale)) => parsed
            .explicit_scale
            .clamp(required_scale.max(0), i64::from(scale).max(0))
            as u16,
        None => parsed.explicit_scale.clamp(required_scale.max(0), 16_383) as u16,
    };
    let p_max = parsed.point - 1 - first;
    let p_min = parsed.point - 1 - last;
    let g_max = p_max.div_euclid(4);
    let g_min = p_min.div_euclid(4);
    let weight = i16::try_from(g_max).map_err(|_| fail())?;
    let ndigits = u16::try_from(g_max - g_min + 1).map_err(|_| fail())?;
    let payload_len = 8usize
        .checked_add(usize::from(ndigits) * 2)
        .ok_or_else(budget_exceeded)?;
    reserve_field(row, payload_len, limit)?;
    let mut digits = vec![0u16; usize::from(ndigits)];
    const POWERS: [u16; 4] = [1, 10, 100, 1000];
    let mut index = 0i64;
    for byte in parsed.mantissa.bytes() {
        if byte == b'.' {
            continue;
        }
        if index >= first && index <= last && byte != b'0' {
            let p = parsed.point - 1 - index;
            let group = (g_max - p.div_euclid(4)) as usize;
            let rem = p.rem_euclid(4) as usize;
            digits[group] += u16::from(byte - b'0') * POWERS[rem];
        }
        index += 1;
    }
    let sign = if parsed.negative {
        NUMERIC_NEG
    } else {
        NUMERIC_POS
    };
    row.extend_from_slice(&ndigits.to_be_bytes());
    row.extend_from_slice(&weight.to_be_bytes());
    row.extend_from_slice(&sign.to_be_bytes());
    row.extend_from_slice(&dscale.to_be_bytes());
    for digit in digits {
        row.extend_from_slice(&digit.to_be_bytes());
    }
    Ok(())
}

fn validate_float_text(text: &str, column: &ColumnSchema) -> Result<f64> {
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
            Ok(value)
        } else {
            Err(fail())
        };
    }
    // 1074 fractional digits represent every finite f64 exactly, including subnormals.
    let exact = format!("{value:.1074}");
    let shape = parse_decimal(text).ok_or_else(fail)?.shape;
    if shape != parse_decimal(&exact).ok_or_else(fail)?.shape {
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
    Ok(value)
}

fn significant_digits(text: &str) -> impl Iterator<Item = u8> + '_ {
    text.split(['e', 'E'])
        .next()
        .unwrap_or_default()
        .bytes()
        .filter(u8::is_ascii_digit)
        .skip_while(|byte| *byte == b'0')
}

struct ParsedDecimal<'a> {
    negative: bool,
    mantissa: &'a str,
    point: i64,
    explicit_scale: i64,
    nonzero_bounds: Option<(i64, i64)>,
    /// None is zero; Some((integer_digits, required_scale)) for non-zero values.
    shape: Option<(i64, i64)>,
}

/// Parse decimal syntax without f64, rounding, or allocating exponent-sized strings.
fn parse_decimal(text: &str) -> Option<ParsedDecimal<'_>> {
    let negative = text.starts_with('-');
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
    let frac_digits = decimal.map_or(0, |d| digits - d);
    let explicit_scale = (frac_digits - i64::from(exponent)).max(0);
    let point = decimal.unwrap_or(digits) + i64::from(exponent);
    let Some(first) = first else {
        return Some(ParsedDecimal {
            negative: false,
            mantissa,
            point,
            explicit_scale,
            nonzero_bounds: None,
            shape: None,
        });
    };
    let integer_digits = point - first;
    let scale = last + 1 - point;
    if integer_digits > 131_072 || scale > 16_383 {
        return None;
    }
    Some(ParsedDecimal {
        negative,
        mantissa,
        point,
        explicit_scale,
        nonzero_bounds: Some((first, last)),
        shape: Some((integer_digits, scale)),
    })
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
                    "+NaN",
                    "-NaN",
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
                vec!["16777216", "16777218", "0.5", "+NaN", "-NaN"],
                vec!["16777217", "1.00000001", "0.1", "1e39", "1e-50"],
            ),
        ] {
            let column = ColumnSchema::new("value", kind)?;
            for text in accepted {
                let mut row = Vec::new();
                push_field(
                    &mut row,
                    Some(FieldValue::StringValue(text.into())),
                    &column,
                    4096,
                )?;
                let expected_len = if column.data_type == PostgresType::Real {
                    8
                } else {
                    12
                };
                assert_eq!(row.len(), expected_len, "{text}");
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
            let mut from_text = Vec::new();
            push_field(
                &mut from_text,
                Some(FieldValue::StringValue(exact)),
                &double,
                4096,
            )?;
            let mut from_real = Vec::new();
            push_field(
                &mut from_real,
                Some(FieldValue::RealValue(value)),
                &double,
                4096,
            )?;
            assert_eq!(from_text, from_real);
        }
        Ok(())
    }

    #[test]
    fn binary_text_preserves_control_characters_rejects_nul_and_enforces_budget() -> Result<()> {
        let column = ColumnSchema::new("text", PostgresType::Text)?;
        let mut encoded = Vec::new();
        push_field(
            &mut encoded,
            Some(FieldValue::StringValue("\\N\tline\n\\".into())),
            &column,
            usize::MAX,
        )?;
        assert_eq!(&encoded[..4], &9i32.to_be_bytes());
        assert_eq!(&encoded[4..], b"\\N\tline\n\\");

        let mut nul_row = Vec::new();
        assert!(
            push_field(
                &mut nul_row,
                Some(FieldValue::StringValue("\0".into())),
                &column,
                usize::MAX,
            )
            .is_err()
        );
        assert!(nul_row.is_empty());

        let mut bounded = vec![1];
        assert!(matches!(
            push_field(
                &mut bounded,
                Some(FieldValue::StringValue("ab".into())),
                &column,
                6,
            ),
            Err(PostGisError::ResourceLimit(_))
        ));
        assert_eq!(bounded, [1]);
        Ok(())
    }

    #[test]
    fn binary_decimals_preserve_exact_digits_and_reject_rounding() -> Result<()> {
        let column = ColumnSchema::new(
            "value",
            PostgresType::Decimal {
                precision: 30,
                scale: 12,
            },
        )?;
        let mut row = Vec::new();
        push_str_field(&mut row, "123456789012345678.123456789012", &column, 4096)?;
        assert_eq!(i32::from_be_bytes(row[0..4].try_into().unwrap()), 24);
        assert_eq!(u16::from_be_bytes(row[4..6].try_into().unwrap()), 8); // ndigits
        assert_eq!(i16::from_be_bytes(row[6..8].try_into().unwrap()), 4); // weight
        assert_eq!(
            u16::from_be_bytes(row[8..10].try_into().unwrap()),
            NUMERIC_POS
        );
        assert_eq!(u16::from_be_bytes(row[10..12].try_into().unwrap()), 12); // dscale
        let digits: Vec<u16> = row[12..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&chunk| u16::from_be_bytes(chunk))
            .collect();
        assert_eq!(digits, [12, 3456, 7890, 1234, 5678, 1234, 5678, 9012]);

        assert!(
            push_str_field(
                &mut Vec::new(),
                "123456789012345678.1234567890121",
                &column,
                4096
            )
            .is_err()
        );

        let negative = ColumnSchema::new(
            "value",
            PostgresType::Decimal {
                precision: 2,
                scale: -2,
            },
        )?;
        let mut neg_row = Vec::new();
        push_str_field(&mut neg_row, "-1.2e3", &negative, 4096)?;
        assert_eq!(u16::from_be_bytes(neg_row[4..6].try_into().unwrap()), 1);
        assert_eq!(i16::from_be_bytes(neg_row[6..8].try_into().unwrap()), 0);
        assert_eq!(
            u16::from_be_bytes(neg_row[8..10].try_into().unwrap()),
            NUMERIC_NEG
        );
        assert_eq!(u16::from_be_bytes(neg_row[10..12].try_into().unwrap()), 0);
        assert_eq!(
            u16::from_be_bytes(neg_row[12..14].try_into().unwrap()),
            1200
        );

        assert!(push_str_field(&mut Vec::new(), "1200", &negative, 4096).is_ok());
        assert!(push_str_field(&mut Vec::new(), "1230", &negative, 4096).is_err());
        assert!(push_str_field(&mut Vec::new(), "NaN", &negative, 4096).is_err());
        let mut zero_row = Vec::new();
        push_str_field(&mut zero_row, "-0", &negative, 4096)?;
        assert_eq!(&zero_row[4..12], &[0, 0, 0, 0, 0, 0, 0, 0]);
        Ok(())
    }

    #[test]
    fn binary_arrays_and_jsonb_encode_postgres_wire_headers() -> Result<()> {
        let array_col = ColumnSchema::new("arr", PostgresType::IntegerArray)?;
        let mut empty = Vec::new();
        push_field(
            &mut empty,
            Some(FieldValue::IntegerListValue(Vec::new())),
            &array_col,
            4096,
        )?;
        assert_eq!(i32::from_be_bytes(empty[0..4].try_into().unwrap()), 12);
        assert_eq!(i32::from_be_bytes(empty[4..8].try_into().unwrap()), 0);
        assert_eq!(u32::from_be_bytes(empty[12..16].try_into().unwrap()), 23);

        let mut non_empty = Vec::new();
        push_field(
            &mut non_empty,
            Some(FieldValue::IntegerListValue(vec![2, -1])),
            &array_col,
            4096,
        )?;
        assert_eq!(i32::from_be_bytes(non_empty[0..4].try_into().unwrap()), 36);
        assert_eq!(i32::from_be_bytes(non_empty[4..8].try_into().unwrap()), 1);
        assert_eq!(i32::from_be_bytes(non_empty[16..20].try_into().unwrap()), 2);
        assert_eq!(i32::from_be_bytes(non_empty[24..28].try_into().unwrap()), 4);
        assert_eq!(i32::from_be_bytes(non_empty[28..32].try_into().unwrap()), 2);
        assert_eq!(
            i32::from_be_bytes(non_empty[36..40].try_into().unwrap()),
            -1
        );

        let jsonb_col = ColumnSchema::new("doc", PostgresType::Jsonb)?;
        let mut jsonb = Vec::new();
        push_field(
            &mut jsonb,
            Some(FieldValue::StringValue(r#"{"a":1}"#.into())),
            &jsonb_col,
            4096,
        )?;
        assert_eq!(i32::from_be_bytes(jsonb[0..4].try_into().unwrap()), 8);
        assert_eq!(jsonb[4], JSONB_BINARY_VERSION);
        assert_eq!(&jsonb[5..], br#"{"a":1}"#);
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
        assert!(
            push_field(
                &mut Vec::new(),
                Some(FieldValue::IntegerValue(16777217)),
                &ColumnSchema::new("v", PostgresType::Real)?,
                usize::MAX
            )
            .is_err()
        );
        assert!(
            push_str_field(
                &mut Vec::new(),
                "abcd",
                &ColumnSchema::new("v", PostgresType::Varchar(3))?,
                4096
            )
            .is_err()
        );
        assert!(
            push_str_field(
                &mut Vec::new(),
                "1.5",
                &ColumnSchema::new("v", PostgresType::Integer)?,
                4096
            )
            .is_err()
        );
        assert!(
            push_str_field(
                &mut Vec::new(),
                "2026-01-01T12:00:00",
                &ColumnSchema::new("v", PostgresType::TimestampTz(6))?,
                4096
            )
            .is_err()
        );
        assert!(
            push_str_field(
                &mut Vec::new(),
                "2026-01-01T12:00:00.1234+08:00",
                &ColumnSchema::new("v", PostgresType::TimestampTz(3))?,
                4096
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn timestamps_reject_leap_seconds_before_encoding_copy() -> Result<()> {
        for precision in 0..=6 {
            for (kind, offset) in [
                (PostgresType::Timestamp(precision), ""),
                (PostgresType::TimestampTz(precision), "Z"),
                (PostgresType::TimestampTz(precision), "+08:00"),
            ] {
                let column = ColumnSchema::new("time", kind)?;
                for seconds in ["60", "60.5", "60.123456"] {
                    let mut row = Vec::new();
                    let text = format!("2016-12-31T23:59:{seconds}{offset}");
                    assert!(
                        push_field(
                            &mut row,
                            Some(FieldValue::StringValue(text.clone())),
                            &column,
                            1024,
                        )
                        .is_err(),
                        "{text}, precision={precision}",
                    );
                    assert!(row.is_empty());
                }
                let fraction = if precision == 0 {
                    String::new()
                } else {
                    format!(".{}", "1".repeat(usize::from(precision)))
                };
                let text = format!("2016-12-31T23:59:59{fraction}{offset}");
                let mut row = Vec::new();
                push_field(
                    &mut row,
                    Some(FieldValue::StringValue(text.clone())),
                    &column,
                    1024,
                )?;
                assert_eq!(row.len(), 12);
            }
        }
        Ok(())
    }
}
