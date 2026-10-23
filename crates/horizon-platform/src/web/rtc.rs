//! WebRTC between two browsers: a world shared from one page and joined
//! from another (web/README.md). A relay's rooms (`crates/horizon-web-relay`)
//! carry each guest's offer and the host's answer; the game's bytes then go
//! over a data channel between the two pages, as a [`Socket`].
use super::socket::Socket;
use crate::time::Duration;
use js_sys::{Function, JSON, Promise};
use std::{
    cell::RefCell,
    collections::VecDeque,
    future::poll_fn,
    rc::Rc,
    sync::{Arc, Mutex},
    task::{Poll, Waker},
};
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    type RTCPeerConnection;
    #[wasm_bindgen(constructor, catch)]
    fn new(config: &JsValue) -> Result<RTCPeerConnection, JsValue>;
    #[wasm_bindgen(method, js_name = createDataChannel)]
    fn create_data_channel(this: &RTCPeerConnection, label: &str) -> JsValue;
    #[wasm_bindgen(method, js_name = createOffer)]
    fn create_offer(this: &RTCPeerConnection) -> Promise;
    #[wasm_bindgen(method, js_name = createAnswer)]
    fn create_answer(this: &RTCPeerConnection) -> Promise;
    #[wasm_bindgen(method, js_name = setLocalDescription)]
    fn set_local_description(this: &RTCPeerConnection, description: &JsValue) -> Promise;
    #[wasm_bindgen(method, js_name = setRemoteDescription)]
    fn set_remote_description(this: &RTCPeerConnection, description: &JsValue) -> Promise;
    #[wasm_bindgen(method, getter, js_name = localDescription)]
    fn local_description(this: &RTCPeerConnection) -> JsValue;
    #[wasm_bindgen(method, getter, js_name = iceGatheringState)]
    fn ice_gathering_state(this: &RTCPeerConnection) -> String;
    #[wasm_bindgen(method, setter = onicegatheringstatechange)]
    fn set_onicegatheringstatechange(this: &RTCPeerConnection, handler: Option<&Function>);
    #[wasm_bindgen(method, setter = ondatachannel)]
    fn set_ondatachannel(this: &RTCPeerConnection, handler: Option<&Function>);
    #[wasm_bindgen(method)]
    fn close(this: &RTCPeerConnection);

    type DataChannelEvent;
    #[wasm_bindgen(method, getter)]
    fn channel(this: &DataChannelEvent) -> JsValue;

    #[wasm_bindgen(js_name = setTimeout)]
    fn set_timeout(handler: &Function, ms: f64) -> JsValue;
    #[wasm_bindgen(js_name = clearTimeout)]
    fn clear_timeout(id: &JsValue);
}

/// How long a peer connection has to come up.
const CONNECT: Duration = Duration::from_secs(20);
/// How long candidates are gathered before the offer or answer goes anyway.
const GATHER: Duration = Duration::from_secs(5);

/// `url` with `query` added to its query string.
pub fn with_query(url: &str, query: &str) -> String {
    let url = url.trim();
    let join = if url.contains('?') { '&' } else { '?' };
    format!("{url}{join}{query}")
}

/// `{"iceServers": [...]}` for STUN servers given as `host:port` (or full
/// `stun:`/`turn:` URLs). None: candidates on this machine's own addresses only.
fn configuration(ice: &[String]) -> JsValue {
    let urls: Vec<String> = ice
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s.starts_with("stun:") || s.starts_with("turn:") || s.starts_with("turns:") {
                format!("{s:?}")
            } else {
                format!("{:?}", format!("stun:{s}"))
            }
        })
        .collect();
    let servers = if urls.is_empty() {
        String::new()
    } else {
        format!("{{\"urls\":[{}]}}", urls.join(","))
    };
    JSON::parse(&format!("{{\"iceServers\":[{servers}]}}")).unwrap_or(JsValue::UNDEFINED)
}

fn js_error(e: JsValue) -> String {
    e.dyn_ref::<js_sys::Error>()
        .map_or_else(|| format!("{e:?}"), |e| String::from(e.message()))
}

async fn wait(promise: Promise) -> Result<JsValue, String> {
    JsFuture::from(promise).await.map_err(js_error)
}

/// Something a browser event fills in, awaited once.
struct Slot<T>(Rc<RefCell<(Option<T>, Option<Waker>)>>);
impl<T> Clone for Slot<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T> Slot<T> {
    fn new() -> Self {
        Self(Rc::new(RefCell::new((None, None))))
    }
    fn fill(&self, value: T) {
        let mut slot = self.0.borrow_mut();
        if slot.0.is_none() {
            slot.0 = Some(value);
        }
        if let Some(waker) = slot.1.take() {
            waker.wake();
        }
    }
    /// The value, or `on_timeout` once `timeout` passed.
    async fn take(&self, timeout: Duration, on_timeout: T) -> T
    where
        T: 'static,
    {
        let late = self.clone();
        let mut on_timeout = Some(on_timeout);
        let timer = Closure::<dyn FnMut()>::new(move || {
            if let Some(value) = on_timeout.take() {
                late.fill(value);
            }
        });
        let id = set_timeout(
            timer.as_ref().unchecked_ref(),
            timeout.as_secs_f64() * 1000.0,
        );
        let value = poll_fn(|cx| {
            let mut slot = self.0.borrow_mut();
            match slot.0.take() {
                Some(value) => Poll::Ready(value),
                None => {
                    slot.1 = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        })
        .await;
        clear_timeout(&id);
        value
    }
}

/// The local description once candidates are gathered (or [`GATHER`] passed),
/// as the JSON the other page reads.
async fn described(pc: &RTCPeerConnection) -> Result<String, String> {
    if pc.ice_gathering_state() != "complete" {
        let done = Slot::new();
        let fill = done.clone();
        let pc_for_state: RTCPeerConnection = JsValue::clone(pc.as_ref()).unchecked_into();
        let on_change = Closure::<dyn FnMut()>::new(move || {
            if pc_for_state.ice_gathering_state() == "complete" {
                fill.fill(());
            }
        });
        pc.set_onicegatheringstatechange(Some(on_change.as_ref().unchecked_ref()));
        done.take(GATHER, ()).await;
        pc.set_onicegatheringstatechange(None);
    }
    JSON::stringify(&pc.local_description())
        .map(String::from)
        .map_err(js_error)
}

fn parse(text: &str) -> Result<JsValue, String> {
    JSON::parse(text).map_err(|_| "The other page sent something unreadable".to_owned())
}

/// Join the world shared with `code` through the relay at `relay`.
pub async fn join(relay: &str, code: &str, ice: &[String]) -> Result<Socket, String> {
    let code: String = code.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let signal = Socket::connect(&with_query(relay, &format!("join={code}")), CONNECT).await?;
    let pc = RTCPeerConnection::new(&configuration(ice)).map_err(js_error)?;
    let channel = pc.create_data_channel("minecraft");
    let offer = wait(pc.create_offer()).await?;
    wait(pc.set_local_description(&offer)).await?;
    signal
        .write(described(&pc).await?.as_bytes())
        .map_err(|e| e.to_string())?;
    let answer = signal.message(CONNECT).await.ok_or_else(|| {
        signal
            .close_reason()
            .unwrap_or_else(|| "Your friend's world didn't answer".to_owned())
    })?;
    let answer = String::from_utf8_lossy(&answer);
    wait(pc.set_remote_description(&parse(&answer)?)).await?;
    let socket = Socket::adopt(channel, pc.into());
    socket
        .opened(CONNECT)
        .await
        .map_err(|_| "Couldn't reach your friend's browser directly".to_owned())?;
    signal.close();
    Ok(socket)
}

/// What a shared world hears from its relay room.
pub enum HostEvent {
    /// The code friends join with.
    Code(String),
    /// A friend's data channel opened.
    Guest(Socket),
    /// The room closed: why.
    Closed(String),
}

/// A world shared through a relay's room: friends' offers are answered as
/// they come, and their connections arrive through [`Host::poll`].
pub struct Host {
    /// The room's socket once open, and whether the room was closed.
    signal: Arc<Mutex<(Option<Socket>, bool)>>,
    events: Arc<Mutex<VecDeque<HostEvent>>>,
}
impl Host {
    pub fn start(relay: &str, ice: &[String]) -> Self {
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let signal = Arc::new(Mutex::new((None, false)));
        let (url, ice) = (with_query(relay, "host"), ice.to_vec());
        let (task_events, task_signal) = (events.clone(), signal.clone());
        wasm_bindgen_futures::spawn_local(async move {
            let reason = match host(&url, &ice, &task_events, &task_signal).await {
                Ok(()) => "Stopped sharing".to_owned(),
                Err(reason) => reason,
            };
            task_events
                .lock()
                .unwrap()
                .push_back(HostEvent::Closed(reason));
        });
        Self { signal, events }
    }
    /// What happened since the last poll.
    pub fn poll(&self) -> Vec<HostEvent> {
        self.events.lock().unwrap().drain(..).collect()
    }
    /// Close the room: the code stops working; friends in stay.
    pub fn stop(&mut self) {
        let mut signal = self.signal.lock().unwrap();
        signal.1 = true;
        if let Some(socket) = signal.0.take() {
            socket.close();
        }
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn host(
    url: &str,
    ice: &[String],
    events: &Arc<Mutex<VecDeque<HostEvent>>>,
    shared_signal: &Arc<Mutex<(Option<Socket>, bool)>>,
) -> Result<(), String> {
    let signal = Socket::connect(url, CONNECT).await?;
    {
        let mut shared = shared_signal.lock().unwrap();
        // Stopped while the room opened.
        if shared.1 {
            return Ok(());
        }
        shared.0 = Some(signal.clone());
    }
    let forever = Duration::from_secs(u64::from(u32::MAX));
    while let Some(message) = signal.message(forever).await {
        let message = String::from_utf8_lossy(&message).into_owned();
        let Some((head, rest)) = message.split_once(' ') else {
            continue;
        };
        match head {
            "room" => events
                .lock()
                .unwrap()
                .push_back(HostEvent::Code(rest.to_owned())),
            "left" => {}
            id => {
                let (id, offer, signal) = (id.to_owned(), rest.to_owned(), signal.clone());
                let (events, ice) = (events.clone(), ice.to_vec());
                wasm_bindgen_futures::spawn_local(async move {
                    if let Ok(socket) = answer(&id, &offer, &ice, &signal).await {
                        events.lock().unwrap().push_back(HostEvent::Guest(socket));
                    }
                });
            }
        }
    }
    Err(signal
        .close_reason()
        .unwrap_or_else(|| "The relay closed the room".to_owned()))
}

/// Answer guest `id`'s offer; its data channel once it opens.
async fn answer(id: &str, offer: &str, ice: &[String], signal: &Socket) -> Result<Socket, String> {
    let pc = RTCPeerConnection::new(&configuration(ice)).map_err(js_error)?;
    // The channel is read from its first event on: the friend sends as
    // soon as it opens.
    let channel = Slot::<Option<Socket>>::new();
    let fill = channel.clone();
    let peer = JsValue::clone(pc.as_ref());
    let on_channel = Closure::<dyn FnMut(DataChannelEvent)>::new(move |e: DataChannelEvent| {
        fill.fill(Some(Socket::adopt(e.channel(), peer.clone())));
    });
    pc.set_ondatachannel(Some(on_channel.as_ref().unchecked_ref()));
    wait(pc.set_remote_description(&parse(offer)?)).await?;
    let answer = wait(pc.create_answer()).await?;
    wait(pc.set_local_description(&answer)).await?;
    let described = described(&pc).await?;
    signal
        .write(format!("{id} {described}").as_bytes())
        .map_err(|e| e.to_string())?;
    let socket = channel.take(CONNECT, None).await;
    pc.set_ondatachannel(None);
    let Some(socket) = socket else {
        pc.close();
        return Err("the friend's channel never came".to_owned());
    };
    socket.opened(CONNECT).await?;
    Ok(socket)
}
