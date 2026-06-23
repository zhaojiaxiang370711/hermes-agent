//! Shared SSE line splitter for provider streaming.
//!
//! HTTP server-sent events arrive as a byte stream; events are separated by
//! blank lines and each event's payload is on a `data:` line. For OpenAI /
//! Anthropic chat streaming every event has exactly one `data:` line holding a
//! JSON object, so we yield each complete `data:` payload as a `String` the
//! moment its line is fully received — letting each provider own only the
//! per-event interpretation and termination rule.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;

use crate::ProviderError;

pub(super) struct SseLineStream<S> {
    inner: S,
    buf: String,
}

impl<S> SseLineStream<S> {
    pub(super) fn new(inner: S) -> Self {
        Self {
            inner,
            buf: String::new(),
        }
    }
}

impl<S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin> Stream for SseLineStream<S> {
    type Item = Result<String, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // Yield any complete `data:` line already buffered.
            if let Some(nl) = self.buf.find('\n') {
                let line: String = self.buf.drain(..=nl).collect();
                if let Some(payload) = parse_data_line(&line) {
                    return Poll::Ready(Some(Ok(payload)));
                }
                continue;
            }
            // Need more bytes from the wire.
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(chunk))) => self.buf.push_str(&String::from_utf8_lossy(&chunk)),
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(ProviderError::Http(e)))),
                Poll::Ready(None) => {
                    // Flush a trailing line not terminated by a newline.
                    if !self.buf.is_empty() {
                        let line = std::mem::take(&mut self.buf);
                        if let Some(payload) = parse_data_line(&line) {
                            return Poll::Ready(Some(Ok(payload)));
                        }
                    }
                    return Poll::Ready(None);
                }
            }
        }
    }
}

/// If `line` is a `data:` SSE field, return its trimmed payload; otherwise None
/// (blank separators, `event:`/`id:`/`retry:` fields, comments). The line still
/// carries its trailing newline (drained inclusive), so trim both ends.
fn parse_data_line(line: &str) -> Option<String> {
    let payload = line.strip_prefix("data:")?.trim();
    if payload.is_empty() {
        None
    } else {
        Some(payload.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use futures::StreamExt;

    async fn collect<S: Stream<Item = Result<String, ProviderError>> + Unpin>(s: S) -> Vec<String> {
        let mut out = Vec::new();
        let mut s = s;
        while let Some(item) = s.next().await {
            out.push(item.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn splits_data_lines_across_chunks() {
        // Bytes arrive split mid-line and mid-event; only complete `data:`
        // payloads surface, blanks/other fields are dropped.
        let chunks: Vec<Result<Bytes, reqwest::Error>> = vec![
            Ok(Bytes::from_static(b": heartbeat\n")),
            Ok(Bytes::from_static(b"data: {\"a\":1}\n\n")),
            Ok(Bytes::from_static(b"dat")),
            Ok(Bytes::from_static(b"a: {\"b\":2}\n")),
            Ok(Bytes::from_static(b"data: [DONE]\n")),
        ];
        let lines = SseLineStream::new(stream::iter(chunks));
        let got = collect(lines).await;
        assert_eq!(got, vec!["{\"a\":1}", "{\"b\":2}", "[DONE]"]);
    }

    #[tokio::test]
    async fn flushes_trailing_line_without_newline() {
        let chunks: Vec<Result<Bytes, reqwest::Error>> =
            vec![Ok(Bytes::from_static(b"data: tail-no-newline"))];
        let lines = SseLineStream::new(stream::iter(chunks));
        let got = collect(lines).await;
        assert_eq!(got, vec!["tail-no-newline"]);
    }
}
