//! Stable JSON serialization + sha256 hashing helpers — Rust port of
//! `packages/reader/src/hash.ts`.
//!
//! The TS helper takes any value (`unknown`) and reduces it to a canonical
//! string before hashing. The Rust API mirrors that flexibility by being
//! generic over `Serialize`: any record type or `serde_json::Value` works
//! without callers having to convert first. The 16-character truncation on
//! [`args_hash`] / [`content_hash`] mirrors the TS slice so detector output
//! stays visually consistent.

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Stable JSON stringification: object keys are sorted, arrays keep order,
/// primitives serialize the way `serde_json` does. Mirrors the TS
/// `stableStringify` so hash inputs are byte-identical across the two ports.
pub fn stable_stringify<T: Serialize + ?Sized>(value: &T) -> String {
    let v = serde_json::to_value(value).unwrap_or(Value::Null);
    let mut out = String::new();
    write_stable(&v, &mut out);
    out
}

fn write_stable(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => out.push_str(&serde_json::to_string(s).unwrap()),
        Value::Array(arr) => {
            out.push('[');
            for (i, v) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_stable(v, out);
            }
            out.push(']');
        }
        Value::Object(obj) => {
            let mut keys: Vec<&String> = obj.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap());
                out.push(':');
                write_stable(&obj[*k], out);
            }
            out.push('}');
        }
    }
}

/// Short hash of any serializable value. Mirrors TS `argsHash`: sha256 over
/// [`stable_stringify`], hex-encoded, truncated to 16 chars.
pub fn args_hash<T: Serialize + ?Sized>(input: &T) -> String {
    short_sha256(stable_stringify(input).as_bytes())
}

/// Short hash of a raw byte sequence (Edit pre/post, Write content). Same
/// 16-char truncation as [`args_hash`].
pub fn content_hash(s: impl AsRef<[u8]>) -> String {
    short_sha256(s.as_ref())
}

fn short_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let mut hex = hex::encode(hasher.finalize());
    hex.truncate(16);
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stable_stringify_sorts_object_keys() {
        let v = json!({ "b": 1, "a": 2, "c": [3, { "y": 1, "x": 2 }] });
        assert_eq!(
            stable_stringify(&v),
            r#"{"a":2,"b":1,"c":[3,{"x":2,"y":1}]}"#
        );
    }

    #[test]
    fn stable_stringify_handles_primitives() {
        assert_eq!(stable_stringify(&json!(null)), "null");
        assert_eq!(stable_stringify(&json!(true)), "true");
        assert_eq!(stable_stringify(&json!(42)), "42");
        assert_eq!(stable_stringify(&json!("hello")), r#""hello""#);
    }

    #[test]
    fn stable_stringify_preserves_array_order() {
        assert_eq!(stable_stringify(&json!([3, 1, 2])), "[3,1,2]");
    }

    #[test]
    fn stable_stringify_accepts_typed_records() {
        // Generic over `Serialize`: typed records work without an explicit
        // `to_value` round-trip.
        #[derive(serde::Serialize)]
        struct Args {
            a: u32,
            b: &'static str,
        }
        let s = stable_stringify(&Args { a: 1, b: "two" });
        assert_eq!(s, r#"{"a":1,"b":"two"}"#);
    }

    #[test]
    fn args_hash_is_16_chars_hex() {
        let h = args_hash(&json!({ "command": "ls", "cwd": "/tmp" }));
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn args_hash_is_stable_under_key_reordering() {
        assert_eq!(
            args_hash(&json!({ "a": 1, "b": 2 })),
            args_hash(&json!({ "b": 2, "a": 1 })),
        );
    }

    #[test]
    fn content_hash_of_empty_string_is_well_known() {
        assert_eq!(content_hash(""), "e3b0c44298fc1c14");
        assert_eq!(content_hash(b"" as &[u8]), "e3b0c44298fc1c14");
    }
}
