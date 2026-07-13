//! OID-keyed JSON value mapping (ADR-0019).
//!
//! This is the ONE place a Postgres column's TEXT-mode wire representation
//! turns into a JSON value. Both [`super::pg::tuple_to_json_payload`]
//! (streaming) and [`super::snapshot::build_json_payload`] (initial snapshot)
//! call [`append_typed_value`] — never their own ad hoc formatting — so a row
//! renders byte-identically no matter which path produced it.
//!
//! ## Contract
//!
//! - `cell = None` is SQL NULL. It ALWAYS renders as the JSON literal `null`,
//!   regardless of the column's type — a null `bool` is `null`, never `false`;
//!   inventing a value would be a lie the client can't detect.
//! - `cell = Some(text)` is the value exactly as Postgres's TEXT-mode wire
//!   protocol (pgoutput) or `COPY ... TO STDOUT` sends it — the same text in
//!   both cases (both go through the server's output functions), which is
//!   what makes one mapping function correct for both callers.
//! - A value that claims a numeric/bool/timestamp OID but fails to parse
//!   (corrupt text, an unsupported `DateStyle`, a non-hex `bytea`, ...) falls
//!   back to quoted-string passthrough. We never drop a row or panic on a
//!   parse failure — the ceiling is "less precise than expected", not "sync
//!   stops".
//! - Enum / array / domain / unrecognized OIDs pass through as a JSON string
//!   (today's behavior, unchanged).
//!
//! ponytail: arrays and typed-decode of domains/enums beyond string
//! passthrough are deferred. Upgrade path: a relation-metadata wire frame so
//! the client can materialize enum labels / array element types without the
//! server guessing; see ADR-0019 for the fuller discussion.

use super::pg::json_escape_into;

/// Builtin OIDs we special-case. Anything else is string passthrough. See
/// Postgres's `pg_type.dat` — these are stable, standard OIDs (not
/// catalog-lookup-dependent), so hardcoding them is safe.
mod oid {
    pub(super) const BOOL: i32 = 16;
    pub(super) const BYTEA: i32 = 17;
    pub(super) const INT8: i32 = 20;
    pub(super) const INT2: i32 = 21;
    pub(super) const INT4: i32 = 23;
    pub(super) const OID: i32 = 26;
    pub(super) const JSON: i32 = 114;
    pub(super) const FLOAT4: i32 = 700;
    pub(super) const FLOAT8: i32 = 701;
    pub(super) const MONEY: i32 = 790;
    pub(super) const DATE: i32 = 1082;
    pub(super) const TIME: i32 = 1083;
    pub(super) const TIMESTAMP: i32 = 1114;
    pub(super) const TIMESTAMPTZ: i32 = 1184;
    pub(super) const TIMETZ: i32 = 1266;
    pub(super) const NUMERIC: i32 = 1700;
    pub(super) const UUID: i32 = 2950;
    pub(super) const JSONB: i32 = 3802;
}

/// Append one column's value as JSON to `out`, driven by its Postgres type
/// OID. See the module docs for the full contract.
pub(crate) fn append_typed_value(out: &mut String, type_oid: i32, cell: Option<&str>) {
    let Some(text) = cell else {
        out.push_str("null");
        return;
    };
    // The int8/oid/numeric/money/date/time/uuid/json(b) arm and the trailing
    // wildcard both call `append_quoted` — deliberately: the explicit arm is
    // the OID-mapping CONTRACT table (ADR-0019), documented per-type even
    // though several types share the same string-passthrough behavior;
    // folding it into the wildcard would hide which OIDs are a considered
    // decision (int8-as-string, money's PG-formatted text, json(b)'s
    // Debezium-style string-wrapping) versus which fall through by default
    // (enums/arrays/domains/anything unrecognized).
    #[allow(clippy::match_same_arms)]
    match type_oid {
        oid::BOOL => append_bool(out, text),
        oid::INT2 | oid::INT4 => append_bare_if(out, text, |s| s.parse::<i64>().is_ok()),
        oid::FLOAT4 | oid::FLOAT8 => append_float(out, text),
        oid::TIMESTAMP => append_quoted(out, normalize_timestamp(text).as_deref().unwrap_or(text)),
        oid::TIMESTAMPTZ => {
            append_quoted(out, normalize_timestamptz(text).as_deref().unwrap_or(text));
        }
        oid::TIMETZ => append_quoted(out, normalize_timetz(text).as_deref().unwrap_or(text)),
        oid::BYTEA => append_quoted(
            out,
            &bytea_hex_to_base64(text).unwrap_or_else(|| text.to_string()),
        ),
        // int8/oid/numeric/money → string (ADR-0019: the int8-as-string
        // decision). DATE/TIME carry no timezone in Postgres itself, so
        // there's nothing to normalize — pass the already-ISO text through.
        // json(b) → the serialized JSON text AS a JSON string (Debezium
        // convention), not embedded as a JSON value.
        oid::INT8
        | oid::OID
        | oid::NUMERIC
        | oid::MONEY
        | oid::DATE
        | oid::TIME
        | oid::UUID
        | oid::JSON
        | oid::JSONB => append_quoted(out, text),
        // Enums / arrays / domains / anything unrecognized: string passthrough
        // (today's behavior). ponytail: see module docs.
        _ => append_quoted(out, text),
    }
}

/// Map a Postgres column type OID to the SQLite column affinity a client
/// should use when materializing the typed table for that column. WS1's
/// `GET /schema` endpoint ships this so the Flutter SDK can auto-build typed
/// tables without a hand-written `Schema`.
///
/// The affinity mirrors the JSON token shape [`append_typed_value`] emits, so
/// the wire value stores in the client's SQLite column without coercion: types
/// rendered as a bare JSON number → `INTEGER`/`REAL`; everything nostos renders
/// as a quoted string → `TEXT`. Notably `int8`/`oid`/`numeric`/`money` map to
/// `TEXT` because ADR-0019 deliberately renders them as strings to preserve
/// precision — a typed-record layer (WS6) can parse them back to numbers.
///
/// This is the upgrade ADR-0019 names as deferred ("relation-metadata wire
/// frame so the client stops guessing") — the `/schema` endpoint + this fn.
// ponytail: affinity mirrors wire reality, not PG semantic type. A future
// typed-record layer (WS6) can surface richer Dart types (boolean, DateTime,
// int64) above a TEXT-affinity store.
pub(crate) fn oid_to_sqlite_affinity(type_oid: i32) -> &'static str {
    match type_oid {
        oid::BOOL | oid::INT2 | oid::INT4 => "INTEGER",
        oid::FLOAT4 | oid::FLOAT8 => "REAL",
        // int8/oid/numeric/money are rendered as STRINGS (ADR-0019
        // precision-preserving decision) → TEXT keeps the exact value.
        // BYTEA (base64 text), timestamps, uuid, json(b), and all unrecognized
        // OIDs (enums/arrays/domains) are likewise string-rendered → TEXT.
        _ => "TEXT",
    }
}

#[cfg(test)]
mod oid_affinity_tests {
    //! The affinity contract the `/schema` endpoint hands the client (WS1).
    use super::oid;
    use super::oid_to_sqlite_affinity;

    #[test]
    fn bare_number_types_are_integer_or_real() {
        // Rendered as a bare JSON token by `append_typed_value` → numeric affinity.
        assert_eq!(oid_to_sqlite_affinity(oid::BOOL), "INTEGER");
        assert_eq!(oid_to_sqlite_affinity(oid::INT2), "INTEGER");
        assert_eq!(oid_to_sqlite_affinity(oid::INT4), "INTEGER");
        assert_eq!(oid_to_sqlite_affinity(oid::FLOAT4), "REAL");
        assert_eq!(oid_to_sqlite_affinity(oid::FLOAT8), "REAL");
    }

    #[test]
    fn string_rendered_types_are_text() {
        // ADR-0019 renders these as quoted strings (precision / fidelity), so
        // the affinity is TEXT — an INTEGER column would coerce / lose precision.
        for &oid_val in &[
            oid::INT8,
            oid::OID,
            oid::NUMERIC,
            oid::MONEY,
            oid::DATE,
            oid::TIME,
            oid::TIMESTAMP,
            oid::TIMESTAMPTZ,
            oid::TIMETZ,
            oid::UUID,
            oid::JSON,
            oid::JSONB,
            oid::BYTEA,
        ] {
            assert_eq!(
                oid_to_sqlite_affinity(oid_val),
                "TEXT",
                "OID {oid_val} is string-rendered → TEXT affinity"
            );
        }
    }

    #[test]
    fn unrecognized_oid_is_text_and_never_panics() {
        // Enums / arrays / domains (unrecognized OIDs): string passthrough →
        // TEXT. A negative OID is nostos's u32→i32 convert-failure sentinel
        // (snapshot_source.rs:177) — also TEXT, never a panic.
        assert_eq!(oid_to_sqlite_affinity(1_000_000), "TEXT");
        assert_eq!(oid_to_sqlite_affinity(-1), "TEXT");
    }
}

fn append_quoted(out: &mut String, s: &str) {
    out.push('"');
    json_escape_into(out, s);
    out.push('"');
}

/// Push `text` verbatim as a bare JSON token if `valid(text)`, else fall back
/// to a quoted string (corrupt-text guard — never panic, never drop a row).
fn append_bare_if(out: &mut String, text: &str, valid: impl FnOnce(&str) -> bool) {
    if valid(text) {
        out.push_str(text);
    } else {
        append_quoted(out, text);
    }
}

/// Postgres's `boolout` renders `t`/`f` on BOTH the replication wire and
/// `COPY ... TO STDOUT` text format. Accept `true`/`false` too (defensive —
/// never observed from Postgres itself, but cheap to accept). Anything else
/// falls back to quoted-string passthrough.
fn append_bool(out: &mut String, text: &str) {
    match text {
        "t" | "true" => out.push_str("true"),
        "f" | "false" => out.push_str("false"),
        other => append_quoted(out, other),
    }
}

/// RFC 8259 forbids the bare tokens `NaN`/`Infinity`/`-Infinity` — Postgres's
/// float text output uses exactly those spellings for non-finite values, so
/// they must be quoted. Everything else that parses as a finite `f64` is
/// pushed as a bare JSON number token (Postgres's float text form already
/// matches JSON number grammar); parse failure falls back to quoted string.
fn append_float(out: &mut String, text: &str) {
    match text {
        "NaN" | "Infinity" | "-Infinity" => append_quoted(out, text),
        _ => match text.parse::<f64>() {
            Ok(v) if v.is_finite() => out.push_str(text),
            _ => append_quoted(out, text),
        },
    }
}

/// Decode Postgres's default `bytea_output=hex` text form (`\x` + hex digits)
/// into base64. Any other shape (e.g. the legacy `escape` format, or corrupt
/// hex) returns `None` — the caller falls back to raw-text passthrough.
fn bytea_hex_to_base64(text: &str) -> Option<String> {
    let hex = text.strip_prefix("\\x")?;
    if hex.len() % 2 != 0 {
        return None;
    }
    let hb = hex.as_bytes();
    let mut bytes = Vec::with_capacity(hb.len() / 2);
    let mut i = 0;
    while i < hb.len() {
        let hi = (hb[i] as char).to_digit(16)?;
        let lo = (hb[i + 1] as char).to_digit(16)?;
        bytes.push(u8::try_from((hi << 4) | lo).ok()?);
        i += 2;
    }
    Some(base64_encode(&bytes))
}

/// Minimal standard-alphabet base64 encode (WITH padding — the general
/// convention client libraries expect for arbitrary binary, unlike the JWT
/// base64url-no-padding helper in `auth.rs`). Hand-rolled to avoid a `base64`
/// crate dependency for this one call site, matching `auth.rs`'s precedent.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        out.push(ALPHABET[usize::from(b0 >> 2)] as char);
        out.push(ALPHABET[usize::from(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4))] as char);
        out.push(match b1 {
            Some(b1) => ALPHABET[usize::from(((b1 & 0x0f) << 2) | (b2.unwrap_or(0) >> 6))] as char,
            None => '=',
        });
        out.push(match b2 {
            Some(b2) => ALPHABET[usize::from(b2 & 0x3f)] as char,
            None => '=',
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Timestamp normalization: Postgres ISO `DateStyle` text → RFC 3339 UTC.
//
// Nostos's control-plane and replication connections never `SET DateStyle`, so
// we can rely on the server default ("ISO, MDY"):
//   timestamp:    YYYY-MM-DD HH:MM:SS[.ffffff]
//   timestamptz:  YYYY-MM-DD HH:MM:SS[.ffffff][+-]HH[:MM[:SS]]
//   timetz:                  HH:MM:SS[.ffffff][+-]HH[:MM[:SS]]
// A non-default DateStyle (or a BC date, which Postgres suffixes with " BC")
// fails to parse here and falls back to raw-text passthrough — ponytail:
// no BC / non-ISO-DateStyle support; upgrade path is `SET DateStyle` pinning
// at connect time if a design partner ever needs it.
// ---------------------------------------------------------------------------

/// `timestamp` (no tz): treated as naive UTC (Debezium's convention) —
/// reformat the space-separated PG text into `T`...`Z` form. `None` on parse
/// failure (caller falls back to raw-text passthrough).
fn normalize_timestamp(text: &str) -> Option<String> {
    let (date_part, time_part) = split_date_time(text)?;
    let (year, month, day) = parse_date(date_part)?;
    let (hour, minute, second, frac) = parse_hms(time_part)?;
    Some(format_rfc3339(
        year,
        month,
        day,
        hour,
        minute,
        second,
        frac.as_deref(),
    ))
}

/// `timestamptz`: parse the trailing UTC offset and convert to true UTC
/// (Postgres's text form always includes an offset for this type).
fn normalize_timestamptz(text: &str) -> Option<String> {
    let (date_part, rest) = split_date_time(text)?;
    let (year, month, day) = parse_date(date_part)?;
    let (time_part, offset_part) = split_time_and_offset(rest);
    let (hour, minute, second, frac) = parse_hms(time_part)?;
    let offset_secs = match offset_part {
        Some(o) => parse_offset(o)?,
        None => 0,
    };
    let days = days_from_civil(year, i64::from(month), i64::from(day));
    let total_secs =
        days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second)
            - offset_secs;
    let new_days = total_secs.div_euclid(86_400);
    let rem = total_secs.rem_euclid(86_400);
    let (new_year, new_month, new_day) = civil_from_days(new_days);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "rem is in [0, 86_400) after rem_euclid — the h/m/s decomposition below is always in u32 range"
    )]
    let (new_hour, new_minute, new_second) = (
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    );
    Some(format_rfc3339(
        new_year,
        new_month,
        new_day,
        new_hour,
        new_minute,
        new_second,
        frac.as_deref(),
    ))
}

/// `timetz`: a time-of-day with an offset but no date. Convert to UTC
/// time-of-day (wrapping mod 24h — there is no date to roll over).
fn normalize_timetz(text: &str) -> Option<String> {
    let (time_part, offset_part) = split_time_and_offset(text);
    let (hour, minute, second, frac) = parse_hms(time_part)?;
    let offset_secs = match offset_part {
        Some(o) => parse_offset(o)?,
        None => 0,
    };
    let total = i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second) - offset_secs;
    let total = total.rem_euclid(86_400);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "total is in [0, 86_400) after rem_euclid — the h/m/s decomposition below is always in u32 range"
    )]
    let (new_hour, new_minute, new_second) = (
        (total / 3600) as u32,
        ((total % 3600) / 60) as u32,
        (total % 60) as u32,
    );
    Some(format_time_z(
        new_hour,
        new_minute,
        new_second,
        frac.as_deref(),
    ))
}

/// Split `"<date> <time...>"` (space OR `T` separator) into `(date, rest)`.
fn split_date_time(text: &str) -> Option<(&str, &str)> {
    let pos = text.find([' ', 'T'])?;
    Some((&text[..pos], &text[pos + 1..]))
}

/// Split a time-plus-optional-offset tail on the first `+`/`-` (the time
/// portion itself — `HH:MM:SS[.ffffff]` — never contains either character).
fn split_time_and_offset(rest: &str) -> (&str, Option<&str>) {
    match rest.find(['+', '-']) {
        Some(pos) => (&rest[..pos], Some(&rest[pos..])),
        None => (rest, None),
    }
}

/// Parse `YYYY-MM-DD` into `(year, month, day)`. Rejects non-positive years
/// (BC dates are out of scope — see module docs) and out-of-range month/day.
fn parse_date(date_part: &str) -> Option<(i64, u32, u32)> {
    let mut parts = date_part.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || year < 1 || !(1..=12).contains(&month) || !(1..=31).contains(&day)
    {
        return None;
    }
    Some((year, month, day))
}

/// Parse `HH:MM:SS[.ffffff]` into `(hour, minute, second, fraction_digits)`.
/// `fraction_digits` is the raw text after the `.` (no leading dot), passed
/// through verbatim to preserve Postgres's own precision.
fn parse_hms(time_part: &str) -> Option<(u32, u32, u32, Option<String>)> {
    let mut split = time_part.splitn(2, '.');
    let hms = split.next()?;
    let frac = split.next().map(ToString::to_string);
    let mut parts = hms.split(':');
    let hour: u32 = parts.next()?.parse().ok()?;
    let minute: u32 = parts.next()?.parse().ok()?;
    let second: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        // second > 60 rejects garbage but allows a leap second's ":60" — PG
        // itself never emits ":60" for a plain time field; kept permissive.
        return None;
    }
    Some((hour, minute, second, frac))
}

/// Parse a UTC offset like `+05`, `-05:30`, `+05:30:00` into total seconds
/// (positive = east of UTC, matching Postgres's own sign convention).
fn parse_offset(offset_text: &str) -> Option<i64> {
    let (sign, rest) = match offset_text.strip_prefix('-') {
        Some(r) => (-1i64, r),
        None => (1i64, offset_text.strip_prefix('+')?),
    };
    let mut parts = rest.split(':');
    let hours: i64 = parts.next()?.parse().ok()?;
    let minutes: i64 = match parts.next() {
        Some(p) => p.parse().ok()?,
        None => 0,
    };
    let seconds: i64 = match parts.next() {
        Some(p) => p.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(sign * (hours * 3600 + minutes * 60 + seconds))
}

fn format_rfc3339(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    frac: Option<&str>,
) -> String {
    match frac {
        Some(f) if !f.is_empty() => {
            format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{f}Z")
        }
        _ => format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"),
    }
}

fn format_time_z(hour: u32, minute: u32, second: u32, frac: Option<&str>) -> String {
    match frac {
        Some(f) if !f.is_empty() => format!("{hour:02}:{minute:02}:{second:02}.{f}Z"),
        _ => format!("{hour:02}:{minute:02}:{second:02}Z"),
    }
}

/// Howard Hinnant's `days_from_civil`: proleptic-Gregorian civil date →
/// days-since-1970-01-01. Requires `year >= 1` (enforced by `parse_date`).
#[allow(
    clippy::cast_sign_loss,
    reason = "year_of_era/day_of_year/day_of_era are all non-negative by construction (year>=1 is enforced by parse_date); the algorithm's own invariants guarantee this, not just the specific inputs we pass"
)]
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400; // [0, 399]
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1; // [0, 365]
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year; // [0, 146096]
    era * 146_097 + day_of_era - 719_468
}

/// Howard Hinnant's `civil_from_days`: inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146_096) / 365; // [0, 399]
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // [0, 365]
    let month_prime = (5 * day_of_year + 2) / 153; // [0, 11]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "day_of_year/month_prime are non-negative and bounded by construction (day_of_era is in [0,146096], the algorithm's own invariant), so this always fits in u32"
    )]
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32; // [1, 31]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "month_prime is in [0,11] by construction, so both branches are non-negative and fit in u32"
    )]
    let month = (if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    }) as u32; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(oid: i32, cell: Option<&str>) -> String {
        let mut out = String::new();
        append_typed_value(&mut out, oid, cell);
        out
    }

    #[test]
    fn null_is_json_null_regardless_of_oid() {
        for oid in [
            oid::BOOL,
            oid::INT4,
            oid::INT8,
            oid::FLOAT8,
            oid::TIMESTAMPTZ,
            99999,
        ] {
            assert_eq!(render(oid, None), "null", "oid {oid}");
        }
    }

    #[test]
    fn bool_maps_to_json_bool() {
        assert_eq!(render(oid::BOOL, Some("t")), "true");
        assert_eq!(render(oid::BOOL, Some("f")), "false");
    }

    #[test]
    fn bool_corrupt_text_falls_back_to_string() {
        assert_eq!(render(oid::BOOL, Some("maybe")), "\"maybe\"");
    }

    #[test]
    fn int2_int4_map_to_bare_number() {
        assert_eq!(render(oid::INT2, Some("42")), "42");
        assert_eq!(render(oid::INT4, Some("-7")), "-7");
    }

    #[test]
    fn int4_corrupt_text_falls_back_to_string() {
        assert_eq!(render(oid::INT4, Some("abc")), "\"abc\"");
    }

    #[test]
    fn int8_oid_numeric_money_map_to_string() {
        // int8 beyond 2^53 — the whole point of the string decision.
        assert_eq!(
            render(oid::INT8, Some("9223372036854775807")),
            "\"9223372036854775807\""
        );
        assert_eq!(render(oid::OID, Some("12345")), "\"12345\"");
        assert_eq!(
            render(oid::NUMERIC, Some("3.14159265358979323846")),
            "\"3.14159265358979323846\""
        );
        assert_eq!(render(oid::MONEY, Some("$3,500.00")), "\"$3,500.00\"");
    }

    #[test]
    fn float_maps_to_bare_number() {
        assert_eq!(render(oid::FLOAT8, Some("2.5")), "2.5");
        assert_eq!(render(oid::FLOAT4, Some("1e+20")), "1e+20");
    }

    #[test]
    fn float_non_finite_maps_to_quoted_string_rfc8259_guard() {
        assert_eq!(render(oid::FLOAT8, Some("NaN")), "\"NaN\"");
        assert_eq!(render(oid::FLOAT8, Some("Infinity")), "\"Infinity\"");
        assert_eq!(render(oid::FLOAT8, Some("-Infinity")), "\"-Infinity\"");
    }

    #[test]
    fn uuid_json_bytea_date_time_pass_through_as_string() {
        assert_eq!(
            render(oid::UUID, Some("123e4567-e89b-12d3-a456-426614174000")),
            "\"123e4567-e89b-12d3-a456-426614174000\""
        );
        assert_eq!(render(oid::JSON, Some(r#"{"a":1}"#)), "\"{\\\"a\\\":1}\"");
        assert_eq!(render(oid::JSONB, Some("[1,2]")), "\"[1,2]\"");
        assert_eq!(render(oid::DATE, Some("2026-07-12")), "\"2026-07-12\"");
        assert_eq!(render(oid::TIME, Some("14:30:00")), "\"14:30:00\"");
    }

    #[test]
    fn unknown_oid_passes_through_as_string() {
        assert_eq!(
            render(999_999, Some("some-enum-label")),
            "\"some-enum-label\""
        );
    }

    #[test]
    fn bytea_hex_decodes_to_base64() {
        // "hi" = 0x68 0x69 → base64 "aGk="
        assert_eq!(render(oid::BYTEA, Some("\\x6869")), "\"aGk=\"");
        // Empty bytea.
        assert_eq!(render(oid::BYTEA, Some("\\x")), "\"\"");
    }

    #[test]
    fn bytea_non_hex_falls_back_to_raw_text() {
        assert_eq!(render(oid::BYTEA, Some("not-hex")), "\"not-hex\"");
    }

    #[test]
    fn timestamp_naive_treated_as_utc() {
        assert_eq!(
            normalize_timestamp("2026-07-12 14:30:00.123456"),
            Some("2026-07-12T14:30:00.123456Z".to_string())
        );
        assert_eq!(
            normalize_timestamp("2026-07-12 14:30:00"),
            Some("2026-07-12T14:30:00Z".to_string())
        );
    }

    #[test]
    fn timestamptz_zero_offset() {
        assert_eq!(
            normalize_timestamptz("1970-01-01 00:00:00+00"),
            Some("1970-01-01T00:00:00Z".to_string())
        );
    }

    #[test]
    fn timestamptz_positive_offset_same_day() {
        assert_eq!(
            normalize_timestamptz("2026-07-12 23:30:00+05:30"),
            Some("2026-07-12T18:00:00Z".to_string())
        );
    }

    #[test]
    fn timestamptz_positive_offset_rolls_back_a_day() {
        assert_eq!(
            normalize_timestamptz("2026-07-12 01:00:00+05:30"),
            Some("2026-07-11T19:30:00Z".to_string())
        );
    }

    #[test]
    fn timestamptz_negative_offset_rolls_forward() {
        assert_eq!(
            normalize_timestamptz("2026-07-12 23:00:00-05"),
            Some("2026-07-13T04:00:00Z".to_string())
        );
    }

    #[test]
    fn timestamptz_via_append_typed_value() {
        assert_eq!(
            render(oid::TIMESTAMPTZ, Some("2026-07-12 23:30:00+05:30")),
            "\"2026-07-12T18:00:00Z\""
        );
    }

    #[test]
    fn timestamptz_corrupt_text_falls_back_to_raw_passthrough() {
        assert_eq!(
            render(oid::TIMESTAMPTZ, Some("not-a-timestamp")),
            "\"not-a-timestamp\""
        );
    }

    #[test]
    fn timetz_normalizes_offset() {
        assert_eq!(
            normalize_timetz("23:30:00+05:30"),
            Some("18:00:00Z".to_string())
        );
        // Wraps past midnight.
        assert_eq!(
            normalize_timetz("01:00:00+05:30"),
            Some("19:30:00Z".to_string())
        );
    }

    #[test]
    fn civil_days_roundtrip() {
        // Unix epoch is the canonical fixed point for this algorithm.
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // Round-trip a range of dates including a leap day and a year boundary.
        let cases: [(i64, u32, u32); 5] = [
            (2026, 7, 12),
            (2024, 2, 29),
            (2000, 1, 1),
            (1999, 12, 31),
            (1, 1, 1),
        ];
        for (y, m, d) in cases {
            let days = days_from_civil(y, i64::from(m), i64::from(d));
            assert_eq!(
                civil_from_days(days),
                (y, m, d),
                "roundtrip failed for {y}-{m}-{d}"
            );
        }
    }

    #[test]
    fn base64_encode_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
