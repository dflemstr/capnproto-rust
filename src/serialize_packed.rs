// Copyright (c) 2013-2015 Sandstorm Development Group, Inc. and contributors
// Licensed under the MIT License:
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

use std::{io, mem, ptr, slice};
use std::io::{Read, BufRead, Write};

use serialize;
use Result;
use message::*;
use util::read_exact;

struct PackedRead<R> where R: BufRead {
    inner: R,
}

impl <R> PackedRead<R> where R: BufRead {

    fn get_read_buffer(&mut self) -> io::Result<(*const u8, *const u8)> {
        let buf = try!(self.inner.fill_buf());
        unsafe {
            Ok((buf.as_ptr(), buf.get_unchecked(buf.len())))
        }
    }
}

#[inline]
fn ptr_sub<T>(p1: *const T, p2: *const T) -> usize {
    return (p1 as usize - p2 as usize) / mem::size_of::<T>();
}

macro_rules! refresh_buffer(
    ($this:expr, $size:ident, $in_ptr:ident, $in_end:ident, $out:ident,
     $outBuf:ident, $buffer_begin:ident) => (
        {
            $this.inner.consume($size);
            let (b, e) = try!($this.get_read_buffer());
            $in_ptr = b;
            $in_end = e;
            $size = ptr_sub($in_end, $in_ptr);
            $buffer_begin = b;
            assert!($size > 0);
        }
        );
    );

impl <R> Read for PackedRead<R> where R: BufRead {

    fn read(&mut self, out_buf: &mut [u8]) -> io::Result<usize> {
        let len = out_buf.len();

        if len == 0 { return Ok(0); }

        assert!(len % 8 == 0, "PackedRead reads must be word-aligned");

        unsafe {
            let mut out = out_buf.as_mut_ptr();
            let out_end: *mut u8 = out_buf.get_unchecked_mut(len);

            let (mut in_ptr, mut in_end) = try!(self.get_read_buffer());
            let mut buffer_begin = in_ptr;
            let mut size = ptr_sub(in_end, in_ptr);
            if size == 0 {
                return Ok(0);
            }

            loop {

                let mut tag : u8;

                assert!(ptr_sub(out, out_buf.as_mut_ptr()) % 8 == 0,
                        "Output pointer should always be aligned here.");

                if ptr_sub(in_end, in_ptr) < 10 {
                    if out >= out_end {
                        self.inner.consume(ptr_sub(in_ptr, buffer_begin));
                        return Ok(ptr_sub(out, out_buf.as_mut_ptr()));
                    }

                    if ptr_sub(in_end, in_ptr) == 0 {
                        refresh_buffer!(self, size, in_ptr, in_end, out, out_buf, buffer_begin);
                        continue;
                    }

                    //# We have at least 1, but not 10, bytes available. We need to read
                    //# slowly, doing a bounds check on each byte.

                    tag = *in_ptr;
                    in_ptr = in_ptr.offset(1);

                    for i in 0..8 {
                        if (tag & (1u8 << i)) != 0 {
                            if ptr_sub(in_end, in_ptr) == 0 {
                                refresh_buffer!(self, size, in_ptr, in_end,
                                                out, out_buf, buffer_begin);
                            }
                            *out = *in_ptr;
                            out = out.offset(1);
                            in_ptr = in_ptr.offset(1);
                        } else {
                            *out = 0;
                            out = out.offset(1);
                        }
                    }

                    if ptr_sub(in_end, in_ptr) == 0 && (tag == 0 || tag == 0xff) {
                        refresh_buffer!(self, size, in_ptr, in_end,
                                        out, out_buf, buffer_begin);
                    }
                } else {
                    tag = *in_ptr;
                    in_ptr = in_ptr.offset(1);

                    for n in 0..8 {
                        let is_nonzero = (tag & (1u8 << n)) != 0;
                        *out = (*in_ptr) & ((-(is_nonzero as i8)) as u8);
                        out = out.offset(1);
                        in_ptr = in_ptr.offset(is_nonzero as isize);
                    }
                }
                if tag == 0 {
                    assert!(ptr_sub(in_end, in_ptr) > 0,
                            "Should always have non-empty buffer here");

                    let run_length : usize = (*in_ptr) as usize * 8;
                    in_ptr = in_ptr.offset(1);

                    if run_length > ptr_sub(out_end, out) {
                        return Err(io::Error::new(io::ErrorKind::Other,
                                                  "Packed input did not end cleanly on a segment boundary"));
                    }

                    ptr::write_bytes(out, 0, run_length);
                    out = out.offset(run_length as isize);

                } else if tag == 0xff {
                    assert!(ptr_sub(in_end, in_ptr) > 0,
                            "Should always have non-empty buffer here");

                    let mut run_length : usize = (*in_ptr) as usize * 8;
                    in_ptr = in_ptr.offset(1);

                    if run_length > ptr_sub(out_end, out) {
                        return Err(io::Error::new(io::ErrorKind::Other,
                                                  "Packed input did not end cleanly on a segment boundary"));
                    }

                    let in_remaining = ptr_sub(in_end, in_ptr);
                    if in_remaining >= run_length {
                        //# Fast path.
                        ptr::copy_nonoverlapping(in_ptr, out, run_length);
                        out = out.offset(run_length as isize);
                        in_ptr = in_ptr.offset(run_length as isize);
                    } else {
                        //# Copy over the first buffer, then do one big read for the rest.
                        ptr::copy_nonoverlapping(in_ptr, out, in_remaining);
                        out = out.offset(in_remaining as isize);
                        run_length -= in_remaining;

                        self.inner.consume(size);
                        {
                            let buf = slice::from_raw_parts_mut::<u8>(out, run_length);
                            try!(read_exact(&mut self.inner, buf));
                        }

                        out = out.offset(run_length as isize);

                        if out == out_end {
                            return Ok(len);
                        } else {
                            let (b, e) = try!(self.get_read_buffer());
                            in_ptr = b;
                            in_end = e;
                            size = ptr_sub(e, b);
                            buffer_begin = in_ptr;
                            continue;
                        }
                    }
                }

                if out == out_end {
                    self.inner.consume(ptr_sub(in_ptr, buffer_begin));
                    return Ok(len);
                }
            }
        }
    }
}

pub fn read_message<R>(read: &mut R,
                          options: ReaderOptions)
                          -> Result<serialize::OwnedSpaceMessageReader>
where R: BufRead {
    let mut packed_read = PackedRead { inner: read };
    serialize::read_message(&mut packed_read, options)
}

struct PackedWrite<W> where W: Write {
    inner: W,
}

impl <W> Write for PackedWrite<W> where W: Write {
    fn write(&mut self, in_buf: &[u8]) -> io::Result<usize> {
        unsafe {
            let mut buf_idx: usize = 0;
            let mut buf: [u8; 64] = [0; 64];

            let mut in_ptr: *const u8 = in_buf.get_unchecked(0);
            let in_end: *const u8 = in_buf.get_unchecked(in_buf.len());

            while in_ptr < in_end {

                if buf_idx + 10 > buf.len() {
                    //# Oops, we're out of space. We need at least 10
                    //# bytes for the fast path, since we don't
                    //# bounds-check on every byte.
                    try!(self.inner.write_all(&buf[..buf_idx]));
                    buf_idx = 0;
                }

                let tag_pos = buf_idx;
                buf_idx += 1;

                let bit0 = (*in_ptr != 0) as u8;
                *buf.get_unchecked_mut(buf_idx) = *in_ptr;
                buf_idx += bit0 as usize;
                in_ptr = in_ptr.offset(1);

                let bit1 = (*in_ptr != 0) as u8;
                *buf.get_unchecked_mut(buf_idx) = *in_ptr;
                buf_idx += bit1 as usize;
                in_ptr = in_ptr.offset(1);

                let bit2 = (*in_ptr != 0) as u8;
                *buf.get_unchecked_mut(buf_idx) = *in_ptr;
                buf_idx += bit2 as usize;
                in_ptr = in_ptr.offset(1);

                let bit3 = (*in_ptr != 0) as u8;
                *buf.get_unchecked_mut(buf_idx) = *in_ptr;
                buf_idx += bit3 as usize;
                in_ptr = in_ptr.offset(1);

                let bit4 = (*in_ptr != 0) as u8;
                *buf.get_unchecked_mut(buf_idx) = *in_ptr;
                buf_idx += bit4 as usize;
                in_ptr = in_ptr.offset(1);

                let bit5 = (*in_ptr != 0) as u8;
                *buf.get_unchecked_mut(buf_idx) = *in_ptr;
                buf_idx += bit5 as usize;
                in_ptr = in_ptr.offset(1);

                let bit6 = (*in_ptr != 0) as u8;
                *buf.get_unchecked_mut(buf_idx) = *in_ptr;
                buf_idx += bit6 as usize;
                in_ptr = in_ptr.offset(1);

                let bit7 = (*in_ptr != 0) as u8;
                *buf.get_unchecked_mut(buf_idx) = *in_ptr;
                buf_idx += bit7 as usize;
                in_ptr = in_ptr.offset(1);

                let tag: u8 = (bit0 << 0) | (bit1 << 1) | (bit2 << 2) | (bit3 << 3)
                            | (bit4 << 4) | (bit5 << 5) | (bit6 << 6) | (bit7 << 7);


                *buf.get_unchecked_mut(tag_pos) = tag;

                if tag == 0 {
                    //# An all-zero word is followed by a count of
                    //# consecutive zero words (not including the first
                    //# one).

                    let mut in_word : *const u64 = in_ptr as *const u64;
                    let mut limit : *const u64 = in_end as *const u64;
                    if ptr_sub(limit, in_word) > 255 {
                        limit = in_word.offset(255);
                    }
                    while in_word < limit && *in_word == 0 {
                        in_word = in_word.offset(1);
                    }

                    *buf.get_unchecked_mut(buf_idx) = ptr_sub(in_word, in_ptr as *const u64) as u8;
                    buf_idx += 1;
                    in_ptr = in_word as *const u8;
                } else if tag == 0xff {
                    //# An all-nonzero word is followed by a count of
                    //# consecutive uncompressed words, followed by the
                    //# uncompressed words themselves.

                    //# Count the number of consecutive words in the input
                    //# which have no more than a single zero-byte. We look
                    //# for at least two zeros because that's the point
                    //# where our compression scheme becomes a net win.
                    let run_start = in_ptr;
                    let mut limit = in_end;
                    if ptr_sub(limit, in_ptr) > 255 * 8 {
                        limit = in_ptr.offset(255 * 8);
                    }

                    while in_ptr < limit {
                        let mut c = 0;

                        for _ in 0..8 {
                            c += (*in_ptr == 0) as u8;
                            in_ptr = in_ptr.offset(1);
                        }

                        if c >= 2 {
                            //# Un-read the word with multiple zeros, since
                            //# we'll want to compress that one.
                            in_ptr = in_ptr.offset(-8);
                            break;
                        }
                    }

                    let count: usize = ptr_sub(in_ptr, run_start);
                    *buf.get_unchecked_mut(buf_idx) = (count / 8) as u8;
                    buf_idx += 1;

                    try!(self.inner.write_all(&buf[..buf_idx]));
                    buf_idx = 0;
                    try!(self.inner.write_all(slice::from_raw_parts::<u8>(run_start, count)));
                }
            }

            try!(self.inner.write_all(&buf[..buf_idx]));
            Ok(in_buf.len())
        }
    }

   fn flush(&mut self) -> io::Result<()> { self.inner.flush() }
}

pub fn write_message<W, M>(write: &mut W, message : &mut M) -> io::Result<()>
where W: Write, M: MessageBuilder {
    let mut packed_write = PackedWrite { inner: write };
    serialize::write_message(&mut packed_write, message)
}

#[cfg(test)]
mod tests {

    use std::iter;
    use std::io::Write;

    use std::io::Cursor;
    use quickcheck::{quickcheck, TestResult};

    use {Word, MessageReader};
    use message::ReaderOptions;
    use serialize::test::write_message_segments;
    use serialize_packed::{PackedRead, PackedWrite};
    use super::read_message;
    use util::read_exact;

    pub fn expect_packs_to(unpacked : &[u8],
                           packed : &[u8]) {

        // --------
        // write

        let mut bytes : Vec<u8> = iter::repeat(0u8).take(packed.len()).collect();
        {
            let mut packed_write = PackedWrite { inner: &mut bytes[..] };
            packed_write.write(unpacked).unwrap();
        }

        assert_eq!(bytes, packed);

        // --------
        // read

        let mut packed_read = PackedRead {inner: packed};


        let mut bytes : Vec<u8> = iter::repeat(0u8).take(unpacked.len()).collect();
        read_exact(&mut packed_read, &mut bytes[..]).unwrap();

        //    assert!(packed_read.eof());
        assert_eq!(bytes, unpacked);
    }

    #[test]
    pub fn simple_packing() {
        expect_packs_to(&[], &[]);
        expect_packs_to(&[0; 8], &[0,0]);
        expect_packs_to(&[0,0,12,0,0,34,0,0], &[0x24,12,34]);
        expect_packs_to(&[1,3,2,4,5,7,6,8], &[0xff,1,3,2,4,5,7,6,8,0]);
        expect_packs_to(&[0,0,0,0,0,0,0,0,1,3,2,4,5,7,6,8], &[0,0,0xff,1,3,2,4,5,7,6,8,0]);
        expect_packs_to(&[0,0,12,0,0,34,0,0,1,3,2,4,5,7,6,8], &[0x24,12,34,0xff,1,3,2,4,5,7,6,8,0]);
        expect_packs_to(&[1,3,2,4,5,7,6,8,8,6,7,4,5,2,3,1], &[0xff,1,3,2,4,5,7,6,8,1,8,6,7,4,5,2,3,1]);

        expect_packs_to(
            &[1,2,3,4,5,6,7,8, 1,2,3,4,5,6,7,8, 1,2,3,4,5,6,7,8, 1,2,3,4,5,6,7,8, 0,2,4,0,9,0,5,1],
            &[0xff,1,2,3,4,5,6,7,8, 3, 1,2,3,4,5,6,7,8, 1,2,3,4,5,6,7,8, 1,2,3,4,5,6,7,8,
              0xd6,2,4,9,5,1]);
        expect_packs_to(
            &[1,2,3,4,5,6,7,8, 1,2,3,4,5,6,7,8, 6,2,4,3,9,0,5,1, 1,2,3,4,5,6,7,8, 0,2,4,0,9,0,5,1],
            &[0xff,1,2,3,4,5,6,7,8, 3, 1,2,3,4,5,6,7,8, 6,2,4,3,9,0,5,1, 1,2,3,4,5,6,7,8,
              0xd6,2,4,9,5,1]);

        expect_packs_to(
            &[8,0,100,6,0,1,1,2, 0,0,0,0,0,0,0,0, 0,0,0,0,0,0,0,0, 0,0,0,0,0,0,0,0, 0,0,1,0,2,0,3,1],
            &[0xed,8,100,6,1,1,2, 0,2, 0xd4,1,2,3,1]);

        expect_packs_to(&[0; 16], &[0,1]);
        expect_packs_to(&[0,0,0,0,0,0,0,0, 0,0,0,0,0,0,0,0, 0,0,0,0,0,0,0,0], &[0,2]);
    }

    #[test]
    fn check_round_trip() {
        fn round_trip(segments: Vec<Vec<Word>>) -> TestResult {
            if segments.len() == 0 { return TestResult::discard(); }
            let mut cursor = Cursor::new(Vec::new());

            write_message_segments(&mut PackedWrite { inner: &mut cursor }, &segments);
            cursor.set_position(0);
            let message = read_message(&mut cursor, ReaderOptions::new()).unwrap();

            TestResult::from_bool(segments.iter().enumerate().all(|(i, segment)| {
                &segment[..] == message.get_segment(i)
            }))
        }

        quickcheck(round_trip as fn(Vec<Vec<Word>>) -> TestResult);
    }
}
