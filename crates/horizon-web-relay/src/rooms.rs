//! Worlds played in a browser and shared with friends (web/README.md): the
//! relay introduces the browsers, which then connect to each other over
//! WebRTC. The host's page opens `?host` and gets a code; a friend's page
//! opens `?join=CODE`, and the two trade the offer and answer that set up
//! their peer connection through here. The game's bytes then go straight
//! between them, unless they can't (below).
//!
//! Codes look like Play with Friends' (`K7QM-3XPD`, Crockford base32), so
//! the game's Join a Friend screen takes them as they are.
//!
//! Messages are text. To the host: `room CODE` once, then `ID MESSAGE` for
//! what guest ID sent and `left ID` when it goes; from the host: `ID MESSAGE`
//! for guest ID. Guests send and receive their messages as they are.
//!
//! When two browsers can't reach each other (both behind strict NATs, with no
//! TURN server), the guest sends `relay`: the relay answers it `pipe TOKEN`
//! and tells the host `pipe ID TOKEN`, both pages connect `?pipe=TOKEN`, and
//! the relay passes the game's bytes between them (at most `max_pipes` at
//! once; 0, none: the guest hears `nopipe REASON`).
use super::{close_frame, frame, read_frame};
use std::{
    collections::HashMap,
    io::{self, BufReader, Write},
    net::{Shutdown, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// Crockford base32, as horizon-friends' codes: no I, L, O or U.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_LEN: usize = 8;
/// The largest message: an offer or answer, with its candidates, is a few KiB.
const MAX_MESSAGE: usize = 64 << 10;

/// How long a pipe's first page waits for the other.
const PIPE_WAIT: Duration = Duration::from_secs(30);
/// Pipe tokens are longer than codes: they are never typed.
const TOKEN_LEN: usize = 24;

/// The open rooms by code.
#[derive(Default)]
pub struct Rooms {
    rooms: Mutex<HashMap<String, Room>>,
    pub max_rooms: usize,
    pub max_guests: usize,
    /// Pipes asked for and not yet joined by both pages, by token.
    pipes: Mutex<HashMap<String, Pipe>>,
    /// Pipes passing bytes.
    piping: AtomicUsize,
    pub max_pipes: usize,
}

/// A pipe asked for: when, and its first page once it connects, which waits
/// for the other's writer.
struct Pipe {
    asked: Instant,
    first: Option<(Writer, mpsc::Sender<Writer>)>,
}

struct Room {
    host: Writer,
    guests: HashMap<u32, Writer>,
    next: u32,
}

/// One end's socket, written from more than one thread.
type Writer = Arc<Mutex<TcpStream>>;

fn send(to: &Writer, text: &str) -> io::Result<()> {
    (&*to.lock().unwrap()).write_all(&frame(0x1, text.as_bytes()))
}
fn close(to: &Writer, code: u16, reason: &str) {
    let socket = to.lock().unwrap();
    let _ = (&*socket).write_all(&close_frame(code, reason));
    let _ = socket.shutdown(Shutdown::Both);
}

/// A fresh code, none of the open rooms'.
fn new_code<T>(rooms: &HashMap<String, T>) -> io::Result<String> {
    new_key(rooms, CODE_LEN)
}
fn new_key<T>(taken: &HashMap<String, T>, len: usize) -> io::Result<String> {
    loop {
        let mut bytes = vec![0u8; len];
        getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
        let code: String = bytes
            .iter()
            .map(|b| char::from(ALPHABET[usize::from(*b) % ALPHABET.len()]))
            .collect();
        if !taken.contains_key(&code) {
            return Ok(code);
        }
    }
}

/// A code as a player typed it, canonical: case, spaces and dashes don't
/// matter, `O` reads as `0` and `I` or `L` as `1`.
pub fn canonical(code: &str) -> String {
    code.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect()
}

/// A code as players see it: `K7QM-3XPD`.
pub fn shown(code: &str) -> String {
    let (a, b) = code.split_at(code.len() / 2);
    format!("{a}-{b}")
}

/// The next message from a page; None once it closed.
fn next_message(reader: &mut BufReader<TcpStream>, writer: &Writer) -> Option<String> {
    loop {
        match read_frame(reader) {
            Ok((0x0..=0x2, payload)) if payload.len() <= MAX_MESSAGE => {
                return String::from_utf8(payload).ok();
            }
            Ok((0x9, payload)) => {
                let _ = (&*writer.lock().unwrap()).write_all(&frame(0xA, &payload));
            }
            Ok((0x8, _)) | Err(_) => return None,
            Ok((0xA, _)) => {}
            Ok(_) => return None,
        }
    }
}

/// The next data a pipe's page sent, of any size the relay's frames take;
/// None once it closed.
fn next_bytes(reader: &mut BufReader<TcpStream>, writer: &Writer) -> Option<Vec<u8>> {
    loop {
        match read_frame(reader) {
            Ok((0x0..=0x2, payload)) => return Some(payload),
            Ok((0x9, payload)) => {
                let _ = (&*writer.lock().unwrap()).write_all(&frame(0xA, &payload));
            }
            Ok((0x8, _)) | Err(_) => return None,
            Ok((0xA, _)) => {}
            Ok(_) => return None,
        }
    }
}

impl Rooms {
    pub fn new(max_rooms: usize, max_guests: usize, max_pipes: usize) -> Self {
        Self {
            rooms: Mutex::default(),
            max_rooms,
            max_guests,
            pipes: Mutex::default(),
            piping: AtomicUsize::new(0),
            max_pipes,
        }
    }

    /// A new pipe's token, or why there is none.
    fn ask_pipe(&self) -> Result<String, &'static str> {
        if self.max_pipes == 0 {
            return Err("This relay passes no game traffic");
        }
        let mut pipes = self.pipes.lock().unwrap();
        pipes.retain(|_, pipe| pipe.asked.elapsed() < PIPE_WAIT);
        // Both pages of a pipe count in `piping`.
        if pipes.len() + self.piping.load(Ordering::Relaxed) / 2 >= self.max_pipes {
            return Err("The relay is passing too many games; try again later");
        }
        let token = new_key(&pipes, TOKEN_LEN).map_err(|_| "The relay has no randomness")?;
        pipes.insert(
            token.clone(),
            Pipe {
                asked: Instant::now(),
                first: None,
            },
        );
        Ok(token)
    }

    /// One page of a pipe (`?pipe=TOKEN`): what it sends goes to the other's.
    pub fn pipe(
        &self,
        token: &str,
        mut reader: BufReader<TcpStream>,
        socket: TcpStream,
    ) -> io::Result<()> {
        let writer: Writer = Arc::new(Mutex::new(socket));
        let waiting = {
            let mut pipes = self.pipes.lock().unwrap();
            match pipes.get_mut(token) {
                Some(pipe) if pipe.asked.elapsed() < PIPE_WAIT => match pipe.first.take() {
                    Some((first, give)) => {
                        pipes.remove(token);
                        let _ = give.send(writer.clone());
                        Ok(first)
                    }
                    None => {
                        let (give, take) = mpsc::channel();
                        pipe.first = Some((writer.clone(), give));
                        Err(take)
                    }
                },
                _ => {
                    close(&writer, 1008, "The pipe isn't open (any more)");
                    return Ok(());
                }
            }
        };
        let other = match waiting {
            Ok(first) => first,
            Err(take) => match take.recv_timeout(PIPE_WAIT) {
                Ok(other) => other,
                Err(_) => {
                    self.pipes.lock().unwrap().remove(token);
                    close(&writer, 1011, "The other page never came");
                    return Ok(());
                }
            },
        };
        self.piping.fetch_add(1, Ordering::Relaxed);
        while let Some(payload) = next_bytes(&mut reader, &writer) {
            if (&*other.lock().unwrap())
                .write_all(&frame(0x2, &payload))
                .is_err()
            {
                break;
            }
        }
        self.piping.fetch_sub(1, Ordering::Relaxed);
        close(&other, 1000, "");
        close(&writer, 1000, "");
        Ok(())
    }

    /// A host's page: its room is open while the page stays connected.
    pub fn host(&self, mut reader: BufReader<TcpStream>, socket: TcpStream) -> io::Result<()> {
        let writer: Writer = Arc::new(Mutex::new(socket));
        let code = {
            let mut rooms = self.rooms.lock().unwrap();
            if rooms.len() >= self.max_rooms {
                close(
                    &writer,
                    1013,
                    "The relay has too many worlds open; try again later",
                );
                return Ok(());
            }
            let code = new_code(&rooms)?;
            rooms.insert(
                code.clone(),
                Room {
                    host: writer.clone(),
                    guests: HashMap::new(),
                    next: 1,
                },
            );
            code
        };
        send(&writer, &format!("room {}", shown(&code)))?;
        while let Some(message) = next_message(&mut reader, &writer) {
            let Some((id, rest)) = message.split_once(' ') else {
                continue;
            };
            let guest = id.parse().ok().and_then(|id: u32| {
                let rooms = self.rooms.lock().unwrap();
                rooms.get(&code)?.guests.get(&id).cloned()
            });
            if let Some(guest) = guest {
                let _ = send(&guest, rest);
            }
        }
        let room = self.rooms.lock().unwrap().remove(&code);
        for guest in room.into_iter().flat_map(|r| r.guests.into_values()) {
            close(&guest, 1001, "The world closed");
        }
        close(&writer, 1000, "");
        Ok(())
    }

    /// A friend's page, for the world `code` names.
    pub fn join(
        &self,
        code: &str,
        mut reader: BufReader<TcpStream>,
        socket: TcpStream,
    ) -> io::Result<()> {
        let writer: Writer = Arc::new(Mutex::new(socket));
        let code = canonical(code);
        let joined = {
            let mut rooms = self.rooms.lock().unwrap();
            match rooms.get_mut(&code) {
                None => Err("No world is shared with that code"),
                Some(room) if room.guests.len() >= self.max_guests => {
                    Err("Too many players are joining that world; try again")
                }
                Some(room) => {
                    let id = room.next;
                    room.next += 1;
                    room.guests.insert(id, writer.clone());
                    Ok((id, room.host.clone()))
                }
            }
        };
        let (id, host) = match joined {
            Ok(joined) => joined,
            Err(reason) => {
                close(&writer, 1008, reason);
                return Ok(());
            }
        };
        while let Some(message) = next_message(&mut reader, &writer) {
            // The two browsers couldn't connect: the relay passes the bytes.
            if message == "relay" {
                match self.ask_pipe() {
                    Ok(token) => {
                        if send(&host, &format!("pipe {id} {token}")).is_err() {
                            break;
                        }
                        let _ = send(&writer, &format!("pipe {token}"));
                    }
                    Err(reason) => {
                        let _ = send(&writer, &format!("nopipe {reason}"));
                    }
                }
                continue;
            }
            if send(&host, &format!("{id} {message}")).is_err() {
                break;
            }
        }
        let host = {
            let mut rooms = self.rooms.lock().unwrap();
            rooms.get_mut(&code).map(|room| {
                room.guests.remove(&id);
                room.host.clone()
            })
        };
        if let Some(host) = host {
            let _ = send(&host, &format!("left {id}"));
        }
        close(&writer, 1000, "");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_read_as_typed() {
        assert_eq!(canonical(" k7qm-3xpd "), "K7QM3XPD");
        assert_eq!(canonical("O1IL"), "0111");
        assert_eq!(shown("K7QM3XPD"), "K7QM-3XPD");
        let code = new_code(&HashMap::<String, ()>::new()).unwrap();
        assert_eq!(code.len(), CODE_LEN);
        assert_eq!(canonical(&shown(&code)), code);
    }
}
