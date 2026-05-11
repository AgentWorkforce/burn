//! Stable JSON serialization + sha256 hashing helpers — Rust port of
//! `packages/reader/src/hash.ts`.
//!
//! The TS helper takes any value (`unknown`) and reduces it to a canonical
//! string before hashing. The Rust API mirrors that flexibility by being
//! generic over `Serialize`: any record type or `serde_json::Value` works
//! without callers having to convert first. The 16-character truncation on
//! [`args_hash`] / [`content_hash`] mirrors the TS slice so detector output
//! stays visually consistent.

use std::fmt;

use serde::ser::{
    self, Serialize, SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant,
    SerializeTuple, SerializeTupleStruct, SerializeTupleVariant, Serializer,
};
use sha2::{Digest, Sha256};

/// Stable JSON stringification: object keys are sorted, arrays keep order,
/// primitives serialize the way `serde_json` does. Mirrors the TS
/// `stableStringify` so hash inputs are byte-identical across the two ports.
pub fn stable_stringify<T: Serialize + ?Sized>(value: &T) -> String {
    let mut out = String::new();
    let _ = value.serialize(StableSerializer { out: &mut out });
    out
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
    hex::encode(&hasher.finalize()[..8])
}

#[derive(Debug)]
struct StableError(String);

impl fmt::Display for StableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StableError {}

impl ser::Error for StableError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self(msg.to_string())
    }
}

/// Custom serializer that writes JSON directly into a `String` buffer with
/// sorted object keys. Avoids materializing an intermediate `serde_json::Value`
/// tree for every hash input. Primitive formatting (numbers, escaped strings)
/// is delegated to `serde_json` so the output is byte-identical to a Value
/// roundtrip.
struct StableSerializer<'a> {
    out: &'a mut String,
}

fn write_primitive<T: Serialize + ?Sized>(out: &mut String, value: &T) -> Result<(), StableError> {
    let s = serde_json::to_string(value).map_err(|e| StableError(e.to_string()))?;
    out.push_str(&s);
    Ok(())
}

impl<'a> Serializer for StableSerializer<'a> {
    type Ok = ();
    type Error = StableError;
    type SerializeSeq = StableSeq<'a>;
    type SerializeTuple = StableSeq<'a>;
    type SerializeTupleStruct = StableSeq<'a>;
    type SerializeTupleVariant = StableTupleVariant<'a>;
    type SerializeMap = StableMap<'a>;
    type SerializeStruct = StableMap<'a>;
    type SerializeStructVariant = StableStructVariant<'a>;

    fn serialize_bool(self, v: bool) -> Result<(), StableError> {
        self.out.push_str(if v { "true" } else { "false" });
        Ok(())
    }
    fn serialize_i8(self, v: i8) -> Result<(), StableError> {
        write_primitive(self.out, &v)
    }
    fn serialize_i16(self, v: i16) -> Result<(), StableError> {
        write_primitive(self.out, &v)
    }
    fn serialize_i32(self, v: i32) -> Result<(), StableError> {
        write_primitive(self.out, &v)
    }
    fn serialize_i64(self, v: i64) -> Result<(), StableError> {
        write_primitive(self.out, &v)
    }
    fn serialize_i128(self, v: i128) -> Result<(), StableError> {
        write_primitive(self.out, &v)
    }
    fn serialize_u8(self, v: u8) -> Result<(), StableError> {
        write_primitive(self.out, &v)
    }
    fn serialize_u16(self, v: u16) -> Result<(), StableError> {
        write_primitive(self.out, &v)
    }
    fn serialize_u32(self, v: u32) -> Result<(), StableError> {
        write_primitive(self.out, &v)
    }
    fn serialize_u64(self, v: u64) -> Result<(), StableError> {
        write_primitive(self.out, &v)
    }
    fn serialize_u128(self, v: u128) -> Result<(), StableError> {
        write_primitive(self.out, &v)
    }
    fn serialize_f32(self, v: f32) -> Result<(), StableError> {
        write_primitive(self.out, &v)
    }
    fn serialize_f64(self, v: f64) -> Result<(), StableError> {
        write_primitive(self.out, &v)
    }
    fn serialize_char(self, v: char) -> Result<(), StableError> {
        write_primitive(self.out, &v)
    }
    fn serialize_str(self, v: &str) -> Result<(), StableError> {
        write_primitive(self.out, v)
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<(), StableError> {
        write_primitive(self.out, v)
    }
    fn serialize_none(self) -> Result<(), StableError> {
        self.out.push_str("null");
        Ok(())
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<(), StableError> {
        v.serialize(self)
    }
    fn serialize_unit(self) -> Result<(), StableError> {
        self.out.push_str("null");
        Ok(())
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<(), StableError> {
        self.out.push_str("null");
        Ok(())
    }
    fn serialize_unit_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
    ) -> Result<(), StableError> {
        write_primitive(self.out, variant)
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        v: &T,
    ) -> Result<(), StableError> {
        v.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        v: &T,
    ) -> Result<(), StableError> {
        self.out.push('{');
        write_primitive(self.out, variant)?;
        self.out.push(':');
        v.serialize(StableSerializer { out: self.out })?;
        self.out.push('}');
        Ok(())
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<StableSeq<'a>, StableError> {
        self.out.push('[');
        Ok(StableSeq {
            out: self.out,
            first: true,
        })
    }
    fn serialize_tuple(self, len: usize) -> Result<StableSeq<'a>, StableError> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_struct(
        self,
        _: &'static str,
        len: usize,
    ) -> Result<StableSeq<'a>, StableError> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        _: usize,
    ) -> Result<StableTupleVariant<'a>, StableError> {
        self.out.push('{');
        write_primitive(self.out, variant)?;
        self.out.push(':');
        self.out.push('[');
        Ok(StableTupleVariant {
            out: self.out,
            first: true,
        })
    }
    fn serialize_map(self, _: Option<usize>) -> Result<StableMap<'a>, StableError> {
        Ok(StableMap {
            out: self.out,
            entries: Vec::new(),
            current_key: None,
        })
    }
    fn serialize_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<StableMap<'a>, StableError> {
        Ok(StableMap {
            out: self.out,
            entries: Vec::new(),
            current_key: None,
        })
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        _: usize,
    ) -> Result<StableStructVariant<'a>, StableError> {
        self.out.push('{');
        write_primitive(self.out, variant)?;
        self.out.push(':');
        Ok(StableStructVariant {
            out: self.out,
            entries: Vec::new(),
        })
    }
}

struct StableSeq<'a> {
    out: &'a mut String,
    first: bool,
}

impl<'a> SerializeSeq for StableSeq<'a> {
    type Ok = ();
    type Error = StableError;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), StableError> {
        if !self.first {
            self.out.push(',');
        }
        self.first = false;
        v.serialize(StableSerializer { out: self.out })
    }
    fn end(self) -> Result<(), StableError> {
        self.out.push(']');
        Ok(())
    }
}

impl<'a> SerializeTuple for StableSeq<'a> {
    type Ok = ();
    type Error = StableError;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), StableError> {
        SerializeSeq::serialize_element(self, v)
    }
    fn end(self) -> Result<(), StableError> {
        SerializeSeq::end(self)
    }
}

impl<'a> SerializeTupleStruct for StableSeq<'a> {
    type Ok = ();
    type Error = StableError;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), StableError> {
        SerializeSeq::serialize_element(self, v)
    }
    fn end(self) -> Result<(), StableError> {
        SerializeSeq::end(self)
    }
}

struct StableTupleVariant<'a> {
    out: &'a mut String,
    first: bool,
}

impl<'a> SerializeTupleVariant for StableTupleVariant<'a> {
    type Ok = ();
    type Error = StableError;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), StableError> {
        if !self.first {
            self.out.push(',');
        }
        self.first = false;
        v.serialize(StableSerializer { out: self.out })
    }
    fn end(self) -> Result<(), StableError> {
        self.out.push_str("]}");
        Ok(())
    }
}

struct StableMap<'a> {
    out: &'a mut String,
    entries: Vec<(String, String)>,
    current_key: Option<String>,
}

fn finalize_object(out: &mut String, entries: &mut [(String, String)]) {
    // Sort by the raw (unescaped) key so the order matches serde_json's
    // BTreeMap-backed Value path. Sorting by JSON-encoded keys diverges for
    // keys containing characters that JSON escapes (control chars, quotes).
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    out.push('{');
    for (i, (k, v)) in entries.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        // JSON-encode the key at write time. Using `to_string` here matches
        // `serde_json::to_string` for string values — i.e. the exact same
        // escaping the previous `Value::String(s) => to_string(s)` path used.
        let encoded = serde_json::to_string(k).unwrap_or_else(|_| String::from("\"\""));
        out.push_str(&encoded);
        out.push(':');
        out.push_str(v);
    }
    out.push('}');
}

impl<'a> SerializeMap for StableMap<'a> {
    type Ok = ();
    type Error = StableError;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), StableError> {
        // JSON requires object keys to be strings. Mirror `serde_json`'s
        // `MapKeySerializer`: accept &str, coerce primitive numeric/bool keys
        // to their string form, reject anything else.
        let raw = key.serialize(MapKeyCollector)?;
        self.current_key = Some(raw);
        Ok(())
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), StableError> {
        let mut buf = String::new();
        value.serialize(StableSerializer { out: &mut buf })?;
        let k = self
            .current_key
            .take()
            .ok_or_else(|| StableError("value before key".into()))?;
        self.entries.push((k, buf));
        Ok(())
    }
    fn end(mut self) -> Result<(), StableError> {
        finalize_object(self.out, &mut self.entries);
        Ok(())
    }
}

impl<'a> SerializeStruct for StableMap<'a> {
    type Ok = ();
    type Error = StableError;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), StableError> {
        let mut vbuf = String::new();
        value.serialize(StableSerializer { out: &mut vbuf })?;
        // Store the raw key; `finalize_object` JSON-encodes it. Lets us sort
        // by raw value to match the previous Value-roundtrip path.
        self.entries.push((key.to_string(), vbuf));
        Ok(())
    }
    fn end(mut self) -> Result<(), StableError> {
        finalize_object(self.out, &mut self.entries);
        Ok(())
    }
}

struct StableStructVariant<'a> {
    out: &'a mut String,
    entries: Vec<(String, String)>,
}

impl<'a> SerializeStructVariant for StableStructVariant<'a> {
    type Ok = ();
    type Error = StableError;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), StableError> {
        let mut vbuf = String::new();
        value.serialize(StableSerializer { out: &mut vbuf })?;
        self.entries.push((key.to_string(), vbuf));
        Ok(())
    }
    fn end(mut self) -> Result<(), StableError> {
        finalize_object(self.out, &mut self.entries);
        self.out.push('}');
        Ok(())
    }
}

/// Serializer for map keys. JSON requires object keys to be strings, so this
/// mirrors `serde_json`'s `MapKeySerializer`: accept strings as-is, coerce
/// primitive numeric/bool keys to their string form, error on composite types.
/// Returns the raw key string (without JSON quoting/escaping) so callers can
/// sort by raw value and encode at write time.
struct MapKeyCollector;

impl Serializer for MapKeyCollector {
    type Ok = String;
    type Error = StableError;
    type SerializeSeq = ser::Impossible<String, StableError>;
    type SerializeTuple = ser::Impossible<String, StableError>;
    type SerializeTupleStruct = ser::Impossible<String, StableError>;
    type SerializeTupleVariant = ser::Impossible<String, StableError>;
    type SerializeMap = ser::Impossible<String, StableError>;
    type SerializeStruct = ser::Impossible<String, StableError>;
    type SerializeStructVariant = ser::Impossible<String, StableError>;

    fn serialize_str(self, v: &str) -> Result<String, StableError> {
        Ok(v.to_string())
    }
    fn serialize_char(self, v: char) -> Result<String, StableError> {
        Ok(v.to_string())
    }
    fn serialize_bool(self, v: bool) -> Result<String, StableError> {
        Ok(if v { "true".into() } else { "false".into() })
    }
    fn serialize_i8(self, v: i8) -> Result<String, StableError> {
        Ok(v.to_string())
    }
    fn serialize_i16(self, v: i16) -> Result<String, StableError> {
        Ok(v.to_string())
    }
    fn serialize_i32(self, v: i32) -> Result<String, StableError> {
        Ok(v.to_string())
    }
    fn serialize_i64(self, v: i64) -> Result<String, StableError> {
        Ok(v.to_string())
    }
    fn serialize_i128(self, v: i128) -> Result<String, StableError> {
        Ok(v.to_string())
    }
    fn serialize_u8(self, v: u8) -> Result<String, StableError> {
        Ok(v.to_string())
    }
    fn serialize_u16(self, v: u16) -> Result<String, StableError> {
        Ok(v.to_string())
    }
    fn serialize_u32(self, v: u32) -> Result<String, StableError> {
        Ok(v.to_string())
    }
    fn serialize_u64(self, v: u64) -> Result<String, StableError> {
        Ok(v.to_string())
    }
    fn serialize_u128(self, v: u128) -> Result<String, StableError> {
        Ok(v.to_string())
    }
    fn serialize_f32(self, _: f32) -> Result<String, StableError> {
        Err(StableError("float map key not supported".into()))
    }
    fn serialize_f64(self, _: f64) -> Result<String, StableError> {
        Err(StableError("float map key not supported".into()))
    }
    fn serialize_bytes(self, _: &[u8]) -> Result<String, StableError> {
        Err(StableError("bytes map key not supported".into()))
    }
    fn serialize_none(self) -> Result<String, StableError> {
        Err(StableError("map key must be a string".into()))
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<String, StableError> {
        v.serialize(self)
    }
    fn serialize_unit(self) -> Result<String, StableError> {
        Err(StableError("map key must be a string".into()))
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<String, StableError> {
        Err(StableError("map key must be a string".into()))
    }
    fn serialize_unit_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
    ) -> Result<String, StableError> {
        Ok(variant.to_string())
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        v: &T,
    ) -> Result<String, StableError> {
        v.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<String, StableError> {
        Err(StableError("map key must be a string".into()))
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, StableError> {
        Err(StableError("map key must be a string".into()))
    }
    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, StableError> {
        Err(StableError("map key must be a string".into()))
    }
    fn serialize_tuple_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleStruct, StableError> {
        Err(StableError("map key must be a string".into()))
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant, StableError> {
        Err(StableError("map key must be a string".into()))
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, StableError> {
        Err(StableError("map key must be a string".into()))
    }
    fn serialize_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStruct, StableError> {
        Err(StableError("map key must be a string".into()))
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant, StableError> {
        Err(StableError("map key must be a string".into()))
    }
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
    fn stable_stringify_sorts_struct_fields() {
        #[derive(serde::Serialize)]
        struct Args {
            zebra: u32,
            apple: u32,
        }
        let s = stable_stringify(&Args {
            zebra: 1,
            apple: 2,
        });
        assert_eq!(s, r#"{"apple":2,"zebra":1}"#);
    }

    #[test]
    fn stable_stringify_sorts_keys_by_raw_value_not_json_encoded() {
        // `!` is raw 0x21; `\n` is raw 0x0a. Raw sort: \n < !. JSON-encoded
        // sort (after the surrounding quote): `\\` (0x5c) > `!` (0x21), so
        // encoded order would be !, \n — the opposite. The previous
        // Value-roundtrip impl sorted by raw value; assert we still do.
        let v = json!({ "!": 1, "\n": 2 });
        assert_eq!(stable_stringify(&v), "{\"\\n\":2,\"!\":1}");
    }

    #[test]
    fn stable_stringify_coerces_numeric_map_keys_to_strings() {
        use std::collections::BTreeMap;
        let mut m: BTreeMap<u32, u32> = BTreeMap::new();
        m.insert(10, 1);
        m.insert(2, 2);
        // Numeric keys get coerced to their string form, like `serde_json`.
        // Sorted by raw string value, so "10" < "2" lexicographically.
        assert_eq!(stable_stringify(&m), r#"{"10":1,"2":2}"#);
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
