use {client, server, frame, ConnectionError};
use proto::*;
use super::*;

use error::Reason::*;

use std::collections::VecDeque;
use std::marker::PhantomData;

#[derive(Debug)]
pub(super) struct Recv<B> {
    /// Maximum number of remote initiated streams
    max_streams: Option<usize>,

    /// Current number of remote initiated streams
    num_streams: usize,

    /// Initial window size of remote initiated streams
    init_window_sz: WindowSize,

    /// Connection level flow control governing received data
    flow: FlowControl,

    /// The lowest stream ID that is still idle
    next_stream_id: StreamId,

    /// The stream ID of the last processed stream
    last_processed_id: StreamId,

    /// Streams that have pending window updates
    pending_window_updates: store::Queue<B, stream::NextWindowUpdate>,

    /// New streams to be accepted
    pending_accept: store::Queue<B, stream::Next>,

    /// Holds frames that are waiting to be read
    buffer: Buffer<Bytes>,

    /// Refused StreamId, this represents a frame that must be sent out.
    refused: Option<StreamId>,

    _p: PhantomData<(B)>,
}

#[derive(Debug, Clone, Copy)]
struct Indices {
    head: store::Key,
    tail: store::Key,
}

impl<B> Recv<B> where B: Buf {
    pub fn new<P: Peer>(config: &Config) -> Self {
        let next_stream_id = if P::is_server() {
            1
        } else {
            2
        };

        let mut flow = FlowControl::new();

        flow.inc_window(config.init_remote_window_sz);
        flow.assign_capacity(config.init_remote_window_sz);

        Recv {
            max_streams: config.max_remote_initiated,
            num_streams: 0,
            init_window_sz: config.init_remote_window_sz,
            flow: flow,
            next_stream_id: next_stream_id.into(),
            pending_window_updates: store::Queue::new(),
            last_processed_id: StreamId::zero(),
            pending_accept: store::Queue::new(),
            buffer: Buffer::new(),
            refused: None,
            _p: PhantomData,
        }
    }

    /// Returns the ID of the last processed stream
    pub fn last_processed_id(&self) -> StreamId {
        self.last_processed_id
    }

    /// Update state reflecting a new, remotely opened stream
    ///
    /// Returns the stream state if successful. `None` if refused
    pub fn open<P: Peer>(&mut self, id: StreamId)
        -> Result<Option<Stream<B>>, ConnectionError>
    {
        assert!(self.refused.is_none());

        try!(self.ensure_can_open::<P>(id));

        if !self.can_inc_num_streams() {
            self.refused = Some(id);
            return Ok(None);
        }

        Ok(Some(Stream::new(id)))
    }

    pub fn take_request(&mut self, stream: &mut store::Ptr<B>)
        -> Result<Request<()>, ConnectionError>
    {
        match stream.pending_recv.pop_front(&mut self.buffer) {
            Some(Frame::Headers(frame)) => {
                // TODO: This error should probably be caught on receipt of the
                // frame vs. now.
                Ok(server::Peer::convert_poll_message(frame)?)
            }
            _ => panic!(),
        }
    }

    pub fn poll_response(&mut self, stream: &mut store::Ptr<B>)
        -> Poll<Response<()>, ConnectionError> {
        // If the buffer is not empty, then the first frame must be a HEADERS
        // frame or the user violated the contract.
        match stream.pending_recv.pop_front(&mut self.buffer) {
            Some(Frame::Headers(v)) => {
                // TODO: This error should probably be caught on receipt of the
                // frame vs. now.
                Ok(client::Peer::convert_poll_message(v)?.into())
            }
            Some(frame) => unimplemented!(),
            None => {
                stream.state.ensure_recv_open()?;

                stream.recv_task = Some(task::current());
                Ok(Async::NotReady)
            }
        }
    }

    /// Transition the stream state based on receiving headers
    pub fn recv_headers<P: Peer>(&mut self,
                                 frame: frame::Headers,
                                 stream: &mut store::Ptr<B>)
        -> Result<(), ConnectionError>
    {
        trace!("opening stream; init_window={}", self.init_window_sz);
        let is_initial = stream.state.recv_open(frame.is_end_stream())?;

        if stream.state.is_recv_streaming() {
            stream.recv_flow.inc_window(self.init_window_sz)?;
            stream.recv_flow.assign_capacity(self.init_window_sz);
        }

        if is_initial {
            if !self.can_inc_num_streams() {
                unimplemented!();
            }

            if frame.stream_id() >= self.next_stream_id {
                self.next_stream_id = frame.stream_id();
                self.next_stream_id.increment();
            } else {
                return Err(ProtocolError.into());
            }

            // TODO: be smarter about this logic
            if frame.stream_id() > self.last_processed_id {
                self.last_processed_id = frame.stream_id();
            }

            // Increment the number of concurrent streams
            self.inc_num_streams();
        }

        // Push the frame onto the stream's recv buffer
        stream.pending_recv.push_back(&mut self.buffer, frame.into());
        stream.notify_recv();

        // Only servers can receive a headers frame that initiates the stream.
        // This is verified in `Streams` before calling this function.
        if P::is_server() {
            self.pending_accept.push(stream);
        }

        Ok(())
    }

    /// Transition the stream based on receiving trailers
    pub fn recv_trailers<P: Peer>(&mut self,
                                  frame: frame::Headers,
                                  stream: &mut store::Ptr<B>)
        -> Result<(), ConnectionError>
    {
        // Transition the state
        stream.state.recv_close();

        // Push the frame onto the stream's recv buffer
        stream.pending_recv.push_back(&mut self.buffer, frame.into());
        stream.notify_recv();

        Ok(())
    }

    pub fn release_capacity(&mut self,
                            capacity: WindowSize,
                            stream: &mut store::Ptr<B>,
                            send: &mut Send<B>,
                            task: &mut Option<Task>)
        -> Result<(), ConnectionError>
    {
        if capacity > stream.in_flight_recv_data {
            // TODO: Handle error
            unimplemented!();
        }

        // Decrement in-flight data
        stream.in_flight_recv_data -= capacity;

        // Assign capacity to connection & stream
        self.flow.assign_capacity(capacity);
        stream.recv_flow.assign_capacity(capacity);

        // Queue the stream for sending the WINDOW_UPDATE frame.
        self.pending_window_updates.push(stream);

        if let Some(task) = task.take() {
            task.notify();
        }

        Ok(())
    }

    pub fn recv_data(&mut self,
                     frame: frame::Data,
                     stream: &mut store::Ptr<B>)
        -> Result<(), ConnectionError>
    {
        let sz = frame.payload().len();

        if sz > MAX_WINDOW_SIZE as usize {
            unimplemented!();
        }

        let sz = sz as WindowSize;

        if !stream.state.is_recv_streaming() {
            // Receiving a DATA frame when not expecting one is a protocol
            // error.
            return Err(ProtocolError.into());
        }

        trace!("recv_data; size={}; connection={}; stream={}",
               sz, self.flow.window_size(), stream.recv_flow.window_size());

        // Ensure that there is enough capacity on the connection before acting
        // on the stream.
        if self.flow.window_size() < sz || stream.recv_flow.window_size() < sz {
            return Err(FlowControlError.into());
        }

        // Update connection level flow control
        self.flow.send_data(sz);

        // Update stream level flow control
        stream.recv_flow.send_data(sz);

        // Track the data as in-flight
        stream.in_flight_recv_data += sz;

        if frame.is_end_stream() {
            try!(stream.state.recv_close());
        }

        // Push the frame onto the recv buffer
        stream.pending_recv.push_back(&mut self.buffer, frame.into());
        stream.notify_recv();

        Ok(())
    }

    pub fn recv_push_promise<P: Peer>(&mut self,
                                      frame: frame::PushPromise,
                                      stream: store::Key,
                                      store: &mut Store<B>)
        -> Result<(), ConnectionError>
    {
        // First, make sure that the values are legit
        self.ensure_can_reserve::<P>(frame.promised_id())?;

        // Make sure that the stream state is valid
        store[stream].state.ensure_recv_open()?;

        // TODO: Streams in the reserved states do not count towards the concurrency
        // limit. However, it seems like there should be a cap otherwise this
        // could grow in memory indefinitely.

        /*
        if !self.inc_num_streams() {
            self.refused = Some(frame.promised_id());
            return Ok(());
        }
        */

        // TODO: All earlier stream IDs should be implicitly closed.

        // Now, create a new entry for the stream
        let mut new_stream = Stream::new(frame.promised_id());
        new_stream.state.reserve_remote();

        let mut ppp = store[stream].pending_push_promises.take();

        {
            // Store the stream
            let mut new_stream = store
                .insert(frame.promised_id(), new_stream);

            ppp.push(&mut new_stream);
        }

        let stream = &mut store[stream];

        stream.pending_push_promises = ppp;
        stream.notify_recv();

        Ok(())
    }

    pub fn ensure_not_idle(&self, id: StreamId) -> Result<(), ConnectionError> {
        if id >= self.next_stream_id {
            return Err(ProtocolError.into());
        }

        Ok(())
    }

    pub fn recv_reset(&mut self, frame: frame::Reset, stream: &mut Stream<B>)
        -> Result<(), ConnectionError>
    {
        let err = ConnectionError::Proto(frame.reason());

        // Notify the stream
        stream.state.recv_err(&err);
        stream.notify_recv();
        Ok(())
    }

    /// Handle a received error
    pub fn recv_err(&mut self, err: &ConnectionError, stream: &mut Stream<B>) {
        // Receive an error
        stream.state.recv_err(err);

        // If a receiver is waiting, notify it
        stream.notify_recv();
    }

    /// Returns true if the current stream concurrency can be incremetned
    fn can_inc_num_streams(&self) -> bool {
        if let Some(max) = self.max_streams {
            max > self.num_streams
        } else {
            true
        }
    }

    /// Increments the number of concurrenty streams. Panics on failure as this
    /// should have been validated before hand.
    fn inc_num_streams(&mut self) {
        if !self.can_inc_num_streams() {
            panic!();
        }

        // Increment the number of remote initiated streams
        self.num_streams += 1;
    }

    pub fn dec_num_streams(&mut self) {
        self.num_streams -= 1;
    }

    /// Returns true if the remote peer can initiate a stream with the given ID.
    fn ensure_can_open<P: Peer>(&self, id: StreamId)
        -> Result<(), ConnectionError>
    {
        if !P::is_server() {
            // Remote is a server and cannot open streams. PushPromise is
            // registered by reserving, so does not go through this path.
            return Err(ProtocolError.into());
        }

        // Ensure that the ID is a valid server initiated ID
        if !id.is_client_initiated() {
            return Err(ProtocolError.into());
        }

        Ok(())
    }

    /// Returns true if the remote peer can reserve a stream with the given ID.
    fn ensure_can_reserve<P: Peer>(&self, promised_id: StreamId)
        -> Result<(), ConnectionError>
    {
        // TODO: Are there other rules?
        if P::is_server() {
            // The remote is a client and cannot reserve
            return Err(ProtocolError.into());
        }

        if !promised_id.is_server_initiated() {
            return Err(ProtocolError.into());
        }

        Ok(())
    }

    /// Send any pending refusals.
    pub fn send_pending_refusal<T>(&mut self, dst: &mut Codec<T, Prioritized<B>>)
        -> Poll<(), ConnectionError>
        where T: AsyncWrite,
    {
        if let Some(stream_id) = self.refused.take() {
            let frame = frame::Reset::new(stream_id, RefusedStream);

            match dst.start_send(frame.into())? {
                AsyncSink::Ready => {
                    self.reset(stream_id, RefusedStream);
                    return Ok(Async::Ready(()));
                }
                AsyncSink::NotReady(_) => {
                    self.refused = Some(stream_id);
                    return Ok(Async::NotReady);
                }
            }
        }

        Ok(Async::Ready(()))
    }

    pub fn poll_complete<T>(&mut self,
                            store: &mut Store<B>,
                            dst: &mut Codec<T, Prioritized<B>>)
        -> Poll<(), ConnectionError>
        where T: AsyncWrite,
    {
        // Send any pending connection level window updates
        try_ready!(self.send_connection_window_update(dst));

        // Send any pending stream level window updates
        try_ready!(self.send_stream_window_updates(store, dst));

        Ok(().into())
    }

    /// Send connection level window update
    fn send_connection_window_update<T>(&mut self, dst: &mut Codec<T, Prioritized<B>>)
        -> Poll<(), ConnectionError>
        where T: AsyncWrite,
    {
        let incr = self.flow.unclaimed_capacity();

        if incr > 0 {
            let frame = frame::WindowUpdate::new(StreamId::zero(), incr);

            if dst.start_send(frame.into())?.is_ready() {
                self.flow.inc_window(incr);
            } else {
                return Ok(Async::NotReady);
            }
        }

        Ok(().into())
    }


    /// Send stream level window update
    pub fn send_stream_window_updates<T>(&mut self,
                                         store: &mut Store<B>,
                                         dst: &mut Codec<T, Prioritized<B>>)
        -> Poll<(), ConnectionError>
        where T: AsyncWrite,
    {
        loop {
            // Ensure the codec has capacity
            try_ready!(dst.poll_ready());

            // Get the next stream
            let stream = match self.pending_window_updates.pop(store) {
                Some(stream) => stream,
                None => return Ok(().into()),
            };

            if !stream.state.is_recv_streaming() {
                // No need to send window updates on the stream if the stream is
                // no longer receiving data.
                continue;
            }

            // TODO: de-dup
            let incr = stream.recv_flow.unclaimed_capacity();

            if incr > 0 {
                let frame = frame::WindowUpdate::new(stream.id, incr);
                let res = dst.start_send(frame.into())?;

                assert!(res.is_ready());
            }
        }
    }

    pub fn next_incoming(&mut self, store: &mut Store<B>) -> Option<store::Key> {
        self.pending_accept.pop(store)
            .map(|ptr| ptr.key())
    }

    pub fn poll_data(&mut self, stream: &mut Stream<B>)
        -> Poll<Option<Bytes>, ConnectionError>
    {
        match stream.pending_recv.pop_front(&mut self.buffer) {
            Some(frame) => {
                match frame {
                    Frame::Data(frame) => {
                        Ok(Some(frame.into_payload()).into())
                    }
                    frame => {
                        // Frame is trailer
                        stream.pending_recv.push_front(&mut self.buffer, frame);

                        // No more data frames
                        Ok(None.into())
                    }
                }
            }
            None => {
                if stream.state.is_recv_closed() {
                    // No more data frames will be received
                    Ok(None.into())
                } else {
                    // Request to get notified once more data frames arrive
                    stream.recv_task = Some(task::current());
                    Ok(Async::NotReady)
                }
            }
        }
    }

    fn reset(&mut self, _stream_id: StreamId, _reason: Reason) {
        unimplemented!();
    }
}
