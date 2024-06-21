use std::io::{Error, ErrorKind};

pub fn serialize_to_bytes_rmp<T>(obj: T) -> Result<Vec<u8>, Error>
    where T: serde::Serialize
{
    match rmp_serde::to_vec(&obj) {
        Ok(r) => Ok(r),
        Err(e) => Err(Error::new(ErrorKind::InvalidData, e))
    }
}

pub fn serialize_to_bytes_json<T>(obj: T) -> Result<Vec<u8>, Error>
    where T: serde::Serialize
{
    match serde_json::to_vec(&obj) {
        Ok(r) => Ok(r),
        Err(e) => Err(Error::new(ErrorKind::InvalidData, e))
    }
}

pub fn deserialize_rmp_to<'a, T: serde::Deserialize<'a>>(read: Vec<u8>) -> Result<T, Error> {
    //     let slice = read.as_slice();
    //     match rmp_serde::from_read::<[u8], T>(*slice) {
    //         Ok(r) => Ok(r),
    //         Err(e) => Err(Error::new(ErrorKind::InvalidData, e))
    //     }
    todo!()
}

pub fn deserialize_json_to<'a, T>(read: &'a Vec<u8>) -> Result<T, Error>
    where T: serde::Deserialize<'a>
{
    match serde_json::from_slice::<'a, T>(&*read) {
        Ok(r) => Ok(r),
        Err(e) => Err(Error::new(ErrorKind::InvalidData, e))
    }
}