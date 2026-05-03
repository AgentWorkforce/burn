//! Stable JSON serialization + sha256 hashing helpers — Rust port of
//! `packages/reader/src/hash.ts`.
//!
//! `stable_stringify` matches `JSON.stringify` byte-for-byte under the same
//! sorted-keys policy the TS helper uses: arrays preserve order, object keys
//! are sorted lexicographically, primitives serialize the same way `serde_json`
//! emits them. The 16-character truncation on `args_hash` / `content_hash`
//! mirrors the TS slice so detector output stays visually consistent.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Stable JSON stringification: object keys are sorted, arrays keep order,
/// primitives serialize the way `serde_json` does. Mirrors the TS
/// `stableStringify` so hash inputs are identical across the two ports.
pub fn stable_stringify(value: &Value) -> String {
    let mut out = String::new();
    write_stable(value, &mut out);
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

/// Short hash of an arbitrary JSON-serializable value. Mirrors TS `argsHash`
/// (sha256 over `stable_stringify`, hex-encoded, sliced to 16 chars).
pub fn args_hash(input: &Value) -> String {
    let canonical = stable_stringify(input);
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let digest = hasher.finalize();
    let mut hex = hex::encode(digest);
    hex.truncate(16);
    hex
}

/// Short hash of a raw string (Edit pre/post, Write content). Same 16-char
/// truncation as `args_hash`.
pub fn content_hash(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let digest = hasher.finalize();
    let mut hex = hex::encode(digest);
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
        let v = json!([3, 1, 2]);
        assert_eq!(stable_stringify(&v), "[3,1,2]");
    }

    #[test]
    fn args_hash_is_16_chars_hex() {
        let h = args_hash(&json!({ "command": "ls", "cwd": "/tmp" }));
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn args_hash_is_stable_under_key_reordering() {
        let a = args_hash(&json!({ "a": 1, "b": 2 }));
        let b = args_hash(&json!({ "b": 2, "a": 1 }));
        assert_eq!(a, b);
    }

    #[test]
    fn content_hash_of_empty_string_is_well_known() {
        // sha256("") = e3b0c44298fc1c14...; first 16 chars must match.
        assert_eq!(content_hash(""), "e3b0c44298fc1c14");
    }
}
