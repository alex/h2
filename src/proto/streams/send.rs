use super::{
    store, Buffer, Codec, Config, Counts, Frame, Prioritize, Prioritized, Store, Stream, StreamId,
    StreamIdOverflow, WindowSize,
};
use crate::codec::{RecvError, UserError};
use crate::frame::{self, Reason};

use bytes::Buf;
use http;
use std::task::{Context, Poll, Waker};
use tokio::io::AsyncWrite;

use std::io;

/// Manages state transitions related to outbound frames.
#[derive(Debug)]
pub(super) struct Send {
    /// Stream identifier to use for next initialized stream.
    next_stream_id: Result<StreamId, StreamIdOverflow>,

    /// Initial window size of locally initiated streams
    init_window_sz: WindowSize,

    /// Prioritization layer
    prioritize: Prioritize,
}

/// A value to detect which public API has called `poll_reset`.
#[derive(Debug)]
pub(crate) enum PollReset {
    AwaitingHeaders,
    Streaming,
}

impl Send {
    /// Create a new `Send`
    pub fn new(config: &Config) -> Self {
        Send {
            init_window_sz: config.remote_init_window_sz,
            next_stream_id: Ok(config.local_next_stream_id),
            prioritize: Prioritize::new(config),
        }
    }

    /// Returns the initial send window size
    pub fn init_window_sz(&self) -> WindowSize {
        self.init_window_sz
    }

    pub fn open(&mut self) -> Result<StreamId, UserError> {
        let stream_id = self.ensure_next_stream_id()?;
        self.next_stream_id = stream_id.next_id();
        Ok(stream_id)
    }

    pub fn reserve_local(&mut self) -> Result<StreamId, UserError> {
        let stream_id = self.ensure_next_stream_id()?;
        self.next_stream_id = stream_id.next_id();
        Ok(stream_id)
    }

    fn check_headers(fields: &http::HeaderMap) -> Result<(), UserError> {
        // 8.1.2.2. Connection-Specific Header Fields
        if fields.contains_key(http::header::CONNECTION)
            || fields.contains_key(http::header::TRANSFER_ENCODING)
            || fields.contains_key(http::header::UPGRADE)
            || fields.contains_key("keep-alive")
            || fields.contains_key("proxy-connection")
        {
            log::debug!("illegal connection-specific headers found");
            return Err(UserError::MalformedHeaders);
        } else if let Some(te) = fields.get(http::header::TE) {
            if te != "trailers" {
                log::debug!("illegal connection-specific headers found");
                return Err(UserError::MalformedHeaders);
            }
        }
        Ok(())
    }

    pub fn send_push_promise<B>(
        &mut self,
        frame: frame::PushPromise,
        buffer: &mut Buffer<Frame<B>>,
        stream: &mut store::Ptr,
        task: &mut Option<Waker>,
    ) -> Result<(), UserError> {
        log::trace!(
            "send_push_promise; frame={:?}; init_window={:?}",
            frame,
            self.init_window_sz
        );

        Self::check_headers(frame.fields())?;

        // Queue the frame for sending
        self.prioritize
            .queue_frame(frame.into(), buffer, stream, task);

        Ok(())
    }

    pub fn send_headers<B>(
        &mut self,
        frame: frame::Headers,
        buffer: &mut Buffer<Frame<B>>,
        stream: &mut store::Ptr,
        counts: &mut Counts,
        task: &mut Option<Waker>,
    ) -> Result<(), UserError> {
        log::trace!(
            "send_headers; frame={:?}; init_window={:?}",
            frame,
            self.init_window_sz
        );

        Self::check_headers(frame.fields())?;

        if frame.has_too_big_field() {
            return Err(UserError::HeaderTooBig);
        }

        let end_stream = frame.is_end_stream();

        // Update the state
        stream.state.send_open(end_stream)?;

        if counts.peer().is_local_init(frame.stream_id()) {
            // If we're waiting on a PushPromise anyway
            // handle potentially queueing the stream at that point
            if !stream.is_pending_push {
                if counts.can_inc_num_send_streams() {
                    counts.inc_num_send_streams(stream);
                } else {
                    self.prioritize.queue_open(stream);
                }
            }
        }

        // Queue the frame for sending
        self.prioritize
            .queue_frame(frame.into(), buffer, stream, task);

        Ok(())
    }

    /// Send an explicit RST_STREAM frame
    pub fn send_reset<B>(
        &mut self,
        reason: Reason,
        buffer: &mut Buffer<Frame<B>>,
        stream: &mut store::Ptr,
        counts: &mut Counts,
        task: &mut Option<Waker>,
    ) {
        let is_reset = stream.state.is_reset();
        let is_closed = stream.state.is_closed();
        let is_empty = stream.pending_send.is_empty();

        log::trace!(
            "send_reset(..., reason={:?}, stream={:?}, ..., \
             is_reset={:?}; is_closed={:?}; pending_send.is_empty={:?}; \
             state={:?} \
             ",
            reason,
            stream.id,
            is_reset,
            is_closed,
            is_empty,
            stream.state
        );

        if is_reset {
            // Don't double reset
            log::trace!(
                " -> not sending RST_STREAM ({:?} is already reset)",
                stream.id
            );
            return;
        }

        // Transition the state to reset no matter what.
        stream.state.set_reset(reason);

        // If closed AND the send queue is flushed, then the stream cannot be
        // reset explicitly, either. Implicit resets can still be queued.
        if is_closed && is_empty {
            log::trace!(
                " -> not sending explicit RST_STREAM ({:?} was closed \
                 and send queue was flushed)",
                stream.id
            );
            return;
        }

        // Clear all pending outbound frames.
        // Note that we don't call `self.recv_err` because we want to enqueue
        // the reset frame before transitioning the stream inside
        // `reclaim_all_capacity`.
        self.prioritize.clear_queue(buffer, stream);

        let frame = frame::Reset::new(stream.id, reason);

        log::trace!("send_reset -- queueing; frame={:?}", frame);
        self.prioritize
            .queue_frame(frame.into(), buffer, stream, task);
        self.prioritize.reclaim_all_capacity(stream, counts);
    }

    pub fn schedule_implicit_reset(
        &mut self,
        stream: &mut store::Ptr,
        reason: Reason,
        counts: &mut Counts,
        task: &mut Option<Waker>,
    ) {
        if stream.state.is_closed() {
            // Stream is already closed, nothing more to do
            return;
        }

        stream.state.set_scheduled_reset(reason);

        self.prioritize.reclaim_reserved_capacity(stream, counts);
        self.prioritize.schedule_send(stream, task);
    }

    pub fn send_data<B>(
        &mut self,
        frame: frame::Data<B>,
        buffer: &mut Buffer<Frame<B>>,
        stream: &mut store::Ptr,
        counts: &mut Counts,
        task: &mut Option<Waker>,
    ) -> Result<(), UserError>
    where
        B: Buf,
    {
        self.prioritize
            .send_data(frame, buffer, stream, counts, task)
    }

    pub fn send_trailers<B>(
        &mut self,
        frame: frame::Headers,
        buffer: &mut Buffer<Frame<B>>,
        stream: &mut store::Ptr,
        counts: &mut Counts,
        task: &mut Option<Waker>,
    ) -> Result<(), UserError> {
        // TODO: Should this logic be moved into state.rs?
        if !stream.state.is_send_streaming() {
            return Err(UserError::UnexpectedFrameType);
        }

        if frame.has_too_big_field() {
            return Err(UserError::HeaderTooBig);
        }

        stream.state.send_close();

        log::trace!("send_trailers -- queuing; frame={:?}", frame);
        self.prioritize
            .queue_frame(frame.into(), buffer, stream, task);

        // Release any excess capacity
        self.prioritize.reserve_capacity(0, stream, counts);

        Ok(())
    }

    pub fn poll_complete<T, B>(
        &mut self,
        cx: &mut Context,
        buffer: &mut Buffer<Frame<B>>,
        store: &mut Store,
        counts: &mut Counts,
        dst: &mut Codec<T, Prioritized<B>>,
    ) -> Poll<io::Result<()>>
    where
        T: AsyncWrite + Unpin,
        B: Buf + Unpin,
    {
        self.prioritize
            .poll_complete(cx, buffer, store, counts, dst)
    }

    /// Request capacity to send data
    pub fn reserve_capacity(
        &mut self,
        capacity: WindowSize,
        stream: &mut store::Ptr,
        counts: &mut Counts,
    ) {
        self.prioritize.reserve_capacity(capacity, stream, counts)
    }

    pub fn poll_capacity(
        &mut self,
        cx: &Context,
        stream: &mut store::Ptr,
    ) -> Poll<Option<Result<WindowSize, UserError>>> {
        if !stream.state.is_send_streaming() {
            return Poll::Ready(None);
        }

        if !stream.send_capacity_inc {
            stream.wait_send(cx);
            return Poll::Pending;
        }

        stream.send_capacity_inc = false;

        Poll::Ready(Some(Ok(self.capacity(stream))))
    }

    /// Current available stream send capacity
    pub fn capacity(&self, stream: &mut store::Ptr) -> WindowSize {
        let available = stream.send_flow.available().as_size();
        let buffered = stream.buffered_send_data;

        if available <= buffered {
            0
        } else {
            available - buffered
        }
    }

    pub fn poll_reset(
        &self,
        cx: &Context,
        stream: &mut Stream,
        mode: PollReset,
    ) -> Poll<Result<Reason, crate::Error>> {
        match stream.state.ensure_reason(mode)? {
            Some(reason) => Poll::Ready(Ok(reason)),
            None => {
                stream.wait_send(cx);
                Poll::Pending
            }
        }
    }

    pub fn recv_connection_window_update(
        &mut self,
        frame: frame::WindowUpdate,
        store: &mut Store,
        counts: &mut Counts,
    ) -> Result<(), Reason> {
        self.prioritize
            .recv_connection_window_update(frame.size_increment(), store, counts)
    }

    pub fn recv_stream_window_update<B>(
        &mut self,
        sz: WindowSize,
        buffer: &mut Buffer<Frame<B>>,
        stream: &mut store::Ptr,
        counts: &mut Counts,
        task: &mut Option<Waker>,
    ) -> Result<(), Reason> {
        if let Err(e) = self.prioritize.recv_stream_window_update(sz, stream) {
            log::debug!("recv_stream_window_update !!; err={:?}", e);

            self.send_reset(Reason::FLOW_CONTROL_ERROR, buffer, stream, counts, task);

            return Err(e);
        }

        Ok(())
    }

    pub fn recv_err<B>(
        &mut self,
        buffer: &mut Buffer<Frame<B>>,
        stream: &mut store::Ptr,
        counts: &mut Counts,
    ) {
        // Clear all pending outbound frames
        self.prioritize.clear_queue(buffer, stream);
        self.prioritize.reclaim_all_capacity(stream, counts);
    }

    pub fn apply_remote_settings<B>(
        &mut self,
        settings: &frame::Settings,
        buffer: &mut Buffer<Frame<B>>,
        store: &mut Store,
        counts: &mut Counts,
        task: &mut Option<Waker>,
    ) -> Result<(), RecvError> {
        // Applies an update to the remote endpoint's initial window size.
        //
        // Per RFC 7540 §6.9.2:
        //
        // In addition to changing the flow-control window for streams that are
        // not yet active, a SETTINGS frame can alter the initial flow-control
        // window size for streams with active flow-control windows (that is,
        // streams in the "open" or "half-closed (remote)" state). When the
        // value of SETTINGS_INITIAL_WINDOW_SIZE changes, a receiver MUST adjust
        // the size of all stream flow-control windows that it maintains by the
        // difference between the new value and the old value.
        //
        // A change to `SETTINGS_INITIAL_WINDOW_SIZE` can cause the available
        // space in a flow-control window to become negative. A sender MUST
        // track the negative flow-control window and MUST NOT send new
        // flow-controlled frames until it receives WINDOW_UPDATE frames that
        // cause the flow-control window to become positive.
        if let Some(val) = settings.initial_window_size() {
            let old_val = self.init_window_sz;
            self.init_window_sz = val;

            if val < old_val {
                // We must decrease the (remote) window on every open stream.
                let dec = old_val - val;
                log::trace!("decrementing all windows; dec={}", dec);

                let mut total_reclaimed = 0;
                store.for_each(|mut stream| {
                    let stream = &mut *stream;

                    stream.send_flow.dec_send_window(dec);

                    // It's possible that decreasing the window causes
                    // `window_size` (the stream-specific window) to fall below
                    // `available` (the portion of the connection-level window
                    // that we have allocated to the stream).
                    // In this case, we should take that excess allocation away
                    // and reassign it to other streams.
                    let window_size = stream.send_flow.window_size();
                    let available = stream.send_flow.available().as_size();
                    let reclaimed = if available > window_size {
                        // Drop down to `window_size`.
                        let reclaim = available - window_size;
                        stream.send_flow.claim_capacity(reclaim);
                        total_reclaimed += reclaim;
                        reclaim
                    } else {
                        0
                    };

                    log::trace!(
                        "decremented stream window; id={:?}; decr={}; reclaimed={}; flow={:?}",
                        stream.id,
                        dec,
                        reclaimed,
                        stream.send_flow
                    );

                    // TODO: Should this notify the producer when the capacity
                    // of a stream is reduced? Maybe it should if the capacity
                    // is reduced to zero, allowing the producer to stop work.

                    Ok::<_, RecvError>(())
                })?;

                self.prioritize
                    .assign_connection_capacity(total_reclaimed, store, counts);
            } else if val > old_val {
                let inc = val - old_val;

                store.for_each(|mut stream| {
                    self.recv_stream_window_update(inc, buffer, &mut stream, counts, task)
                        .map_err(RecvError::Connection)
                })?;
            }
        }

        Ok(())
    }

    pub fn clear_queues(&mut self, store: &mut Store, counts: &mut Counts) {
        self.prioritize.clear_pending_capacity(store, counts);
        self.prioritize.clear_pending_send(store, counts);
        self.prioritize.clear_pending_open(store, counts);
    }

    pub fn ensure_not_idle(&self, id: StreamId) -> Result<(), Reason> {
        if let Ok(next) = self.next_stream_id {
            if id >= next {
                return Err(Reason::PROTOCOL_ERROR);
            }
        }
        // if next_stream_id is overflowed, that's ok.

        Ok(())
    }

    pub fn ensure_next_stream_id(&self) -> Result<StreamId, UserError> {
        self.next_stream_id
            .map_err(|_| UserError::OverflowedStreamId)
    }

    pub fn may_have_created_stream(&self, id: StreamId) -> bool {
        if let Ok(next_id) = self.next_stream_id {
            // Peer::is_local_init should have been called beforehand
            debug_assert_eq!(id.is_server_initiated(), next_id.is_server_initiated(),);
            id < next_id
        } else {
            true
        }
    }
}
