//! What the web build has for a TCP connection: a byte stream over a
//! WebSocket or a WebRTC data channel. The page hands over what arrives as
//! events, so reads never block: they take what has arrived, and
//! [`Socket::readable`] waits for more. [`Socket::message`] reads the same
//! arrivals message by message instead, for a relay's signalling.
//!
//! The browser's objects live in this thread's registry and the socket holds
//! their id, so it stays `Send` like the `TcpStream` it stands in for.
use crate::time::Duration;
use js_sys::{ArrayBuffer, Function, Uint8Array};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    future::poll_fn,
    io,
    sync::{Arc, Mutex},
    task::{Poll, Waker},
};
use wasm_bindgen::{JsCast, prelude::*};

#[wasm_bindgen]
extern "C" {
    type WebSocket;
    #[wasm_bindgen(constructor, catch)]
    fn new(url: &str) -> Result<WebSocket, JsValue>;

    /// A WebSocket or an RTCDataChannel: both carry messages the same way.
    type Transport;
    #[wasm_bindgen(method, setter = binaryType)]
    fn set_binary_type(this: &Transport, kind: &str);
    #[wasm_bindgen(method, getter = readyState)]
    fn ready_state(this: &Transport) -> JsValue;
    #[wasm_bindgen(method, getter = bufferedAmount)]
    fn buffered_amount(this: &Transport) -> f64;
    #[wasm_bindgen(method, setter = bufferedAmountLowThreshold)]
    fn set_buffered_amount_low_threshold(this: &Transport, bytes: f64);
    #[wasm_bindgen(method, setter = onopen)]
    fn set_onopen(this: &Transport, handler: Option<&Function>);
    #[wasm_bindgen(method, setter = onmessage)]
    fn set_onmessage(this: &Transport, handler: Option<&Function>);
    #[wasm_bindgen(method, setter = onclose)]
    fn set_onclose(this: &Transport, handler: Option<&Function>);
    #[wasm_bindgen(method, setter = onerror)]
    fn set_onerror(this: &Transport, handler: Option<&Function>);
    #[wasm_bindgen(method, setter = onbufferedamountlow)]
    fn set_onbufferedamountlow(this: &Transport, handler: Option<&Function>);
    #[wasm_bindgen(method, catch)]
    fn send(this: &Transport, data: &Uint8Array) -> Result<(), JsValue>;
    /// Also an RTCPeerConnection's.
    #[wasm_bindgen(method)]
    fn close(this: &Transport);

    /// A MessageEvent or a CloseEvent.
    type Event;
    #[wasm_bindgen(method, getter)]
    fn data(this: &Event) -> JsValue;
    #[wasm_bindgen(method, getter)]
    fn reason(this: &Event) -> JsValue;

    #[wasm_bindgen(js_name = setTimeout)]
    fn set_timeout(handler: &Function, ms: f64) -> JsValue;
    #[wasm_bindgen(js_name = clearTimeout)]
    fn clear_timeout(id: &JsValue);
}

/// The largest message sent: data channels refuse ones over their peer's
/// limit (256 KiB in Chrome), and a byte stream can be cut anywhere.
const MAX_MESSAGE: usize = 64 << 10;
/// A data channel holds this much before writes wait in [`Entry::queued`],
/// which drains as its buffer falls to [`LOW_WATER`].
const HIGH_WATER: f64 = 4.0 * 1024.0 * 1024.0;
const LOW_WATER: f64 = 1024.0 * 1024.0;

/// What the page has said about one socket.
#[derive(Default)]
struct State {
    /// The messages that arrived; the first may be partly read.
    received: VecDeque<Vec<u8>>,
    read_at: usize,
    open: bool,
    closed: bool,
    /// What the other end said as it closed, or why it could not open.
    reason: Option<String>,
    /// The task waiting for it to open or for bytes to arrive.
    waker: Option<Waker>,
}
impl State {
    fn wake(&mut self) {
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

/// A browser object and the handlers it calls.
struct Entry {
    transport: Transport,
    /// What closes with it: a data channel's peer connection.
    keep: Option<Transport>,
    /// A data channel's writes held back while its buffer is full, in order.
    queued: VecDeque<Uint8Array>,
    flow_control: bool,
    _handlers: Vec<Closure<dyn FnMut(Event)>>,
}
impl Entry {
    /// Send what was held back while the buffer has room.
    fn drain(&mut self) {
        while self.transport.buffered_amount() < HIGH_WATER
            && let Some(message) = self.queued.pop_front()
        {
            if self.transport.send(&message).is_err() {
                self.queued.clear();
            }
        }
    }
}

thread_local! {
    static SOCKETS: RefCell<HashMap<u32, Entry>> = RefCell::new(HashMap::new());
    static NEXT: Cell<u32> = const { Cell::new(1) };
}

/// The last clone of a socket closes it.
struct Handle {
    id: u32,
    state: Arc<Mutex<State>>,
}
impl Drop for Handle {
    fn drop(&mut self) {
        if let Some(entry) = SOCKETS.with(|s| s.borrow_mut().remove(&self.id)) {
            let t = &entry.transport;
            t.set_onopen(None);
            t.set_onmessage(None);
            t.set_onclose(None);
            t.set_onerror(None);
            t.set_onbufferedamountlow(None);
            t.close();
            if let Some(keep) = &entry.keep {
                keep.close();
            }
        }
    }
}

/// A connected byte stream; its clones are the same stream.
#[derive(Clone)]
pub struct Socket(Arc<Handle>);
impl Socket {
    /// Opens a WebSocket to `url` and waits until it is open, or for
    /// `timeout`. The error says why it could not open.
    pub async fn connect(url: &str, timeout: Duration) -> Result<Self, String> {
        let ws = WebSocket::new(url).map_err(|e| {
            e.dyn_ref::<js_sys::Error>()
                .map_or_else(|| format!("{e:?}"), |e| String::from(e.message()))
        })?;
        let socket = Self::over(ws.unchecked_into(), None);
        socket.opened(timeout).await?;
        Ok(socket)
    }

    /// A socket over an RTCDataChannel that is opening or open, closing
    /// `peer` (its RTCPeerConnection) when it closes.
    pub fn adopt(channel: JsValue, peer: JsValue) -> Self {
        let socket = Self::over(channel.unchecked_into(), Some(peer.unchecked_into()));
        SOCKETS.with(|s| {
            let mut sockets = s.borrow_mut();
            let entry = sockets.get_mut(&socket.0.id).expect("just registered");
            entry.flow_control = true;
            entry.transport.set_buffered_amount_low_threshold(LOW_WATER);
            if entry.transport.ready_state().as_string().as_deref() == Some("open") {
                socket.0.state.lock().unwrap().open = true;
            }
        });
        socket
    }

    /// Waits until it is open, or for `timeout`; the error says why not.
    pub async fn opened(&self, timeout: Duration) -> Result<(), String> {
        let state = self.0.state.clone();
        let expire = Closure::<dyn FnMut()>::new(move || {
            let mut state = state.lock().unwrap();
            if !state.open && !state.closed {
                state.closed = true;
                state.reason = Some("Timed out".to_owned());
                state.wake();
            }
        });
        let timer = set_timeout(
            expire.as_ref().unchecked_ref(),
            timeout.as_secs_f64() * 1000.0,
        );
        let opened = poll_fn(|cx| {
            let mut state = self.0.state.lock().unwrap();
            if state.open {
                Poll::Ready(Ok(()))
            } else if state.closed {
                let reason = state.reason.as_deref().unwrap_or("Could not connect");
                Poll::Ready(Err(reason.to_owned()))
            } else {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await;
        clear_timeout(&timer);
        opened
    }

    fn over(transport: Transport, keep: Option<Transport>) -> Self {
        let id = NEXT.with(|n| n.replace(n.get() + 1));
        let state = Arc::new(Mutex::new(State::default()));
        let open = handler(&state, |state, _| state.open = true);
        let message = handler(&state, |state, event| {
            let data = event.data();
            if let Some(data) = data.dyn_ref::<ArrayBuffer>() {
                state.received.push_back(Uint8Array::new(data).to_vec());
            } else if let Some(text) = data.as_string() {
                state.received.push_back(text.into_bytes());
            }
        });
        // Browsers keep why a connection failed to themselves: only a reason
        // the other end gave as it closed comes through.
        let close = handler(&state, |state, event| {
            state.closed = true;
            state.reason = event.reason().as_string().filter(|r| !r.is_empty());
        });
        let low = Closure::<dyn FnMut(Event)>::new(move |_| {
            SOCKETS.with(|s| {
                if let Some(entry) = s.borrow_mut().get_mut(&id) {
                    entry.drain();
                }
            })
        });
        transport.set_binary_type("arraybuffer");
        transport.set_onopen(Some(open.as_ref().unchecked_ref()));
        transport.set_onmessage(Some(message.as_ref().unchecked_ref()));
        transport.set_onclose(Some(close.as_ref().unchecked_ref()));
        transport.set_onbufferedamountlow(Some(low.as_ref().unchecked_ref()));
        SOCKETS.with(|s| {
            s.borrow_mut().insert(
                id,
                Entry {
                    transport,
                    keep,
                    queued: VecDeque::new(),
                    flow_control: false,
                    _handlers: vec![open, message, close, low],
                },
            )
        });
        Self(Arc::new(Handle { id, state }))
    }

    /// What has arrived, up to `out.len()` bytes. `WouldBlock` while nothing
    /// has; once the socket closed and all was read, `Ok(0)`, or the reason
    /// the other end gave.
    pub fn read(&self, out: &mut [u8]) -> io::Result<usize> {
        let mut state = self.0.state.lock().unwrap();
        let mut n = 0;
        while n < out.len() {
            let at = state.read_at;
            let Some(front) = state.received.front() else {
                break;
            };
            let take = (front.len() - at).min(out.len() - n);
            out[n..n + take].copy_from_slice(&front[at..at + take]);
            n += take;
            if at + take == front.len() {
                state.received.pop_front();
                state.read_at = 0;
            } else {
                state.read_at = at + take;
            }
        }
        if n > 0 {
            return Ok(n);
        }
        match (state.closed, &state.reason) {
            (false, _) => Err(io::ErrorKind::WouldBlock.into()),
            (true, None) => Ok(0),
            (true, Some(reason)) => Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                reason.clone(),
            )),
        }
    }
    /// Waits until a read has something to say: bytes or the end.
    pub async fn readable(&self) {
        poll_fn(|cx| {
            let mut state = self.0.state.lock().unwrap();
            if !state.received.is_empty() || state.closed {
                Poll::Ready(())
            } else {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }
    /// The next whole message (what is left of it, if it was partly read);
    /// None once the socket closed or `timeout` passed.
    pub async fn message(&self, timeout: Duration) -> Option<Vec<u8>> {
        let state = self.0.state.clone();
        let expire = Closure::<dyn FnMut()>::new(move || state.lock().unwrap().wake());
        let timer = set_timeout(
            expire.as_ref().unchecked_ref(),
            timeout.as_secs_f64() * 1000.0,
        );
        let deadline = crate::time::Instant::now() + timeout;
        let message = poll_fn(|cx| {
            let mut state = self.0.state.lock().unwrap();
            let at = std::mem::take(&mut state.read_at);
            if let Some(mut message) = state.received.pop_front() {
                message.drain(..at);
                Poll::Ready(Some(message))
            } else if state.closed || crate::time::Instant::now() >= deadline {
                Poll::Ready(None)
            } else {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await;
        clear_timeout(&timer);
        message
    }
    /// Sends `data`, in messages of at most [`MAX_MESSAGE`] bytes. A data
    /// channel whose buffer is full keeps the rest until it drains.
    pub fn write(&self, data: &[u8]) -> io::Result<usize> {
        let state = self.0.state.lock().unwrap();
        if !state.open || state.closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        drop(state);
        SOCKETS.with(|s| {
            let mut sockets = s.borrow_mut();
            let Some(entry) = sockets.get_mut(&self.0.id) else {
                return Err(io::ErrorKind::BrokenPipe.into());
            };
            for piece in data.chunks(MAX_MESSAGE) {
                let message = Uint8Array::from(piece);
                if entry.flow_control
                    && (!entry.queued.is_empty() || entry.transport.buffered_amount() >= HIGH_WATER)
                {
                    entry.queued.push_back(message);
                } else if entry.transport.send(&message).is_err() {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
            }
            Ok(data.len())
        })
    }
    /// Closes it: reads then see the end once what arrived is read.
    pub fn close(&self) {
        let mut state = self.0.state.lock().unwrap();
        state.closed = true;
        state.wake();
        drop(state);
        SOCKETS.with(|s| {
            if let Some(entry) = s.borrow().get(&self.0.id) {
                entry.transport.close();
            }
        });
    }
    pub fn is_closed(&self) -> bool {
        self.0.state.lock().unwrap().closed
    }
    /// What the other end said as it closed, if it closed and said something.
    pub fn close_reason(&self) -> Option<String> {
        let state = self.0.state.lock().unwrap();
        state.closed.then(|| state.reason.clone()).flatten()
    }
}

/// A handler that updates the socket's state and wakes whoever waits on it.
fn handler(
    state: &Arc<Mutex<State>>,
    update: impl Fn(&mut State, Event) + 'static,
) -> Closure<dyn FnMut(Event)> {
    let state = state.clone();
    Closure::new(move |event| {
        let mut state = state.lock().unwrap();
        update(&mut state, event);
        state.wake();
    })
}
