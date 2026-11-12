//! The concealed-content check on Linux. Password managers such as
//! KeePassXC offer the type `x-kde-passwordManagerHint` next to the text of
//! a secret, on X11 and on Wayland. On X11 the check reads only the list of
//! types, and arboard reads the text. On Wayland the check also reads the
//! text, from the seat whose types it checked: wl-clipboard-rs picks an
//! unspecified seat anew for each call, so with several seats a separate
//! read could see another clipboard.

use super::Checked;
use crate::protocol::CLIPBOARD_MAX_TEXT;
use std::{
    error::Error,
    io::Read,
    thread,
    time::{Duration, Instant},
};
use wayland_client::{
    protocol::{wl_registry, wl_seat},
    Dispatch, QueueHandle,
};
use wl_clipboard_rs::{
    paste::{self, ClipboardType, Seat},
    utils,
};
use x11rb::{
    connection::Connection,
    protocol::{
        xproto::{AtomEnum, ConnectionExt, CreateWindowAux, WindowClass},
        Event,
    },
    rust_connection::RustConnection,
    COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, CURRENT_TIME, NONE,
};

const HINT: &str = "x-kde-passwordManagerHint";

/// How long the X11 clipboard owner has to list its types.
const TARGETS_TIMEOUT: Duration = Duration::from_millis(1000);

/// The most bytes that the Wayland read takes from the clipboard's owner:
/// the protocol's limit and one more character, so that the caller sees
/// that it cut the text.
const READ_LIMIT: usize = CLIPBOARD_MAX_TEXT + 4;

/// Asks the backend that arboard reads from, chosen as arboard chooses it:
/// Wayland data control when wl-clipboard-rs's check of the compositor
/// succeeds, and X11 (Xwayland on GNOME) when it fails, also for a
/// compositor without seats.
pub fn concealed() -> Result<Checked, Box<dyn Error>> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some()
        && utils::is_primary_selection_supported().is_ok()
    {
        return wayland_check();
    }
    Ok(if x11_offers_hint()? {
        Checked::Secret
    } else {
        Checked::Clear
    })
}

/// Checks the types of one seat's clipboard and, when it holds no secret,
/// reads its text from the same seat.
fn wayland_check() -> Result<Checked, Box<dyn Error>> {
    let seats = seats()?;
    let seat = match chosen_seat(&seats)? {
        Some(name) => Seat::Specific(name),
        None => Seat::Unspecified,
    };
    match paste::get_mime_types(ClipboardType::Regular, seat) {
        Ok(types) if types.contains(HINT) => return Ok(Checked::Secret),
        Ok(_) => {}
        Err(paste::Error::ClipboardEmpty) => return Ok(Checked::Read(None)),
        Err(err) => return Err(err.into()),
    }
    match paste::get_contents(ClipboardType::Regular, seat, paste::MimeType::Text) {
        Ok((pipe, _)) => Ok(Checked::Read(Some(read_text(pipe)?))),
        Err(paste::Error::ClipboardEmpty | paste::Error::NoMimeType) => Ok(Checked::Read(None)),
        Err(err) => Err(err.into()),
    }
}

/// Reads the text that the clipboard's owner writes, up to `READ_LIMIT`
/// bytes. An owner can write without end.
fn read_text(pipe: impl Read) -> Result<String, Box<dyn Error>> {
    let mut bytes = Vec::new();
    pipe.take(READ_LIMIT as u64).read_to_end(&mut bytes)?;
    let cut = bytes.len() == READ_LIMIT;
    match String::from_utf8(bytes) {
        // The limit can fall inside the last character.
        Err(error) if cut && error.utf8_error().error_len().is_none() => {
            let end = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(end);
            Ok(String::from_utf8(bytes)?)
        }
        text => Ok(text?),
    }
}

/// The compositor's seats: how many there are, and the names of those that
/// sent one, sorted.
#[derive(Default)]
struct Seats {
    count: usize,
    names: Vec<String>,
}

/// The seat to check and read: none to name when there is one seat or
/// none, else the first name in sorted order, so that every request picks
/// the same seat. Several seats of which one has no name cannot be told
/// apart, which is an error.
fn chosen_seat(seats: &Seats) -> Result<Option<&str>, Box<dyn Error>> {
    if seats.count <= 1 {
        return Ok(None);
    }
    if seats.names.len() < seats.count {
        return Err(format!(
            "{} seats, of which only {} have a name",
            seats.count,
            seats.names.len()
        )
        .into());
    }
    Ok(seats.names.first().map(String::as_str))
}

fn seats() -> Result<Seats, Box<dyn Error>> {
    let connection = wayland_client::Connection::connect_to_env()?;
    let mut queue = connection.new_event_queue();
    connection.display().get_registry(&queue.handle(), ());
    let mut seats = Seats::default();
    // The first round trip lists the globals, the second the seat names.
    queue.roundtrip(&mut seats)?;
    queue.roundtrip(&mut seats)?;
    seats.names.sort();
    Ok(seats)
}

impl Dispatch<wl_registry::WlRegistry, ()> for Seats {
    fn event(
        seats: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &wayland_client::Connection,
        queue: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == "wl_seat" {
                seats.count += 1;
                // Seats report their name from version 2 on.
                if version >= 2 {
                    registry.bind::<wl_seat::WlSeat, _, _>(name, 2, queue, ());
                }
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Seats {
    fn event(
        seats: &mut Self,
        _: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &wayland_client::Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Name { name } = event {
            seats.names.push(name);
        }
    }
}

/// Converts the CLIPBOARD selection to TARGETS, which yields the owner's
/// list of types as atoms.
fn x11_offers_hint() -> Result<bool, Box<dyn Error>> {
    let (conn, screen) = RustConnection::connect(None)?;
    let clipboard = conn.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
    if conn.get_selection_owner(clipboard)?.reply()?.owner == NONE {
        return Ok(false);
    }
    let targets = conn.intern_atom(false, b"TARGETS")?.reply()?.atom;
    let window = conn.generate_id()?;
    let root = conn.setup().roots[screen].root;
    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        window,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        COPY_FROM_PARENT,
        &CreateWindowAux::new(),
    )?;
    conn.convert_selection(window, clipboard, targets, targets, CURRENT_TIME)?;
    conn.flush()?;
    if !owner_answered(&conn)? {
        return Ok(false);
    }
    // The owner interns each type it lists, so a hint atom that does not
    // exist after the answer is not in the list.
    let hint = conn.intern_atom(true, HINT.as_bytes())?.reply()?.atom;
    if hint == NONE {
        return Ok(false);
    }
    let list = conn
        .get_property(true, window, targets, AtomEnum::ANY, 0, 4096)?
        .reply()?;
    let mut atoms = list.value32().into_iter().flatten();
    Ok(atoms.any(|atom| atom == hint))
}

/// Waits for the owner's SelectionNotify. False when the owner refuses to
/// list its types.
fn owner_answered(conn: &RustConnection) -> Result<bool, Box<dyn Error>> {
    let deadline = Instant::now() + TARGETS_TIMEOUT;
    loop {
        match conn.poll_for_event()? {
            Some(Event::SelectionNotify(event)) => return Ok(event.property != NONE),
            Some(_) => {}
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            None => return Err("the clipboard owner did not list its types".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seats(count: usize, names: &[&str]) -> Seats {
        Seats {
            count,
            names: names.iter().map(|name| name.to_string()).collect(),
        }
    }

    #[test]
    fn the_read_stops_at_an_owner_that_writes_without_end() {
        let text = read_text(std::io::repeat(b'a')).unwrap();
        assert_eq!(text.len(), READ_LIMIT);
        let (_, truncated) = super::super::truncate(text);
        assert!(truncated);
    }

    #[test]
    fn the_read_limit_keeps_whole_characters() {
        // The limit falls inside a three-byte character.
        let long = format!("a{}", "\u{20ac}".repeat(READ_LIMIT));
        let text = read_text(long.as_bytes()).unwrap();
        assert_eq!(text.len(), READ_LIMIT - (READ_LIMIT - 1) % 3);
        assert!(long.starts_with(&text));
        assert!(text.len() > CLIPBOARD_MAX_TEXT);
    }

    #[test]
    fn text_below_the_limit_that_ends_inside_a_character_is_an_error() {
        assert!(read_text(b"cut \xe2\x82".as_slice()).is_err());
    }

    #[test]
    fn one_seat_needs_no_name() {
        assert_eq!(chosen_seat(&seats(1, &[])).unwrap(), None);
        assert_eq!(chosen_seat(&seats(1, &["seat0"])).unwrap(), None);
    }

    #[test]
    fn several_seats_use_the_first_name() {
        assert_eq!(
            chosen_seat(&seats(2, &["seat0", "seat1"])).unwrap(),
            Some("seat0")
        );
    }

    #[test]
    fn several_seats_without_all_names_fail() {
        assert!(chosen_seat(&seats(2, &["seat1"])).is_err());
    }
}
