//! Wire protocol between the viewer and its agents.
//!
//! The same shape as nerdmux's: every frame is a little-endian `u32` length
//! followed by the body, written by hand (integers little-endian, strings
//! and lists as a `u32` length then their contents, enums as a one-byte tag
//! then their fields).
//!
//! Unlike nerdmux, the other end is not trusted: an agent runs on another
//! machine, and a compromised machine must not be able to take over the
//! viewer. So decoding has hard limits on frame size, string length and
//! list length, every string is cleaned of control characters as it is read
//! (see `text::clean`), and anything malformed is an error, never a panic.

use crate::text;
use std::io::{self, Read, Write};

pub const PROTOCOL_VERSION: u32 = 1;

/// The largest frame either side accepts. A sample is a few hundred bytes
/// and an inventory a few kilobytes; history (later) is the large one.
pub const MAX_FRAME: u32 = 4 * 1024 * 1024;
/// The longest string, in bytes, before cleaning. Command lines are the
/// longest strings we send; they are cut to this.
pub const MAX_STRING: usize = 4096;
/// The longest list. More cores, mounts or GPUs than this is not a real
/// machine.
pub const MAX_LIST: usize = 4096;

pub fn write_frame<W: Write, T: Wire>(w: &mut W, msg: &T) -> io::Result<()> {
    let mut frame = vec![0u8; 4];
    msg.put(&mut frame);
    let len = frame.len() - 4;
    if len > MAX_FRAME as usize {
        return Err(bad("frame too large to send"));
    }
    frame[..4].copy_from_slice(&(len as u32).to_le_bytes());
    w.write_all(&frame)?;
    w.flush()
}

/// Reads one frame. `Ok(None)` is a clean end of stream between frames;
/// ending partway through a frame is an error.
pub fn read_frame<R: Read, T: Wire>(r: &mut R) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read(&mut len[..1])? {
        0 => return Ok(None),
        _ => r.read_exact(&mut len[1..])?,
    }
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        return Err(bad("frame too large"));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    let mut rest = body.as_slice();
    let msg = T::get(&mut rest)?;
    if !rest.is_empty() {
        return Err(bad("trailing bytes in frame"));
    }
    Ok(Some(msg))
}

// ===== encoding =============================================================

pub fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

/// A value that can be written to and read from a frame body.
pub trait Wire: Sized {
    fn put(&self, out: &mut Vec<u8>);
    fn get(input: &mut &[u8]) -> io::Result<Self>;
}

fn take<'a>(input: &mut &'a [u8], n: usize) -> io::Result<&'a [u8]> {
    if input.len() < n {
        return Err(bad("frame ends early"));
    }
    let (head, rest) = input.split_at(n);
    *input = rest;
    Ok(head)
}

macro_rules! wire_int {
    ($($t:ty),*) => {$(
        impl Wire for $t {
            fn put(&self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_le_bytes());
            }
            fn get(input: &mut &[u8]) -> io::Result<Self> {
                let bytes = take(input, std::mem::size_of::<$t>())?;
                Ok(<$t>::from_le_bytes(bytes.try_into().expect("length checked")))
            }
        }
    )*};
}
wire_int!(u8, u16, u32, u64, i32);

impl Wire for f32 {
    fn put(&self, out: &mut Vec<u8>) {
        self.to_bits().put(out);
    }
    fn get(input: &mut &[u8]) -> io::Result<Self> {
        let v = f32::from_bits(u32::get(input)?);
        // NaN and infinities have no business in a measurement, and would
        // poison every sum and scale they reach.
        if v.is_finite() {
            Ok(v)
        } else {
            Err(bad("number is not finite"))
        }
    }
}

impl Wire for bool {
    fn put(&self, out: &mut Vec<u8>) {
        out.push(*self as u8);
    }
    fn get(input: &mut &[u8]) -> io::Result<Self> {
        match u8::get(input)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(bad("bad bool")),
        }
    }
}

impl Wire for String {
    fn put(&self, out: &mut Vec<u8>) {
        let bytes = self.as_bytes();
        let bytes = &bytes[..floor_char_boundary(self, MAX_STRING.min(bytes.len()))];
        (bytes.len() as u32).put(out);
        out.extend_from_slice(bytes);
    }
    fn get(input: &mut &[u8]) -> io::Result<Self> {
        let len = u32::get(input)? as usize;
        if len > MAX_STRING {
            return Err(bad("string too long"));
        }
        let raw = std::str::from_utf8(take(input, len)?).map_err(|_| bad("string is not UTF-8"))?;
        Ok(text::clean(raw))
    }
}

/// The largest index no greater than `i` that starts a character, so a cut
/// string stays valid UTF-8.
fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

impl<T: Wire> Wire for Option<T> {
    fn put(&self, out: &mut Vec<u8>) {
        match self {
            None => out.push(0),
            Some(v) => {
                out.push(1);
                v.put(out);
            }
        }
    }
    fn get(input: &mut &[u8]) -> io::Result<Self> {
        match u8::get(input)? {
            0 => Ok(None),
            1 => Ok(Some(T::get(input)?)),
            _ => Err(bad("bad option")),
        }
    }
}

impl<T: Wire> Wire for Vec<T> {
    fn put(&self, out: &mut Vec<u8>) {
        let n = self.len().min(MAX_LIST);
        (n as u32).put(out);
        for v in &self[..n] {
            v.put(out);
        }
    }
    fn get(input: &mut &[u8]) -> io::Result<Self> {
        let n = u32::get(input)? as usize;
        // Every element takes at least a byte: a bogus count cannot make us
        // reserve more than the frame holds.
        if n > MAX_LIST || n > input.len() {
            return Err(bad("list too long"));
        }
        (0..n).map(|_| T::get(input)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T: Wire + PartialEq + std::fmt::Debug>(v: T) {
        let mut buf = Vec::new();
        write_frame(&mut buf, &v).unwrap();
        let back: T = read_frame(&mut buf.as_slice()).unwrap().unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn values_survive_a_roundtrip() {
        roundtrip(42u64);
        roundtrip(1.5f32);
        roundtrip(Some("gpu-box".to_string()));
        roundtrip(vec![1u16, 2, 3]);
        roundtrip(Option::<u8>::None);
    }

    #[test]
    fn end_of_stream_between_frames_is_clean() {
        let empty: &[u8] = &[];
        assert!(read_frame::<_, u32>(&mut &*empty).unwrap().is_none());
    }

    #[test]
    fn end_of_stream_inside_a_frame_is_an_error() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &7u64).unwrap();
        buf.truncate(buf.len() - 1);
        assert!(read_frame::<_, u64>(&mut buf.as_slice()).is_err());
        assert!(read_frame::<_, u64>(&mut &buf[..2]).is_err());
    }

    #[test]
    fn oversized_frames_are_refused_before_reading_them() {
        let frame = (MAX_FRAME + 1).to_le_bytes();
        assert!(read_frame::<_, u8>(&mut frame.as_slice()).is_err());
    }

    #[test]
    fn bogus_lengths_and_tags_are_refused() {
        // A list claiming more elements than the frame holds.
        let mut body = Vec::new();
        1_000_000u32.put(&mut body);
        assert!(Vec::<u8>::get(&mut body.as_slice()).is_err());
        // A string longer than the limit.
        let mut body = Vec::new();
        ((MAX_STRING + 1) as u32).put(&mut body);
        body.resize(body.len() + MAX_STRING + 1, b'a');
        assert!(String::get(&mut body.as_slice()).is_err());
        // Not UTF-8, a bad bool, a bad option tag, a NaN.
        assert!(String::get(&mut [1, 0, 0, 0, 0xff].as_slice()).is_err());
        assert!(bool::get(&mut [2].as_slice()).is_err());
        assert!(Option::<u8>::get(&mut [9].as_slice()).is_err());
        assert!(f32::get(&mut f32::NAN.to_bits().to_le_bytes().as_slice()).is_err());
    }

    #[test]
    fn trailing_bytes_are_refused() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &(1u32, 2u32)).unwrap();
        assert!(read_frame::<_, u32>(&mut buf.as_slice()).is_err());
    }

    #[test]
    fn strings_are_cleaned_as_they_are_read() {
        let mut body = Vec::new();
        "gpu\x1b]0;pwned\x07name".to_string().put(&mut body);
        let s = String::get(&mut body.as_slice()).unwrap();
        assert!(!s.contains('\x1b') && !s.contains('\x07'));
    }

    #[test]
    fn long_strings_are_cut_on_a_character_boundary() {
        let long = "é".repeat(MAX_STRING);
        let mut body = Vec::new();
        long.put(&mut body);
        let s = String::get(&mut body.as_slice()).unwrap();
        assert!(s.len() <= MAX_STRING && s.chars().all(|c| c == 'é'));
    }

    impl<A: Wire, B: Wire> Wire for (A, B) {
        fn put(&self, out: &mut Vec<u8>) {
            self.0.put(out);
            self.1.put(out);
        }
        fn get(input: &mut &[u8]) -> io::Result<Self> {
            Ok((A::get(input)?, B::get(input)?))
        }
    }
}
