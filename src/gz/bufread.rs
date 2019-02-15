use std::cmp;
use std::io;
use std::io::prelude::*;
use std::mem;
use crc32fast::Hasher;

#[cfg(feature = "tokio")]
use futures::Poll;
#[cfg(feature = "tokio")]
use tokio_io::{AsyncRead, AsyncWrite};

use super::{GzBuilder, GzHeader};
use super::{FCOMMENT, FEXTRA, FHCRC, FNAME};
use crc::CrcReader;
use deflate;
use Compression;

fn copy(into: &mut [u8], from: &[u8], pos: &mut usize) -> usize {
    let min = cmp::min(into.len(), from.len() - *pos);
    for (slot, val) in into.iter_mut().zip(from[*pos..*pos + min].iter()) {
        *slot = *val;
    }
    *pos += min;
    return min;
}

pub(crate) fn corrupt() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "corrupt gzip stream does not have a matching checksum",
    )
}

fn bad_header() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid gzip header")
}

pub(crate) fn read_gz_header<R: Read>(r: &mut R) -> io::Result<GzHeader> {
    let mut state = GzHeaderState::Header(0, [0; 10]);
    let mut header = GzHeader::default();
    let mut flag = 0;
    let mut hasher = Hasher::new();
    read_gz_header2(r, &mut state, &mut header, &mut flag, &mut hasher)
        .map(|_| header)
}

#[derive(Debug)]
enum GzHeaderState {
    Header(usize, [u8; 10]),    // pos, buf
    ExtraLen(usize, [u8; 2]),   // pos, buf
    Extra(usize),               // pos
    FileName,
    Comment,
    Crc(u16, usize, [u8; 2])    // crc, pos, buf
}

fn read_gz_header2<R: Read>(
    r: &mut R,
    state: &mut GzHeaderState,
    header: &mut GzHeader,
    flag: &mut u8,
    hasher: &mut Hasher
) -> io::Result<()> {
    enum Next {
        None,
        ExtraLen,
        Extra,
        FileName,
        Comment,
        Crc
    }

    let mut next = Next::None;

    loop {
        match state {
            GzHeaderState::Header(pos, buf) => if *pos < buf.len() {
                let len = r.read(&mut buf[*pos..])
                    .and_then(|len| if len != 0 {
                        Ok(len)
                    } else {
                        Err(io::ErrorKind::UnexpectedEof.into())
                    })?;
                *pos += len;
            } else {
                hasher.update(buf);

                let id1 = buf[0];
                let id2 = buf[1];
                if id1 != 0x1f || id2 != 0x8b {
                    return Err(bad_header());
                }
                let cm = buf[2];
                if cm != 8 {
                    return Err(bad_header());
                }

                let flg = buf[3];
                let mtime = ((buf[4] as u32) << 0)
                    | ((buf[5] as u32) << 8)
                    | ((buf[6] as u32) << 16)
                    | ((buf[7] as u32) << 24);
                let _xfl = buf[8];
                let os = buf[9];

                header.operating_system = os;
                header.mtime = mtime;
                *flag = flg;

                next = Next::ExtraLen;
            },
            GzHeaderState::ExtraLen(..) if *flag & FEXTRA == 0 => next = Next::FileName,
            GzHeaderState::ExtraLen(pos, buf) => if *pos < buf.len() {
                let len = r.read(&mut buf[*pos..])
                    .and_then(|len| if len != 0 {
                        Ok(len)
                    } else {
                        Err(io::ErrorKind::UnexpectedEof.into())
                    })?;
                *pos += len;
            } else {
                hasher.update(buf);

                let xlen = (buf[0] as u16) | ((buf[1] as u16) << 8);
                header.extra = Some(vec![0; xlen as usize]);
                if xlen != 0 {
                    next = Next::Extra;
                } else {
                    next = Next::FileName;
                }
            },
            GzHeaderState::Extra(pos) => if let Some(extra) = &mut header.extra {
                if *pos < extra.len() {
                    let len = r.read(&mut extra[*pos..])
                        .and_then(|len| if len != 0 {
                            Ok(len)
                        } else {
                            Err(io::ErrorKind::UnexpectedEof.into())
                        })?;
                    *pos += len;
                } else {
                    next = Next::FileName;
                }
            },
            GzHeaderState::FileName if *flag & FNAME == 0 => next = Next::Comment,
            GzHeaderState::FileName => {
                let filename = header.filename.get_or_insert_with(Vec::new);

                // wow this is slow
                for byte in r.by_ref().bytes() {
                    let byte = byte?;
                    if byte == 0 {
                        break;
                    }
                    filename.push(byte);
                }

                hasher.update(filename);
                hasher.update(&[0]);
                next = Next::Comment;
            },
            GzHeaderState::Comment if *flag & FCOMMENT == 0 => next = Next::Crc,
            GzHeaderState::Comment => {
                let comment = header.comment.get_or_insert_with(Vec::new);

                // wow this is slow
                for byte in r.by_ref().bytes() {
                    let byte = byte?;
                    if byte == 0 {
                        break;
                    }
                    comment.push(byte);
                }

                hasher.update(comment);
                hasher.update(&[0]);
                next = Next::Crc
            },
            GzHeaderState::Crc(..) if *flag & FHCRC == 0 => return Ok(()),
            GzHeaderState::Crc(calced_crc, pos, buf) => if *pos < buf.len() {
                let len = r.read(&mut buf[*pos..])
                    .and_then(|len| if len != 0 {
                        Ok(len)
                    } else {
                        Err(io::ErrorKind::UnexpectedEof.into())
                    })?;
                *pos += len;
            } else {
                let stored_crc = (buf[0] as u16) | ((buf[1] as u16) << 8);
                if *calced_crc != stored_crc {
                    return Err(corrupt());
                } else {
                    return Ok(())
                }
            }
        };

        match mem::replace(&mut next, Next::None) {
            Next::ExtraLen => *state = GzHeaderState::ExtraLen(0, [0; 2]),
            Next::Extra => *state = GzHeaderState::Extra(0),
            Next::FileName => *state = GzHeaderState::FileName,
            Next::Comment => *state = GzHeaderState::Comment,
            Next::Crc => *state = GzHeaderState::Crc(hasher.clone().finalize() as u16, 0, [0; 2]),
            Next::None => ()
        }
    }
}

/// A gzip streaming encoder
///
/// This structure exposes a [`BufRead`] interface that will read uncompressed data
/// from the underlying reader and expose the compressed version as a [`BufRead`]
/// interface.
///
/// [`BufRead`]: https://doc.rust-lang.org/std/io/trait.BufRead.html
///
/// # Examples
///
/// ```
/// use std::io::prelude::*;
/// use std::io;
/// use flate2::Compression;
/// use flate2::bufread::GzEncoder;
/// use std::fs::File;
/// use std::io::BufReader;
///
/// // Opens sample file, compresses the contents and returns a Vector or error
/// // File wrapped in a BufReader implements BufRead
///
/// fn open_hello_world() -> io::Result<Vec<u8>> {
///     let f = File::open("examples/hello_world.txt")?;
///     let b = BufReader::new(f);
///     let mut gz = GzEncoder::new(b, Compression::fast());
///     let mut buffer = Vec::new();
///     gz.read_to_end(&mut buffer)?;
///     Ok(buffer)
/// }
/// ```
#[derive(Debug)]
pub struct GzEncoder<R> {
    inner: deflate::bufread::DeflateEncoder<CrcReader<R>>,
    header: Vec<u8>,
    pos: usize,
    eof: bool,
}

pub fn gz_encoder<R: BufRead>(header: Vec<u8>, r: R, lvl: Compression) -> GzEncoder<R> {
    let crc = CrcReader::new(r);
    GzEncoder {
        inner: deflate::bufread::DeflateEncoder::new(crc, lvl),
        header: header,
        pos: 0,
        eof: false,
    }
}

impl<R: BufRead> GzEncoder<R> {
    /// Creates a new encoder which will use the given compression level.
    ///
    /// The encoder is not configured specially for the emitted header. For
    /// header configuration, see the `GzBuilder` type.
    ///
    /// The data read from the stream `r` will be compressed and available
    /// through the returned reader.
    pub fn new(r: R, level: Compression) -> GzEncoder<R> {
        GzBuilder::new().buf_read(r, level)
    }

    fn read_footer(&mut self, into: &mut [u8]) -> io::Result<usize> {
        if self.pos == 8 {
            return Ok(0);
        }
        let crc = self.inner.get_ref().crc();
        let ref arr = [
            (crc.sum() >> 0) as u8,
            (crc.sum() >> 8) as u8,
            (crc.sum() >> 16) as u8,
            (crc.sum() >> 24) as u8,
            (crc.amount() >> 0) as u8,
            (crc.amount() >> 8) as u8,
            (crc.amount() >> 16) as u8,
            (crc.amount() >> 24) as u8,
        ];
        Ok(copy(into, arr, &mut self.pos))
    }
}

impl<R> GzEncoder<R> {
    /// Acquires a reference to the underlying reader.
    pub fn get_ref(&self) -> &R {
        self.inner.get_ref().get_ref()
    }

    /// Acquires a mutable reference to the underlying reader.
    ///
    /// Note that mutation of the reader may result in surprising results if
    /// this encoder is continued to be used.
    pub fn get_mut(&mut self) -> &mut R {
        self.inner.get_mut().get_mut()
    }

    /// Returns the underlying stream, consuming this encoder
    pub fn into_inner(self) -> R {
        self.inner.into_inner().into_inner()
    }
}

#[inline]
fn finish(buf: &[u8; 8]) -> (u32, u32) {
    let crc = ((buf[0] as u32) << 0)
        | ((buf[1] as u32) << 8)
        | ((buf[2] as u32) << 16)
        | ((buf[3] as u32) << 24);
    let amt = ((buf[4] as u32) << 0)
        | ((buf[5] as u32) << 8)
        | ((buf[6] as u32) << 16)
        | ((buf[7] as u32) << 24);
    (crc, amt)
}

impl<R: BufRead> Read for GzEncoder<R> {
    fn read(&mut self, mut into: &mut [u8]) -> io::Result<usize> {
        let mut amt = 0;
        if self.eof {
            return self.read_footer(into);
        } else if self.pos < self.header.len() {
            amt += copy(into, &self.header, &mut self.pos);
            if amt == into.len() {
                return Ok(amt);
            }
            let tmp = into;
            into = &mut tmp[amt..];
        }
        match self.inner.read(into)? {
            0 => {
                self.eof = true;
                self.pos = 0;
                self.read_footer(into)
            }
            n => Ok(amt + n),
        }
    }
}

impl<R: BufRead + Write> Write for GzEncoder<R> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.get_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.get_mut().flush()
    }
}

/// A gzip streaming decoder
///
/// This structure exposes a [`ReadBuf`] interface that will consume compressed
/// data from the underlying reader and emit uncompressed data.
///
/// [`ReadBuf`]: https://doc.rust-lang.org/std/io/trait.BufRead.html
///
/// # Examples
///
/// ```
/// use std::io::prelude::*;
/// use std::io;
/// # use flate2::Compression;
/// # use flate2::write::GzEncoder;
/// use flate2::bufread::GzDecoder;
///
/// # fn main() {
/// #   let mut e = GzEncoder::new(Vec::new(), Compression::default());
/// #   e.write_all(b"Hello World").unwrap();
/// #   let bytes = e.finish().unwrap();
/// #   println!("{}", decode_reader(bytes).unwrap());
/// # }
/// #
/// // Uncompresses a Gz Encoded vector of bytes and returns a string or error
/// // Here &[u8] implements BufRead
///
/// fn decode_reader(bytes: Vec<u8>) -> io::Result<String> {
///    let mut gz = GzDecoder::new(&bytes[..]);
///    let mut s = String::new();
///    gz.read_to_string(&mut s)?;
///    Ok(s)
/// }
/// ```
#[derive(Debug)]
pub struct GzDecoder<R> {
    inner: GzState,
    header: GzHeader,
    reader: CrcReader<deflate::bufread::DeflateDecoder<R>>,
    multi: bool
}

#[derive(Debug)]
enum GzState {
    Header {
        state: GzHeaderState,
        flag: u8,
        hasher: Hasher
    },
    Body,
    Finished(usize, [u8; 8]),
    Err(io::Error),
    End
}

impl<R: BufRead> GzDecoder<R> {
    /// Creates a new decoder from the given reader, immediately parsing the
    /// gzip header.
    pub fn new(mut r: R) -> GzDecoder<R> {
        let mut state = GzHeaderState::Header(0, [0; 10]);
        let mut header = GzHeader::default();
        let mut flag = 0;
        let mut hasher = Hasher::new();
        let result = read_gz_header2(&mut r, &mut state, &mut header, &mut flag, &mut hasher);

        GzDecoder {
            inner: if let Err(err) = result {
                GzState::Err(err)
            } else {
                GzState::Body
            },
            reader: CrcReader::new(deflate::bufread::DeflateDecoder::new(r)),
            multi: false,
            header
        }
    }

    /// Creates a new decoder from the given reader.
    pub fn new2(r: R) -> GzDecoder<R> {
        GzDecoder {
            inner: GzState::Header {
                state: GzHeaderState::Header(0, [0; 10]),
                flag: 0,
                hasher: Hasher::new()
            },
            header: GzHeader::default(),
            reader: CrcReader::new(deflate::bufread::DeflateDecoder::new(r)),
            multi: false
        }
    }

    fn multi(mut self, flag: bool) -> GzDecoder<R> {
        self.multi = flag;
        self
    }
}

impl<R> GzDecoder<R> {
    /// Returns the header associated with this stream, if it was valid
    pub fn header(&self) -> Option<&GzHeader> {
        match self.inner {
            GzState::Err(_) | GzState::Header { .. } => None,
            _ => Some(&self.header)
        }
    }

    /// Acquires a reference to the underlying reader.
    pub fn get_ref(&self) -> &R {
        self.reader.get_ref().get_ref()
    }

    /// Acquires a mutable reference to the underlying stream.
    ///
    /// Note that mutation of the stream may result in surprising results if
    /// this encoder is continued to be used.
    pub fn get_mut(&mut self) -> &mut R {
        self.reader.get_mut().get_mut()
    }

    /// Consumes this decoder, returning the underlying reader.
    pub fn into_inner(self) -> R {
        self.reader.into_inner().into_inner()
    }
}

impl<R: BufRead> Read for GzDecoder<R> {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        let GzDecoder { inner, header, reader, multi } = self;

        enum Next {
            None,
            Header,
            Body,
            Finished,
            Err(io::Error),
            End
        }

        let mut next = Next::None;

        loop {
            match inner {
                GzState::Header { state, flag, hasher } => {
                    match read_gz_header2(reader.get_mut().get_mut(), state, header, flag, hasher) {
                        Ok(_) => next = Next::Body,
                        Err(err) => if io::ErrorKind::WouldBlock == err.kind() {
                            return Err(err);
                        } else {
                            next = Next::Err(err);
                        }
                    }
                },
                GzState::Body => {
                    if into.is_empty() {
                        return Ok(0);
                    }

                    match reader.read(into)? {
                        0 => next = Next::Finished,
                        n => return Ok(n)
                    }
                },
                GzState::Finished(pos, buf) => if *pos < buf.len() {
                    match reader.get_mut().get_mut().read(&mut buf[*pos..]) {
                        Ok(0) => next = Next::Err(io::ErrorKind::UnexpectedEof.into()),
                        Ok(n) => *pos += n,
                        Err(err) => if io::ErrorKind::WouldBlock == err.kind() {
                            return Err(err);
                        } else {
                            next = Next::Err(err);
                        }
                    }
                } else {
                    let (crc, amt) = finish(buf);

                    if crc != reader.crc().sum() {
                        next = Next::Err(corrupt());
                    } else if amt != reader.crc().amount() {
                        next = Next::Err(corrupt());
                    } else if !*multi {
                        next = Next::End;
                    } else {
                        match reader.get_mut().get_mut().fill_buf() {
                            Ok(buf) => if buf.is_empty() {
                                next = Next::End;
                            } else {
                                next = Next::Header;
                            },
                            Err(err) => if io::ErrorKind::WouldBlock == err.kind() {
                                return Err(err);
                            } else {
                                next = Next::Err(err);
                            }
                        }
                    }
                },
                GzState::Err(err) => next = Next::Err(mem::replace(err, io::ErrorKind::Other.into())),
                GzState::End => return Ok(0)
            }

            match mem::replace(&mut next, Next::None) {
                Next::None => (),
                Next::Header => {
                    reader.reset();
                    reader.get_mut().reset_data();
                    *header = GzHeader::default();
                    *inner = GzState::Header {
                        state: GzHeaderState::Header(0, [0; 10]),
                        flag: 0,
                        hasher: Hasher::new()
                    };
                },
                Next::Body => *inner = GzState::Body,
                Next::Finished => *inner = GzState::Finished(0, [0; 8]),
                Next::Err(err) => {
                    *inner = GzState::End;
                    return Err(err);
                },
                Next::End => *inner = GzState::End
            }
        }
    }
}

#[cfg(feature = "tokio")]
impl<R: AsyncRead + BufRead> AsyncRead for GzDecoder<R> {}

impl<R: BufRead + Write> Write for GzDecoder<R> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.get_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.get_mut().flush()
    }
}

#[cfg(feature = "tokio")]
impl<R: AsyncWrite + BufRead> AsyncWrite for GzDecoder<R> {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        self.get_mut().shutdown()
    }
}

/// A gzip streaming decoder that decodes all members of a multistream
///
/// A gzip member consists of a header, compressed data and a trailer. The [gzip
/// specification](https://tools.ietf.org/html/rfc1952), however, allows multiple
/// gzip members to be joined in a single stream. `MultiGzDecoder` will
/// decode all consecutive members while `GzDecoder` will only decompress
/// the first gzip member. The multistream format is commonly used in
/// bioinformatics, for example when using the BGZF compressed data.
///
/// This structure exposes a [`BufRead`] interface that will consume all gzip members
/// from the underlying reader and emit uncompressed data.
///
/// [`BufRead`]: https://doc.rust-lang.org/std/io/trait.BufRead.html
///
/// # Examples
///
/// ```
/// use std::io::prelude::*;
/// use std::io;
/// # use flate2::Compression;
/// # use flate2::write::GzEncoder;
/// use flate2::bufread::MultiGzDecoder;
///
/// # fn main() {
/// #   let mut e = GzEncoder::new(Vec::new(), Compression::default());
/// #   e.write_all(b"Hello World").unwrap();
/// #   let bytes = e.finish().unwrap();
/// #   println!("{}", decode_reader(bytes).unwrap());
/// # }
/// #
/// // Uncompresses a Gz Encoded vector of bytes and returns a string or error
/// // Here &[u8] implements BufRead
///
/// fn decode_reader(bytes: Vec<u8>) -> io::Result<String> {
///    let mut gz = MultiGzDecoder::new(&bytes[..]);
///    let mut s = String::new();
///    gz.read_to_string(&mut s)?;
///    Ok(s)
/// }
/// ```
#[derive(Debug)]
pub struct MultiGzDecoder<R>(GzDecoder<R>);

impl<R: BufRead> MultiGzDecoder<R> {
    /// Creates a new decoder from the given reader, immediately parsing the
    /// (first) gzip header. If the gzip stream contains multiple members all will
    /// be decoded.
    pub fn new(r: R) -> MultiGzDecoder<R> {
        MultiGzDecoder(GzDecoder::new(r).multi(true))
    }

    /// Creates a new decoder from the given reader.
    /// If the gzip stream contains multiple members all will be decoded.
    pub fn new2(r: R) -> MultiGzDecoder<R> {
        MultiGzDecoder(GzDecoder::new2(r).multi(true))
    }
}

impl<R> MultiGzDecoder<R> {
    /// Returns the current header associated with this stream, if it's valid
    pub fn header(&self) -> Option<&GzHeader> {
        self.0.header()
    }

    /// Acquires a reference to the underlying reader.
    pub fn get_ref(&self) -> &R {
        self.0.get_ref()
    }

    /// Acquires a mutable reference to the underlying stream.
    ///
    /// Note that mutation of the stream may result in surprising results if
    /// this encoder is continued to be used.
    pub fn get_mut(&mut self) -> &mut R {
        self.0.get_mut()
    }

    /// Consumes this decoder, returning the underlying reader.
    pub fn into_inner(self) -> R {
        self.0.into_inner()
    }
}

impl<R: BufRead> Read for MultiGzDecoder<R> {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        self.0.read(into)
    }
}

#[cfg(feature = "tokio")]
impl<R: AsyncRead + BufRead> AsyncRead for MultiGzDecoder<R> {}

impl<R: BufRead + Write> Write for MultiGzDecoder<R> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.get_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.get_mut().flush()
    }
}

#[cfg(feature = "tokio")]
impl<R: AsyncWrite + BufRead> AsyncWrite for MultiGzDecoder<R> {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        self.get_mut().shutdown()
    }
}
