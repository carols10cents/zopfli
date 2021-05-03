use std::io::{self, Read, Write};
use adler32::RollingAdler32;
use byteorder::{BigEndian, WriteBytesExt};

use deflate::{deflate, BlockType};
use Options;
use iter_read::IterRead;

pub fn zlib_compress<R, W>(options: &Options, in_data: R, insize: u64, mut out: W) -> io::Result<()>
    where R: Read, W: Write
{
    let cmf = 120;  /* CM 8, CINFO 7. See zlib spec.*/
    let flevel = 3;
    let fdict = 0;
    let mut cmfflg = 256 * cmf + fdict * 32 + flevel * 64;
    let fcheck = 31 - cmfflg % 31;
    cmfflg += fcheck;

    let mut rolling_adler = RollingAdler32::new();
    let mut read_error_kind = None;

    let in_data = IterRead::new(in_data.bytes().filter_map(|byte_result| {
        read_error_kind = byte_result.as_ref().map_or_else(
            |error| Some(error.kind()),
            |byte| {
                rolling_adler.update(*byte);
                None
            }
        );

        byte_result.ok()
    }).fuse());

    try!(out.by_ref().write_u16::<BigEndian>(cmfflg));

    try!(deflate(options, BlockType::Dynamic, in_data, insize, out.by_ref()));

    // in_data is fused and stops reading bytes after the first error, so
    // this if is evaluated as soon as an error occurs. The deflate function
    // has received EOF at this point, so the last block has been written.
    if let Some(error_kind) = read_error_kind {
        return Err(error_kind.into());
    }

    out.write_u32::<BigEndian>(rolling_adler.hash())
}
