//! What the web build has for a TCP connection: a byte stream over a
//! WebSocket (or anything shaped like one, such as a WebRTC data channel).
//! The page hands over what arrives as events, so reads never block: they
//! take what has arrived, and [`Socket::readable`] waits for more.
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
    #[wasm_bindgen(method, setter = onopen)]
    fn set_onopen(this: &Transport, handler: Option<&Function>);
    #[wasm_bindgen(method, setter = onmessage)]
    fn set_onmessage(this: &Transport, handler: Option<&Function>);
    #[wasm_bindgen(method, setter = onclose)]
    fn set_onclose(this: &Transport, handler: Option<&Function>);
    #[wasm_bindgen(method, setter = onerror)]
    fn set_onerror(this: &Transport, handler: Option<&Function>);
    #[wasm_bindgen(method, catch)]
    fn send(this: &Transport, data: &Uint8Array) -> Result<(), JsValue>;
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

/// What the page has said about one socket.
#[derive(Default)]
struct State {
    received: VecDeque<u8>,
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
    _handlers: Vec<Closure<dyn FnMut(Event)>>,
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
            t.close();
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
        let socket = Self::over(ws.unchecked_into());
        let state = socket.0.state.clone();
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
            let mut state = socket.0.state.lock().unwrap();
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
        opened.map(|()| socket)
    }

    /// A socket over a WebSocket or data channel that is opening or open.
    fn over(transport: Transport) -> Self {
        let id = NEXT.with(|n| n.replace(n.get() + 1));
        let state = Arc::new(Mutex::new(State::default()));
        let open = handler(&state, |state, _| state.open = true);
        let message = handler(&state, |state, event| {
            if let Some(data) = event.data().dyn_ref::<ArrayBuffer>() {
                state.received.extend(Uint8Array::new(data).to_vec());
            }
        });
        // Browsers keep why a connection failed to themselves: only a reason
        // the other end gave as it closed comes through.
        let close = handler(&state, |state, event| {
            state.closed = true;
            state.reason = event.reason().as_string().filter(|r| !r.is_empty());
        });
        transport.set_binary_type("arraybuffer");
        transport.set_onopen(Some(open.as_ref().unchecked_ref()));
        transport.set_onmessage(Some(message.as_ref().unchecked_ref()));
        transport.set_onclose(Some(close.as_ref().unchecked_ref()));
        SOCKETS.with(|s| {
            s.borrow_mut().insert(
                id,
                Entry {
                    transport,
                    _handlers: vec![open, message, close],
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
        if state.received.is_empty() {
            return match (state.closed, &state.reason) {
                (false, _) => Err(io::ErrorKind::WouldBlock.into()),
                (true, None) => Ok(0),
                (true, Some(reason)) => Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    reason.clone(),
                )),
            };
        }
        let n = out.len().min(state.received.len());
        for (o, b) in out.iter_mut().zip(state.received.drain(..n)) {
            *o = b;
        }
        Ok(n)
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
    /// Sends `data` as one message.
    pub fn write(&self, data: &[u8]) -> io::Result<usize> {
        let state = self.0.state.lock().unwrap();
        if !state.open || state.closed {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        drop(state);
        SOCKETS.with(|s| match s.borrow().get(&self.0.id) {
            Some(entry) => entry
                .transport
                .send(&Uint8Array::from(data))
                .map(|()| data.len())
                .map_err(|_| io::ErrorKind::BrokenPipe.into()),
            None => Err(io::ErrorKind::BrokenPipe.into()),
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
