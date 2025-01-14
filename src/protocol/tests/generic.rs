use std::fmt::Debug;
use std::io::Cursor;

use fluvio_protocol_core::{Decoder, Encoder};
use fluvio_protocol_derive::{Decoder, Encoder};

#[derive(Encoder, Decoder, Default, Debug)]
pub struct GenericRecord<R>
where
    R: Encoder + Decoder + Debug,
{
    len: i64,
    value: R,
}

#[test]
fn test_generic() {
    let record = GenericRecord {
        len: 20,
        value: 25_i64,
    };

    let mut src = vec![];
    let result = record.encode(&mut src, 0);
    assert!(result.is_ok());

    assert_eq!(src.len(), 16);

    let result2 = GenericRecord::<i64>::decode_from(&mut Cursor::new(&src), 0);
    assert!(result2.is_ok());
    let decoded_record = result2.expect("is ok");
    assert_eq!(decoded_record.len, 20);
    assert_eq!(decoded_record.value, 25);
}
