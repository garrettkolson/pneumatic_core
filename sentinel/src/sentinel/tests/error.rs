//! Error tests for the Sentinel, moved from `crate::sentinel::tests`.

use super::helpers::*;
use super::super::*;


// --- SentinelError From impls ---

#[test]
fn sentinel_error_from_io_error_wraps_as_encoding() {
    let io_err = std::io::Error::new(std::io::ErrorKind::Other, "test io");
    let err: SentinelError = io_err.into();
    match err {
        SentinelError::Encoding(_) => {}
        _ => panic!("expected Encoding"),
    }
}


#[test]
fn sentinel_error_from_data_error_wraps_as_data() {
    let data_err = DataError::DeserializationError(std::io::Error::new(
        std::io::ErrorKind::Other, "test data",
    ));
    let err: SentinelError = data_err.into();
    match err {
        SentinelError::Data(_) => {}
        _ => panic!("expected Data"),
    }
}

