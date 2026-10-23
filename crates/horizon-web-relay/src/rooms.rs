//! Worlds played in a browser and shared with friends (web/README.md): the
//! relay introduces the browsers, which then connect to each other over
//! WebRTC. The host's page opens `?host` and gets a code; a friend's page
//! opens `?join=CODE`, and the two trade the offer and answer that set up
//! their peer connection through here. The game's bytes then go straight
//! between them: the relay never carries them.
//!
//! Codes look like Play with Friends' (`K7QM-3XPD`, Crockford base32), so
//! the game's Join a Friend screen takes them as they are.
//!
//! Messages are text. To the host: `room CODE` once, then `ID MESSAGE` for
//! what guest ID sent and `left ID` when it goes; from the host: `ID MESSAGE`
//! for guest ID. Guests send and receive their messages as they are.
use super::{close_frame, frame, read_frame};
use std::{
    collections::HashMap,
    io::{self, BufReader, Write},
    net::{Shutdown, TcpStream},
    sync::{Arc, Mutex},
};

/// Crockford base32, as horizon-friends' codes: no I, L, O or U.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_LEN: usize = 8;
/// The largest message: an offer or answer, with its candidates, is a few KiB.
const MAX_MESSAGE: usize = 64 << 10;

/// The open rooms by code.
#[derive(Default)]
pub struct Rooms {
    rooms: Mutex<HashMap<String, Room>>,
    pub max_rooms: usize,
    pub max_guests: usize,
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
fn new_code(rooms: &HashMap<String, Room>) -> io::Result<String> {
    loop {
        let mut bytes = [0u8; CODE_LEN];
        getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
        let code: String = bytes
            .iter()
            .map(|b| char::from(ALPHABET[usize::from(*b) % ALPHABET.len()]))
            .collect();
        if !rooms.contains_key(&code) {
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

impl Rooms {
    pub fn new(max_rooms: usize, max_guests: usize) -> Self {
        Self {
            rooms: Mutex::default(),
            max_rooms,
            max_guests,
        }
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
        let code = new_code(&HashMap::new()).unwrap();
        assert_eq!(code.len(), CODE_LEN);
        assert_eq!(canonical(&shown(&code)), code);
    }
}
