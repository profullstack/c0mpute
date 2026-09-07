//! Canonical JSON — the exact bytes a c0mpute signature covers.
//!
//! Two implementations in two languages must produce byte-identical output
//! for the same logical value, or every hash and signature on the network
//! becomes implementation-specific. RFC 8785 (JCS) is the reference, with
//! two deliberate restrictions that remove its hardest corners:
//!
//! 1. **No floating-point numbers.** JCS canonicalizes doubles via the
//!    ECMAScript `Number::toString` algorithm, which is genuinely hard to
//!    reimplement correctly. c0mpute forbids floats in signed payloads
//!    outright: money is a decimal *string* (`"0.042"`), durations and
//!    sizes are integers. [`canonicalize`] rejects a float rather than
//!    guessing.
//! 2. **ASCII object keys.** JCS sorts keys by UTF-16 code unit, which
//!    differs from UTF-8 byte order above U+FFFF. Restricting keys to
//!    ASCII makes the two orderings identical, so an implementation can
//!    sort raw bytes and still be correct.
//!
//! Both restrictions are checked, not assumed. A payload that violates one
//! fails to canonicalize instead of signing something another node will
//! hash differently.

use serde::Serialize;
use serde_json::Value;

use crate::Error;

/// The largest integer magnitude that survives a round trip through an
/// IEEE-754 double, i.e. through any JSON parser backed by JavaScript
/// numbers. Signed payloads stay inside it so a browser client and a Rust
/// node agree on the value.
pub const MAX_SAFE_INT: u64 = (1u64 << 53) - 1;

/// Serialize `value` to canonical JSON bytes.
///
/// Returns [`Error::NonCanonical`] if the value contains a float, an
/// out-of-range integer, or a non-ASCII object key.
pub fn canonicalize<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    let json = serde_json::to_value(value)?;
    let mut out = Vec::new();
    write_value(&json, &mut out)?;
    Ok(out)
}

/// Convenience wrapper around [`canonicalize`] for callers that want a
/// `String` (canonical JSON is always valid UTF-8).
pub fn canonicalize_to_string<T: Serialize>(value: &T) -> Result<String, Error> {
    let bytes = canonicalize(value)?;
    // write_value only ever emits UTF-8.
    Ok(String::from_utf8(bytes).expect("canonical JSON is UTF-8"))
}

fn write_value(v: &Value, out: &mut Vec<u8>) -> Result<(), Error> {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(n) => write_number(n, out)?,
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            // Sort explicitly rather than relying on serde_json's map type:
            // the `preserve_order` feature can be switched on anywhere in the
            // dependency graph and would silently give us insertion order.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push(b'{');
            for (i, key) in keys.iter().enumerate() {
                if !key.is_ascii() {
                    return Err(Error::NonCanonical(format!(
                        "object key {key:?} is not ASCII; canonical JSON keys must be ASCII"
                    )));
                }
                if i > 0 {
                    out.push(b',');
                }
                write_string(key, out);
                out.push(b':');
                write_value(&map[*key], out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

fn write_number(n: &serde_json::Number, out: &mut Vec<u8>) -> Result<(), Error> {
    if let Some(u) = n.as_u64() {
        if u > MAX_SAFE_INT {
            return Err(Error::NonCanonical(format!(
                "integer {u} exceeds the safe-integer range; encode large values as strings"
            )));
        }
        out.extend_from_slice(u.to_string().as_bytes());
        return Ok(());
    }
    if let Some(i) = n.as_i64() {
        if i.unsigned_abs() > MAX_SAFE_INT {
            return Err(Error::NonCanonical(format!(
                "integer {i} exceeds the safe-integer range; encode large values as strings"
            )));
        }
        out.extend_from_slice(i.to_string().as_bytes());
        return Ok(());
    }
    Err(Error::NonCanonical(format!(
        "floating-point number {n} is not allowed in a signed payload; \
         use a decimal string for money and an integer for counts"
    )))
}

fn write_string(s: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for c in s.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\u{08}' => out.extend_from_slice(b"\\b"),
            '\u{0c}' => out.extend_from_slice(b"\\f"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            c if (c as u32) < 0x20 => {
                out.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes());
            }
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn object_keys_are_sorted_regardless_of_input_order() {
        let a = canonicalize_to_string(&json!({"b": 1, "a": 2, "c": 3})).unwrap();
        let b = canonicalize_to_string(&json!({"c": 3, "a": 2, "b": 1})).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, r#"{"a":2,"b":1,"c":3}"#);
    }

    #[test]
    fn nested_objects_are_sorted_too() {
        let s = canonicalize_to_string(&json!({
            "z": {"second": 2, "first": 1},
            "a": [{"y": 1, "x": 2}]
        }))
        .unwrap();
        assert_eq!(s, r#"{"a":[{"x":2,"y":1}],"z":{"first":1,"second":2}}"#);
    }

    #[test]
    fn array_order_is_preserved() {
        let s = canonicalize_to_string(&json!([3, 1, 2])).unwrap();
        assert_eq!(s, "[3,1,2]");
    }

    #[test]
    fn no_insignificant_whitespace() {
        let s = canonicalize_to_string(&json!({"a": [1, 2], "b": {"c": null}})).unwrap();
        assert!(!s.contains(' '));
        assert_eq!(s, r#"{"a":[1,2],"b":{"c":null}}"#);
    }

    #[test]
    fn floats_are_rejected() {
        let err = canonicalize(&json!({"price": 0.042})).unwrap_err();
        assert!(matches!(err, Error::NonCanonical(_)), "got {err:?}");
    }

    #[test]
    fn float_rejection_survives_nesting() {
        let err = canonicalize(&json!({"a": {"b": [1, 2.5]}})).unwrap_err();
        assert!(matches!(err, Error::NonCanonical(_)));
    }

    #[test]
    fn unsafe_integers_are_rejected() {
        let err = canonicalize(&json!({"n": MAX_SAFE_INT + 1})).unwrap_err();
        assert!(matches!(err, Error::NonCanonical(_)));
        // The boundary itself is fine.
        canonicalize(&json!({"n": MAX_SAFE_INT})).unwrap();
    }

    #[test]
    fn non_ascii_keys_are_rejected() {
        let err = canonicalize(&json!({"café": 1})).unwrap_err();
        assert!(matches!(err, Error::NonCanonical(_)));
    }

    #[test]
    fn non_ascii_string_values_are_allowed_verbatim() {
        // Values may be any UTF-8; only keys are restricted.
        let s = canonicalize_to_string(&json!({"note": "café ☕"})).unwrap();
        assert_eq!(s, "{\"note\":\"café ☕\"}");
    }

    #[test]
    fn control_characters_use_lowercase_hex_escapes() {
        let s = canonicalize_to_string(&json!({"a": "\u{1}\u{1f}"})).unwrap();
        assert_eq!(s, "{\"a\":\"\\u0001\\u001f\"}");
    }

    #[test]
    fn short_escapes_are_preferred_over_hex() {
        let s = canonicalize_to_string(&json!({"a": "\n\t\r\"\\"})).unwrap();
        assert_eq!(s, r#"{"a":"\n\t\r\"\\"}"#);
    }
}
