//! Bound framing before Multer buffers it, and prevent its eager stream drain
//! from collecting an always-ready upload. Multer still owns field parsing.
use std::{
    pin::Pin,
    task::{Context, Poll},
};

use axum::{
    RequestExt,
    body::Body,
    extract::{FromRequest, Multipart, Request},
    response::Response,
};
use bytes::Bytes;
use futures::Stream;

const FRAMING_LIMIT: usize = 8 * 1024;
const DELIVERY_LIMIT: usize = 64 * 1024;

pub(crate) struct BoundedMultipart(pub Multipart);

impl<S: Send + Sync> FromRequest<S> for BoundedMultipart {
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let boundary = request
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.len() <= 1024)
            .and_then(|value| multer::parse_boundary(value).ok())
            .filter(|boundary| {
                !boundary.is_empty() && boundary.len() <= 70 && !boundary.bytes().any(|byte| byte.is_ascii_control())
            })
            .ok_or_else(|| super::invalid("Expected multipart/form-data with a boundary of 1 to 70 bytes"))?;
        let (parts, body) = request.with_limited_body().into_parts();
        let body = Body::from_stream(FramedBody::new(body.into_data_stream(), &boundary));
        Multipart::from_request(Request::from_parts(parts, body), state)
            .await
            .map(Self)
            .map_err(|_| super::invalid("Expected multipart/form-data with file and purpose fields"))
    }
}

#[derive(Debug, thiserror::Error)]
enum FramingError {
    #[error("multipart framing exceeds 8 KiB")]
    Limit,
    #[error("malformed multipart boundary suffix")]
    Boundary,
}

pub(super) fn is_framing_limit(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut source = Some(error);
    while let Some(error) = source {
        if matches!(error.downcast_ref::<FramingError>(), Some(FramingError::Limit)) {
            return true;
        }
        source = error.source();
    }
    false
}

struct FramedBody<S> {
    stream: S,
    pending: Bytes,
    framing: Framing,
    yield_before_poll: bool,
    ended: bool,
}

impl<S> FramedBody<S> {
    fn new(stream: S, boundary: &str) -> Self {
        Self {
            stream,
            pending: Bytes::new(),
            framing: Framing::new(boundary),
            yield_before_poll: false,
            ended: false,
        }
    }
}

impl<S: Stream<Item = Result<Bytes, axum::Error>> + Unpin> Stream for FramedBody<S> {
    type Item = Result<Bytes, axum::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.ended {
            return Poll::Ready(None);
        }
        if self.yield_before_poll {
            self.yield_before_poll = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        if self.pending.is_empty() {
            match futures::ready!(Pin::new(&mut self.stream).poll_next(cx)) {
                Some(Ok(bytes)) => self.pending = bytes,
                other => {
                    self.ended = true;
                    return Poll::Ready(other);
                }
            }
        }
        let len = self.pending.len().min(DELIVERY_LIMIT);
        let bytes = self.pending.split_to(len);
        if let Err(error) = self.framing.check(&bytes) {
            self.ended = true;
            return Poll::Ready(Some(Err(axum::Error::new(error))));
        }
        // Multer drains until Pending before examining its buffer. A deliberate
        // yield after each bounded chunk also covers in-memory/always-ready bodies.
        self.yield_before_poll = true;
        Poll::Ready(Some(Ok(bytes)))
    }
}

#[derive(Clone, Copy)]
enum Stage {
    Preamble,
    Boundary(Tail),
    Headers,
    Data,
    Done,
}
#[derive(Clone, Copy)]
enum Tail {
    Start,
    Hyphen,
    Padding,
    Lf,
}

struct Framing {
    stage: Stage,
    count: usize,
    initial: Matcher,
    delimiter: Matcher,
    header_end: u32,
}

impl Framing {
    fn new(boundary: &str) -> Self {
        Self {
            stage: Stage::Preamble,
            count: 0,
            initial: Matcher::new(format!("--{boundary}")),
            delimiter: Matcher::new(format!("\r\n--{boundary}")),
            header_end: 0,
        }
    }

    fn check(&mut self, bytes: &[u8]) -> Result<(), FramingError> {
        for &byte in bytes {
            if !matches!(self.stage, Stage::Data | Stage::Done) {
                self.count += 1;
                if self.count > FRAMING_LIMIT {
                    return Err(FramingError::Limit);
                }
            }
            match self.stage {
                Stage::Preamble if self.initial.accept(byte) => self.boundary(),
                Stage::Preamble | Stage::Done => (),
                Stage::Data => {
                    if self.delimiter.accept(byte) {
                        self.boundary();
                    }
                }
                Stage::Headers => {
                    self.header_end = (self.header_end << 8) | u32::from(byte);
                    if self.header_end == 0x0d0a_0d0a {
                        self.stage = Stage::Data;
                        self.delimiter.matched = 0;
                    }
                }
                Stage::Boundary(tail) => self.tail(tail, byte)?,
            }
        }
        Ok(())
    }

    fn boundary(&mut self) {
        self.stage = Stage::Boundary(Tail::Start);
        self.count = 0;
    }

    fn tail(&mut self, tail: Tail, byte: u8) -> Result<(), FramingError> {
        self.stage = match (tail, byte) {
            (Tail::Start, b'-') => Stage::Boundary(Tail::Hyphen),
            (Tail::Hyphen, b'-') => Stage::Done,
            (Tail::Start | Tail::Padding, b' ' | b'\t') => Stage::Boundary(Tail::Padding),
            (Tail::Start | Tail::Padding, b'\r') => Stage::Boundary(Tail::Lf),
            (Tail::Lf, b'\n') => {
                self.count = 0;
                self.header_end = 0;
                Stage::Headers
            }
            _ => return Err(FramingError::Boundary),
        };
        Ok(())
    }
}

/// Incremental KMP matching keeps only a bounded prefix length across chunks.
struct Matcher {
    pattern: Vec<u8>,
    failure: Vec<usize>,
    matched: usize,
}
impl Matcher {
    fn new(pattern: String) -> Self {
        let pattern = pattern.into_bytes();
        let mut failure = vec![0; pattern.len()];
        let mut matched = 0;
        for index in 1..pattern.len() {
            while matched > 0 && pattern[index] != pattern[matched] {
                matched = failure[matched - 1];
            }
            if pattern[index] == pattern[matched] {
                matched += 1;
            }
            failure[index] = matched;
        }
        Self {
            pattern,
            failure,
            matched: 0,
        }
    }

    fn accept(&mut self, byte: u8) -> bool {
        while self.matched > 0 && byte != self.pattern[self.matched] {
            self.matched = self.failure[self.matched - 1];
        }
        if byte == self.pattern[self.matched] {
            self.matched += 1;
        }
        if self.matched == self.pattern.len() {
            self.matched = self.failure[self.matched - 1];
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{StreamExt as _, stream};

    #[tokio::test]
    async fn split_delimiters_padding_and_binary_prefixes_preserve_file_bytes() {
        let boundary = "abababab";
        let mut payload = (0..=255u8).collect::<Vec<_>>();
        payload.extend_from_slice(b"\r\n--abababax\r\r\n--abababa\x00--abababab\r\n--ababab");
        let mut wire = b"preamble\r\n--abababab \t\r\nContent-Disposition: form-data; name=\"file\"; filename=\"bytes.bin\"\r\n\r\n".to_vec();
        wire.extend_from_slice(&payload);
        wire.extend_from_slice(b"\r\n--abababab\t \r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nvision\r\n--abababab--\r\nepilogue");
        for split in [1, 2, 3, 7, 16, 64 * 1024] {
            let chunks: Vec<_> = wire
                .chunks(split)
                .map(|bytes| Ok(Bytes::copy_from_slice(bytes)))
                .collect();
            let mut multipart = multer::Multipart::new(FramedBody::new(stream::iter(chunks), boundary), boundary);
            let field = multipart.next_field().await.unwrap().unwrap();
            assert_eq!(field.name(), Some("file"));
            assert_eq!(field.bytes().await.unwrap().as_ref(), payload);
            let field = multipart.next_field().await.unwrap().unwrap();
            assert_eq!(field.name(), Some("purpose"));
            assert_eq!(field.text().await.unwrap(), "vision");
            assert!(multipart.next_field().await.unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn always_ready_input_cannot_be_drained_ahead_of_field_consumption() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let consumed = Arc::new(AtomicUsize::new(0));
        let counter = consumed.clone();
        let header = Bytes::from_static(
            b"--upload\r\nContent-Disposition: form-data; name=\"file\"; filename=\"ready.bin\"\r\n\r\n",
        );
        let input = std::iter::once(header)
            .chain((0..16).map(|_| Bytes::from(vec![b'z'; DELIVERY_LIMIT])))
            .chain(std::iter::once(Bytes::from_static(b"\r\n--upload--\r\n")));
        let input = stream::iter(input.map(Ok)).inspect(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let mut multipart = multer::Multipart::new(FramedBody::new(input, "upload"), "upload");
        let mut field = multipart.next_field().await.unwrap().unwrap();
        assert!(
            consumed.load(Ordering::SeqCst) <= 2,
            "Multer must not eagerly buffer an always-ready upload"
        );
        let mut total = 0;
        while let Some(bytes) = field.chunk().await.unwrap() {
            assert!(bytes.len() <= 2 * DELIVERY_LIMIT);
            assert!(bytes.iter().all(|byte| *byte == b'z'));
            total += bytes.len();
        }
        assert_eq!(total, 16 * DELIVERY_LIMIT);
    }

    #[test]
    fn large_input_frames_are_split_and_yield_between_deliveries() {
        let mut bytes = b"--upload\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\n".to_vec();
        bytes.resize(3 * DELIVERY_LIMIT, b'z');
        let mut body = FramedBody::new(stream::iter([Ok(Bytes::from(bytes))]), "upload");
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        for _ in 0..3 {
            let Poll::Ready(Some(Ok(chunk))) = Pin::new(&mut body).poll_next(&mut cx) else {
                panic!("expected data");
            };
            assert_eq!(chunk.len(), DELIVERY_LIMIT);
            assert!(Pin::new(&mut body).poll_next(&mut cx).is_pending());
        }
    }
}
