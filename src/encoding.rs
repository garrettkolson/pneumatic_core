use std::io::{Error, ErrorKind};

/// Serialize to MsgPack bytes.
///
/// Structs are encoded as **named maps** (`to_vec_named`) rather than the
/// default positional arrays. The default `to_vec` writes structs as bare
/// tuples, which is incompatible with `#[serde(skip_serializing_if)]`: a
/// skipped field removes an array slot and shifts every later field, so the
/// positional deserializer reads a value of the wrong type (e.g. a `u64`
/// field landing on a `Vec<u8>` array) and fails with `TypeMismatch`. Named
/// maps identify fields by name, so skipped fields are safe. The rmp-serde
/// deserializer accepts both maps and arrays, so this is backward-compatible
/// on read.
pub fn serialize_to_bytes_rmp<T>(obj: &T) -> Result<Vec<u8>, Error>
    where T: serde::Serialize
{
    match rmp_serde::to_vec_named(obj) {
        Ok(r) => Ok(r),
        Err(e) => Err(Error::new(ErrorKind::InvalidData, e))
    }
}

pub fn serialize_to_bytes_json<T>(obj: &T) -> Result<Vec<u8>, Error>
    where T: serde::Serialize
{
    match serde_json::to_vec(obj) {
        Ok(r) => Ok(r),
        Err(e) => Err(Error::new(ErrorKind::InvalidData, e))
    }
}

pub fn deserialize_rmp_to<'a, T: serde::Deserialize<'a>>(read: &'a Vec<u8>) -> Result<T, Error> {
    match rmp_serde::from_slice::<'a, T>(read) {
        Ok(r) => Ok(r),
        Err(e) => Err(Error::new(ErrorKind::InvalidData, e))
    }
}

pub fn deserialize_json_to<'a, T>(read: &'a Vec<u8>) -> Result<T, Error>
    where T: serde::Deserialize<'a>
{
    match serde_json::from_slice::<'a, T>(read) {
        Ok(r) => Ok(r),
        Err(e) => Err(Error::new(ErrorKind::InvalidData, e))
    }
}
