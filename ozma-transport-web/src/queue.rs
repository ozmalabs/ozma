// SPDX-License-Identifier: AGPL-3.0-only
//! Reliable-frame serialization shared by the browser transport.
//!
//! Every reliable channel shares one ordered byte stream. [`SendQueue`] owns
//! the single writer for that stream: encoding a payload and handing the whole
//! frame to the writer happen in one synchronous step, so frames from several
//! [`crate::BrowserSender`] clones can never interleave.
//!
//! The queue is deliberately independent of `web-sys`, so its ordering
//! behaviour is unit-tested on the host with an injectable [`FrameWriter`].

use ozma_client_core::framing::encode_reliable;
use ozma_protocol::ApiError;
use ozma_transport::ChannelId;

/// A sink that accepts one complete reliable frame at a time.
///
/// Implementations must write each frame atomically. The transport relies on
/// this so concurrent senders can never splice bytes from two frames.
pub trait FrameWriter {
    /// Failure returned when the sink can no longer accept frames.
    type Error;

    /// Write exactly one whole frame. Implementations must not split a frame
    /// across writes that another sender could interleave with.
    fn write_frame(&self, frame: Vec<u8>) -> Result<(), Self::Error>;
}

/// Why [`SendQueue::send`] failed.
#[derive(Debug)]
pub enum SendQueueError<E> {
    /// The payload could not be encoded as a reliable frame.
    Encode(ApiError),
    /// The underlying writer rejected the frame or is gone.
    Writer(E),
}

/// One ordered, serialized writer for reliable channel frames.
///
/// Clone the surrounding `Rc` rather than the queue itself so every sender
/// targets the same writer.
pub struct SendQueue<W: FrameWriter> {
    writer: W,
}

impl<W: FrameWriter> SendQueue<W> {
    /// Wrap a sink.
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Borrow the wrapped sink, mainly for tests and diagnostics.
    pub fn writer(&self) -> &W {
        &self.writer
    }

    /// Encode `payload` for `channel` and write exactly one whole frame.
    ///
    /// Encoding and the writer call are synchronous: callers must not hold the
    /// queue across an `.await`, which is why no lock is required.
    pub fn send(&self, channel: ChannelId, payload: &[u8]) -> Result<(), SendQueueError<W::Error>> {
        let frame = encode_reliable(channel, payload).map_err(SendQueueError::Encode)?;
        self.writer
            .write_frame(frame)
            .map_err(SendQueueError::Writer)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use ozma_client_core::framing::FrameDecoder;

    use super::*;

    #[derive(Default)]
    struct RecordingWriter {
        frames: RefCell<Vec<Vec<u8>>>,
    }

    impl FrameWriter for RecordingWriter {
        type Error = ();

        fn write_frame(&self, frame: Vec<u8>) -> Result<(), Self::Error> {
            self.frames.borrow_mut().push(frame);
            Ok(())
        }
    }

    #[test]
    fn interleaved_senders_preserve_whole_ordered_frames() {
        let queue = Rc::new(SendQueue::new(RecordingWriter::default()));
        let first = Rc::clone(&queue);
        let second = Rc::clone(&queue);

        // Two handles share one writer and alternate sends, the way cloned
        // BrowserSender handles do from separate tasks.
        first.send(ChannelId::CONTROL, b"hello").unwrap();
        second.send(ChannelId::INPUT, b"world").unwrap();
        first.send(ChannelId::CONTROL, b"again").unwrap();

        let frames = queue.writer().frames.borrow();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].len(), 5 + b"hello".len());
        assert_eq!(frames[1].len(), 5 + b"world".len());
        assert_eq!(frames[2].len(), 5 + b"again".len());

        let mut stream = Vec::new();
        for frame in frames.iter() {
            stream.extend_from_slice(frame);
        }
        let mut decoder = FrameDecoder::new(u16::MAX as usize);
        let decoded = decoder.push(&stream).unwrap();
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0].0, ChannelId::CONTROL);
        assert_eq!(decoded[0].1.as_ref(), b"hello");
        assert_eq!(decoded[1].0, ChannelId::INPUT);
        assert_eq!(decoded[1].1.as_ref(), b"world");
        assert_eq!(decoded[2].0, ChannelId::CONTROL);
        assert_eq!(decoded[2].1.as_ref(), b"again");
        assert_eq!(decoder.buffered(), 0);
    }

    struct GoneWriter;

    impl FrameWriter for GoneWriter {
        type Error = &'static str;

        fn write_frame(&self, _frame: Vec<u8>) -> Result<(), Self::Error> {
            Err("writer stopped")
        }
    }

    #[test]
    fn writer_failure_is_reported_as_a_writer_error() {
        let queue = SendQueue::new(GoneWriter);
        match queue.send(ChannelId::CONTROL, b"hello") {
            Err(SendQueueError::Writer("writer stopped")) => {}
            other => panic!("expected a writer error, got {other:?}"),
        }
    }
}
