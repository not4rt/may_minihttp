use std::fmt;
use std::io::{self, BufRead, Read};
use std::mem::MaybeUninit;

pub(crate) const MAX_HEADERS: usize = 16;

use bytes::{Buf, BufMut, BytesMut};

use crate::http_server::err;

pub struct BodyReader<'buf, 'stream, S>
where
    S: Read,
{
    // remaining bytes for body
    req_buf: &'buf mut BytesMut,
    // the max body length limit
    body_limit: usize,
    // total read count
    total_read: usize,
    // used to read extra body bytes
    stream: &'stream mut S,
}

impl<S: Read> BodyReader<'_, '_, S> {
    fn read_more_data(&mut self) -> io::Result<usize> {
        crate::http_server::reserve_buf(self.req_buf);
        let read_buf: &mut [u8] = unsafe { std::mem::transmute(self.req_buf.chunk_mut()) };
        let n = self.stream.read(read_buf)?;
        unsafe { self.req_buf.advance_mut(n) };
        Ok(n)
    }
}

impl<S: Read> Read for BodyReader<'_, '_, S> {
    // the user should control the body reading, don't exceeds the body!
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.total_read >= self.body_limit {
            return Ok(0);
        }

        loop {
            if !self.req_buf.is_empty() {
                let min_len = buf.len().min(self.body_limit - self.total_read);
                let n = self.req_buf.reader().read(&mut buf[..min_len])?;
                self.total_read += n;
                return Ok(n);
            }

            if self.read_more_data()? == 0 {
                return Ok(0);
            }
        }
    }
}

impl<S: Read> BufRead for BodyReader<'_, '_, S> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        let remain = self.body_limit - self.total_read;
        if remain == 0 {
            return Ok(&[]);
        }
        if self.req_buf.is_empty() {
            self.read_more_data()?;
        }
        let n = self.req_buf.len().min(remain);
        Ok(&self.req_buf.chunk()[0..n])
    }

    fn consume(&mut self, amt: usize) {
        assert!(amt <= self.body_limit - self.total_read);
        assert!(amt <= self.req_buf.len());
        self.total_read += amt;
        self.req_buf.advance(amt);
    }
}

impl<S: Read> Drop for BodyReader<'_, '_, S> {
    fn drop(&mut self) {
        // consume all the remaining bytes
        while let Ok(n) = self.fill_buf().map(<[u8]>::len) {
            if n == 0 {
                break;
            }
            // println!("drop: {:?}", n);
            self.consume(n);
        }
    }
}

// we should hold the mut ref of req_buf
// before into body, this req_buf is only for holding headers
// after into body, this req_buf is mutable to read extra body bytes
// and the headers buf can be reused
pub struct Request<'buf, 'header, 'stream, S> {
    req: httparse::Request<'header, 'buf>,
    req_buf: &'buf mut BytesMut,
    stream: &'stream mut S,
}

impl<'buf, 'stream, S> Request<'buf, '_, 'stream, S>
where
    S: Read,
{
    #[must_use] pub fn method(&self) -> &str {
        self.req.method.unwrap()
    }

    #[must_use] pub fn path(&self) -> &str {
        self.req.path.unwrap()
    }

    #[must_use] pub fn version(&self) -> u8 {
        self.req.version.unwrap()
    }

    #[must_use] pub fn headers(&self) -> &[httparse::Header<'_>] {
        self.req.headers
    }

    #[must_use] pub fn body(self) -> BodyReader<'buf, 'stream, S> {
        BodyReader {
            body_limit: self.content_length(),
            total_read: 0,
            stream: self.stream,
            req_buf: self.req_buf,
        }
    }

    fn content_length(&self) -> usize {
        let mut len = 0;
        for header in self.req.headers.iter() {
            if header.name.eq_ignore_ascii_case("content-length") {
                len = std::str::from_utf8(header.value).unwrap().parse().unwrap();
                break;
            }
        }
        len
    }
}

impl<S: Read> fmt::Debug for Request<'_, '_, '_, S> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "<HTTP Request {} {}>", (*self).method(), (*self).path())
    }
}

pub fn decode<'header, 'buf, 'stream, S>(
    headers: &'header mut [MaybeUninit<httparse::Header<'buf>>; MAX_HEADERS],
    req_buf: &'buf mut BytesMut,
    stream: &'stream mut S,
) -> io::Result<Option<Request<'buf, 'header, 'stream, S>>> {
    let mut req = httparse::Request::new(&mut []);
    // safety: don't hold the reference of req_buf
    // so we can transfer the mutable reference to Request
    let buf: &[u8] = unsafe { std::mem::transmute(req_buf.chunk()) };
    let status = match req.parse_with_uninit_headers(buf, headers) {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("failed to parse http request: {e:?}");
            eprintln!("{msg}");
            return err(io::Error::other(msg));
        }
    };

    let len = match status {
        httparse::Status::Complete(amt) => amt,
        httparse::Status::Partial => return Ok(None),
    };
    req_buf.advance(len);

    // println!("req: {:?}", std::str::from_utf8(req_buf).unwrap());
    Ok(Some(Request {
        req,
        req_buf,
        stream,
    }))
}
