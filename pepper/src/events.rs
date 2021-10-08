use std::{error::Error, fmt, str::Chars};

use crate::{
    buffer::BufferHandle,
    buffer_position::BufferRange,
    buffer_view::BufferViewHandle,
    client::ClientHandle,
    cursor::Cursor,
    platform::Key,
    serialization::{DeserializeError, Deserializer, Serialize, Serializer},
};

#[derive(Clone, Copy)]
pub struct EditorEventText {
    from: u32,
    to: u32,
}
impl EditorEventText {
    pub fn as_str<'a>(&self, events: &'a EditorEventQueue) -> &'a str {
        &events.read.texts[self.from as usize..self.to as usize]
    }
}

#[derive(Clone, Copy)]
pub struct EditorEventCursors {
    from: u32,
    to: u32,
}
impl EditorEventCursors {
    pub fn as_cursors<'a>(&self, events: &'a EditorEventQueue) -> &'a [Cursor] {
        &events.read.cursors[self.from as usize..self.to as usize]
    }
}

pub enum EditorEvent {
    Idle,
    BufferRead {
        handle: BufferHandle,
    },
    BufferInsertText {
        handle: BufferHandle,
        range: BufferRange,
        text: EditorEventText,
    },
    BufferDeleteText {
        handle: BufferHandle,
        range: BufferRange,
    },
    BufferWrite {
        handle: BufferHandle,
        new_path: bool,
    },
    BufferClose {
        handle: BufferHandle,
    },
    FixCursors {
        handle: BufferViewHandle,
        cursors: EditorEventCursors,
    },
    BufferViewLostFocus { // TODO: remove since we won't have `BufferProperties.auto_close` anymore
        handle: BufferViewHandle,
    },
}

#[derive(Default)]
struct EventQueue {
    events: Vec<EditorEvent>,
    texts: String,
    cursors: Vec<Cursor>,
}

#[derive(Default)]
pub struct EditorEventQueue {
    read: EventQueue,
    write: EventQueue,
}
impl EditorEventQueue {
    pub fn flip(&mut self) {
        self.read.events.clear();
        self.read.texts.clear();
        std::mem::swap(&mut self.read, &mut self.write);
    }

    pub fn enqueue(&mut self, event: EditorEvent) {
        self.write.events.push(event);
    }

    pub fn enqueue_buffer_insert(&mut self, handle: BufferHandle, range: BufferRange, text: &str) {
        let from = self.write.texts.len();
        self.write.texts.push_str(text);
        let text = EditorEventText {
            from: from as _,
            to: self.write.texts.len() as _,
        };
        self.write.events.push(EditorEvent::BufferInsertText {
            handle,
            range,
            text,
        });
    }

    pub fn enqueue_fix_cursors(&mut self, handle: BufferViewHandle, cursors: &[Cursor]) {
        let from = self.write.cursors.len();
        self.write.cursors.extend_from_slice(cursors);
        let cursors = EditorEventCursors {
            from: from as _,
            to: self.write.cursors.len() as _,
        };
        self.write
            .events
            .push(EditorEvent::FixCursors { handle, cursors });
    }
}

pub struct EditorEventIter(usize);
impl EditorEventIter {
    pub fn new() -> Self {
        Self(0)
    }

    pub fn next<'a>(&mut self, queue: &'a EditorEventQueue) -> Option<&'a EditorEvent> {
        let event = queue.read.events.get(self.0)?;
        self.0 += 1;
        Some(event)
    }
}

#[derive(Debug)]
pub enum KeyParseError {
    UnexpectedEnd,
    InvalidCharacter(char),
}
impl fmt::Display for KeyParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::UnexpectedEnd => write!(f, "could not finish parsing key"),
            Self::InvalidCharacter(c) => write!(f, "invalid character {}", c),
        }
    }
}
impl Error for KeyParseError {}

#[derive(Debug)]
pub struct KeyParseAllError {
    pub index: usize,
    pub error: KeyParseError,
}
impl fmt::Display for KeyParseAllError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} at char: {}", self.error, self.index)
    }
}
impl Error for KeyParseAllError {}

pub struct KeyParser<'a> {
    chars: Chars<'a>,
    raw: &'a str,
}
impl<'a> KeyParser<'a> {
    pub fn new(raw: &'a str) -> Self {
        Self {
            chars: raw.chars(),
            raw,
        }
    }
}
impl<'a> Iterator for KeyParser<'a> {
    type Item = Result<Key, KeyParseAllError>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.chars.as_str().is_empty() {
            return None;
        }
        match parse_key(&mut self.chars) {
            Ok(key) => Some(Ok(key)),
            Err(error) => {
                let parsed_len = self.raw.len() - self.chars.as_str().len();
                let index = self.raw[..parsed_len]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                self.raw = "";
                Some(Err(KeyParseAllError { index, error }))
            }
        }
    }
}

fn parse_key(chars: &mut Chars) -> Result<Key, KeyParseError> {
    fn next(chars: &mut impl Iterator<Item = char>) -> Result<char, KeyParseError> {
        match chars.next() {
            Some(c) => Ok(c),
            None => Err(KeyParseError::UnexpectedEnd),
        }
    }

    fn consume(chars: &mut impl Iterator<Item = char>, c: char) -> Result<(), KeyParseError> {
        let next = next(chars)?;
        if c == next {
            Ok(())
        } else {
            Err(KeyParseError::InvalidCharacter(next))
        }
    }

    fn consume_str(chars: &mut impl Iterator<Item = char>, s: &str) -> Result<(), KeyParseError> {
        for c in s.chars() {
            consume(chars, c)?
        }
        Ok(())
    }

    match next(chars)? {
        '<' => match next(chars)? {
            'b' => {
                consume_str(chars, "ackspace>")?;
                Ok(Key::Backspace)
            }
            's' => {
                consume_str(chars, "pace>")?;
                Ok(Key::Char(' '))
            }
            'e' => match next(chars)? {
                'n' => match next(chars)? {
                    't' => {
                        consume_str(chars, "er>")?;
                        Ok(Key::Enter)
                    }
                    'd' => {
                        consume(chars, '>')?;
                        Ok(Key::End)
                    }
                    c => Err(KeyParseError::InvalidCharacter(c)),
                },
                's' => {
                    consume_str(chars, "c>")?;
                    Ok(Key::Esc)
                }
                c => Err(KeyParseError::InvalidCharacter(c)),
            },
            'l' => {
                consume(chars, 'e')?;
                match next(chars)? {
                    's' => {
                        consume_str(chars, "s>")?;
                        Ok(Key::Char('<'))
                    }
                    'f' => {
                        consume_str(chars, "t>")?;
                        Ok(Key::Left)
                    }
                    c => Err(KeyParseError::InvalidCharacter(c)),
                }
            }
            'g' => {
                consume_str(chars, "reater>")?;
                Ok(Key::Char('>'))
            }
            'r' => {
                consume_str(chars, "ight>")?;
                Ok(Key::Right)
            }
            'u' => {
                consume_str(chars, "p>")?;
                Ok(Key::Up)
            }
            'd' => match next(chars)? {
                'o' => {
                    consume_str(chars, "wn>")?;
                    Ok(Key::Down)
                }
                'e' => {
                    consume_str(chars, "lete>")?;
                    Ok(Key::Delete)
                }
                c => Err(KeyParseError::InvalidCharacter(c)),
            },
            'h' => {
                consume_str(chars, "ome>")?;
                Ok(Key::Home)
            }
            'p' => {
                consume_str(chars, "age")?;
                match next(chars)? {
                    'u' => {
                        consume_str(chars, "p>")?;
                        Ok(Key::PageUp)
                    }
                    'd' => {
                        consume_str(chars, "own>")?;
                        Ok(Key::PageDown)
                    }
                    c => Err(KeyParseError::InvalidCharacter(c)),
                }
            }
            't' => {
                consume_str(chars, "ab>")?;
                Ok(Key::Tab)
            }
            'f' => {
                let c = next(chars)?;
                match c.to_digit(10) {
                    Some(d0) => {
                        let c = next(chars)?;
                        match c.to_digit(10) {
                            Some(d1) => {
                                consume(chars, '>')?;
                                let n = d0 * 10 + d1;
                                Ok(Key::F(n as _))
                            }
                            None => match c {
                                '>' => Ok(Key::F(d0 as _)),
                                _ => Err(KeyParseError::InvalidCharacter(c)),
                            },
                        }
                    }
                    None => Err(KeyParseError::InvalidCharacter(c)),
                }
            }
            'c' => {
                consume(chars, '-')?;
                let c = next(chars)?;
                if c.is_ascii_alphanumeric() {
                    consume(chars, '>')?;
                    Ok(Key::Ctrl(c))
                } else {
                    Err(KeyParseError::InvalidCharacter(c))
                }
            }
            'a' => {
                consume(chars, '-')?;
                let c = next(chars)?;
                if c.is_ascii_alphanumeric() {
                    consume(chars, '>')?;
                    Ok(Key::Alt(c))
                } else {
                    Err(KeyParseError::InvalidCharacter(c))
                }
            }
            c => Err(KeyParseError::InvalidCharacter(c)),
        },
        '>' => Err(KeyParseError::InvalidCharacter('>')),
        c => Ok(Key::Char(c)),
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Key::None => Ok(()),
            Key::Backspace => f.write_str("<backspace>"),
            Key::Enter => f.write_str("<enter>"),
            Key::Left => f.write_str("<left>"),
            Key::Right => f.write_str("<right>"),
            Key::Up => f.write_str("<up>"),
            Key::Down => f.write_str("<down>"),
            Key::Home => f.write_str("<home>"),
            Key::End => f.write_str("<end>"),
            Key::PageUp => f.write_str("<pageup>"),
            Key::PageDown => f.write_str("<pagedown>"),
            Key::Tab => f.write_str("<tab>"),
            Key::Delete => f.write_str("<delete>"),
            Key::F(n) => write!(f, "<f{}>", n),
            Key::Char(' ') => f.write_str("<space>"),
            Key::Char('<') => f.write_str("<less>"),
            Key::Char('>') => f.write_str("<greater>"),
            Key::Char(c) => write!(f, "{}", c),
            Key::Ctrl(c) => write!(f, "<c-{}>", c),
            Key::Alt(c) => write!(f, "<a-{}>", c),
            Key::Esc => f.write_str("<esc>"),
        }
    }
}

fn serialize_key<S>(key: Key, serializer: &mut S)
where
    S: Serializer,
{
    match key {
        Key::None => 0u8.serialize(serializer),
        Key::Backspace => 1u8.serialize(serializer),
        Key::Enter => 2u8.serialize(serializer),
        Key::Left => 3u8.serialize(serializer),
        Key::Right => 4u8.serialize(serializer),
        Key::Up => 5u8.serialize(serializer),
        Key::Down => 6u8.serialize(serializer),
        Key::Home => 7u8.serialize(serializer),
        Key::End => 8u8.serialize(serializer),
        Key::PageUp => 9u8.serialize(serializer),
        Key::PageDown => 10u8.serialize(serializer),
        Key::Tab => 11u8.serialize(serializer),
        Key::Delete => 12u8.serialize(serializer),
        Key::F(n) => {
            13u8.serialize(serializer);
            n.serialize(serializer);
        }
        Key::Char(c) => {
            14u8.serialize(serializer);
            c.serialize(serializer);
        }
        Key::Ctrl(c) => {
            15u8.serialize(serializer);
            c.serialize(serializer);
        }
        Key::Alt(c) => {
            16u8.serialize(serializer);
            c.serialize(serializer);
        }
        Key::Esc => 17u8.serialize(serializer),
    }
}

fn deserialize_key<'de, D>(deserializer: &mut D) -> Result<Key, DeserializeError>
where
    D: Deserializer<'de>,
{
    let discriminant = u8::deserialize(deserializer)?;
    match discriminant {
        0 => Ok(Key::None),
        1 => Ok(Key::Backspace),
        2 => Ok(Key::Enter),
        3 => Ok(Key::Left),
        4 => Ok(Key::Right),
        5 => Ok(Key::Up),
        6 => Ok(Key::Down),
        7 => Ok(Key::Home),
        8 => Ok(Key::End),
        9 => Ok(Key::PageUp),
        10 => Ok(Key::PageDown),
        11 => Ok(Key::Tab),
        12 => Ok(Key::Delete),
        13 => {
            let n = Serialize::deserialize(deserializer)?;
            Ok(Key::F(n))
        }
        14 => {
            let c = Serialize::deserialize(deserializer)?;
            Ok(Key::Char(c))
        }
        15 => {
            let c = Serialize::deserialize(deserializer)?;
            Ok(Key::Ctrl(c))
        }
        16 => {
            let c = Serialize::deserialize(deserializer)?;
            Ok(Key::Alt(c))
        }
        17 => Ok(Key::Esc),
        _ => Err(DeserializeError::InvalidData),
    }
}

pub enum ServerEvent<'a> {
    Display(&'a [u8]),
    Suspend,
    StdoutOutput(&'a [u8]),
}
impl<'a> ServerEvent<'a> {
    pub const fn bytes_variant_header_len() -> usize {
        1 + std::mem::size_of::<u32>()
    }

    pub fn serialize_bytes_variant_header(&self, buf: &mut [u8]) {
        buf[0] = match self {
            Self::Display(_) => 0,
            Self::Suspend => unreachable!(),
            Self::StdoutOutput(_) => 2,
        };
        let len = buf.len() as u32 - Self::bytes_variant_header_len() as u32;
        let len_buf = len.to_le_bytes();
        buf[1..Self::bytes_variant_header_len()].copy_from_slice(&len_buf);
    }
}
impl<'de> Serialize<'de> for ServerEvent<'de> {
    fn serialize<S>(&self, serializer: &mut S)
    where
        S: Serializer,
    {
        match self {
            Self::Display(display) => {
                0u8.serialize(serializer);
                display.serialize(serializer);
            }
            Self::Suspend => 1u8.serialize(serializer),
            Self::StdoutOutput(bytes) => {
                2u8.serialize(serializer);
                bytes.serialize(serializer);
            }
        }
    }

    fn deserialize<D>(deserializer: &mut D) -> Result<Self, DeserializeError>
    where
        D: Deserializer<'de>,
    {
        let discriminant = u8::deserialize(deserializer)?;
        match discriminant {
            0 => {
                let display = Serialize::deserialize(deserializer)?;
                Ok(Self::Display(display))
            }
            1 => Ok(Self::Suspend),
            2 => {
                let bytes = Serialize::deserialize(deserializer)?;
                Ok(Self::StdoutOutput(bytes))
            }
            _ => Err(DeserializeError::InvalidData),
        }
    }
}

#[derive(Clone, Copy)]
pub enum TargetClient {
    Sender,
    Focused,
}
impl<'de> Serialize<'de> for TargetClient {
    fn serialize<S>(&self, serializer: &mut S)
    where
        S: Serializer,
    {
        match self {
            Self::Sender => 0u8.serialize(serializer),
            Self::Focused => 1u8.serialize(serializer),
        }
    }

    fn deserialize<D>(deserializer: &mut D) -> Result<Self, DeserializeError>
    where
        D: Deserializer<'de>,
    {
        let discriminant = u8::deserialize(deserializer)?;
        match discriminant {
            0 => Ok(Self::Sender),
            1 => Ok(Self::Focused),
            _ => Err(DeserializeError::InvalidData),
        }
    }
}

pub enum ClientEvent<'a> {
    Key(TargetClient, Key),
    Resize(u16, u16),
    Command(TargetClient, &'a str),
    StdinInput(TargetClient, &'a [u8]),
}
impl<'de> Serialize<'de> for ClientEvent<'de> {
    fn serialize<S>(&self, serializer: &mut S)
    where
        S: Serializer,
    {
        match self {
            Self::Key(target, key) => {
                0u8.serialize(serializer);
                target.serialize(serializer);
                serialize_key(*key, serializer);
            }
            Self::Resize(width, height) => {
                1u8.serialize(serializer);
                width.serialize(serializer);
                height.serialize(serializer);
            }
            Self::Command(target, command) => {
                2u8.serialize(serializer);
                target.serialize(serializer);
                command.serialize(serializer);
            }
            Self::StdinInput(target, bytes) => {
                3u8.serialize(serializer);
                target.serialize(serializer);
                bytes.serialize(serializer);
            }
        }
    }

    fn deserialize<D>(deserializer: &mut D) -> Result<Self, DeserializeError>
    where
        D: Deserializer<'de>,
    {
        let discriminant = u8::deserialize(deserializer)?;
        match discriminant {
            0 => {
                let target = Serialize::deserialize(deserializer)?;
                let key = deserialize_key(deserializer)?;
                Ok(Self::Key(target, key))
            }
            1 => {
                let width = Serialize::deserialize(deserializer)?;
                let height = Serialize::deserialize(deserializer)?;
                Ok(Self::Resize(width, height))
            }
            2 => {
                let target = Serialize::deserialize(deserializer)?;
                let command = Serialize::deserialize(deserializer)?;
                Ok(Self::Command(target, command))
            }
            3 => {
                let target = Serialize::deserialize(deserializer)?;
                let bytes = Serialize::deserialize(deserializer)?;
                Ok(Self::StdinInput(target, bytes))
            }
            _ => Err(DeserializeError::InvalidData),
        }
    }
}

pub struct ClientEventIter {
    buf_index: usize,
    read_len: usize,
}
impl ClientEventIter {
    pub fn next<'a>(&mut self, receiver: &'a ClientEventReceiver) -> Option<ClientEvent<'a>> {
        let buf = &receiver.bufs[self.buf_index];
        let mut slice = &buf[self.read_len..];
        if slice.is_empty() {
            return None;
        }

        match ClientEvent::deserialize(&mut slice) {
            Ok(event) => {
                self.read_len = buf.len() - slice.len();
                Some(event)
            }
            Err(_) => None,
        }
    }

    pub fn finish(self, receiver: &mut ClientEventReceiver) {
        receiver.bufs[self.buf_index].drain(..self.read_len);
        std::mem::forget(self);
    }
}
impl Drop for ClientEventIter {
    fn drop(&mut self) {
        panic!("forgot to call 'finish' on ClientEventIter");
    }
}

#[derive(Default)]
pub struct ClientEventReceiver {
    bufs: Vec<Vec<u8>>,
}

impl ClientEventReceiver {
    pub fn receive_events(&mut self, client_handle: ClientHandle, bytes: &[u8]) -> ClientEventIter {
        let buf_index = client_handle.into_index();
        if buf_index >= self.bufs.len() {
            self.bufs.resize_with(buf_index + 1, Vec::new);
        }
        let buf = &mut self.bufs[buf_index];
        buf.extend_from_slice(bytes);
        ClientEventIter {
            buf_index,
            read_len: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_parsing() {
        assert_eq!(
            Key::Backspace,
            parse_key(&mut "<backspace>".chars()).unwrap()
        );
        assert_eq!(Key::Char(' '), parse_key(&mut "<space>".chars()).unwrap());
        assert_eq!(Key::Enter, parse_key(&mut "<enter>".chars()).unwrap());
        assert_eq!(Key::Left, parse_key(&mut "<left>".chars()).unwrap());
        assert_eq!(Key::Right, parse_key(&mut "<right>".chars()).unwrap());
        assert_eq!(Key::Up, parse_key(&mut "<up>".chars()).unwrap());
        assert_eq!(Key::Down, parse_key(&mut "<down>".chars()).unwrap());
        assert_eq!(Key::Home, parse_key(&mut "<home>".chars()).unwrap());
        assert_eq!(Key::End, parse_key(&mut "<end>".chars()).unwrap());
        assert_eq!(Key::PageUp, parse_key(&mut "<pageup>".chars()).unwrap());
        assert_eq!(Key::PageDown, parse_key(&mut "<pagedown>".chars()).unwrap());
        assert_eq!(Key::Tab, parse_key(&mut "<tab>".chars()).unwrap());
        assert_eq!(Key::Delete, parse_key(&mut "<delete>".chars()).unwrap());
        assert_eq!(Key::Esc, parse_key(&mut "<esc>".chars()).unwrap());

        for n in 1..=99 {
            let s = format!("<f{}>", n);
            assert_eq!(Key::F(n as _), parse_key(&mut s.chars()).unwrap());
        }

        assert_eq!(Key::Ctrl('z'), parse_key(&mut "<c-z>".chars()).unwrap());
        assert_eq!(Key::Ctrl('0'), parse_key(&mut "<c-0>".chars()).unwrap());
        assert_eq!(Key::Ctrl('9'), parse_key(&mut "<c-9>".chars()).unwrap());

        assert_eq!(Key::Alt('a'), parse_key(&mut "<a-a>".chars()).unwrap());
        assert_eq!(Key::Alt('z'), parse_key(&mut "<a-z>".chars()).unwrap());
        assert_eq!(Key::Alt('0'), parse_key(&mut "<a-0>".chars()).unwrap());
        assert_eq!(Key::Alt('9'), parse_key(&mut "<a-9>".chars()).unwrap());

        assert_eq!(Key::Char('a'), parse_key(&mut "a".chars()).unwrap());
        assert_eq!(Key::Char('z'), parse_key(&mut "z".chars()).unwrap());
        assert_eq!(Key::Char('0'), parse_key(&mut "0".chars()).unwrap());
        assert_eq!(Key::Char('9'), parse_key(&mut "9".chars()).unwrap());
        assert_eq!(Key::Char('_'), parse_key(&mut "_".chars()).unwrap());
        assert_eq!(Key::Char('<'), parse_key(&mut "<less>".chars()).unwrap());
        assert_eq!(Key::Char('>'), parse_key(&mut "<greater>".chars()).unwrap());
        assert_eq!(Key::Char('\\'), parse_key(&mut "\\".chars()).unwrap());
    }

    #[test]
    fn key_serialization() {
        fn assert_key_serialization(key: Key) {
            let mut buf = Vec::new();
            let _ = serialize_key(key, &mut buf);
            let mut slice = buf.as_slice();
            assert!(!slice.is_empty());
            match deserialize_key(&mut slice) {
                Ok(k) => assert_eq!(key, k),
                Err(_) => assert!(false),
            }
        }

        assert_key_serialization(Key::None);
        assert_key_serialization(Key::Backspace);
        assert_key_serialization(Key::Enter);
        assert_key_serialization(Key::Left);
        assert_key_serialization(Key::Right);
        assert_key_serialization(Key::Up);
        assert_key_serialization(Key::Down);
        assert_key_serialization(Key::Home);
        assert_key_serialization(Key::End);
        assert_key_serialization(Key::PageUp);
        assert_key_serialization(Key::PageDown);
        assert_key_serialization(Key::Tab);
        assert_key_serialization(Key::Delete);
        assert_key_serialization(Key::F(0));
        assert_key_serialization(Key::F(9));
        assert_key_serialization(Key::F(12));
        assert_key_serialization(Key::F(99));
        assert_key_serialization(Key::Char('a'));
        assert_key_serialization(Key::Char('z'));
        assert_key_serialization(Key::Char('A'));
        assert_key_serialization(Key::Char('Z'));
        assert_key_serialization(Key::Char('0'));
        assert_key_serialization(Key::Char('9'));
        assert_key_serialization(Key::Char('$'));
        assert_key_serialization(Key::Ctrl('a'));
        assert_key_serialization(Key::Ctrl('z'));
        assert_key_serialization(Key::Ctrl('A'));
        assert_key_serialization(Key::Ctrl('Z'));
        assert_key_serialization(Key::Ctrl('0'));
        assert_key_serialization(Key::Ctrl('9'));
        assert_key_serialization(Key::Ctrl('$'));
        assert_key_serialization(Key::Alt('a'));
        assert_key_serialization(Key::Alt('z'));
        assert_key_serialization(Key::Alt('A'));
        assert_key_serialization(Key::Alt('Z'));
        assert_key_serialization(Key::Alt('0'));
        assert_key_serialization(Key::Alt('9'));
        assert_key_serialization(Key::Alt('$'));
        assert_key_serialization(Key::Esc);
    }

    #[test]
    fn client_event_deserialize_splitted() {
        const CHAR: char = 'x';
        const EVENT_COUNT: usize = 100;

        fn check_next_event(events: &mut ClientEventIter, receiver: &ClientEventReceiver) -> bool {
            match events.next(receiver) {
                Some(ClientEvent::Key(_, Key::Char(CHAR))) => true,
                Some(ClientEvent::Key(_, Key::Char(c))) => {
                    panic!("received char {} instead of {}", c, CHAR);
                }
                Some(event) => panic!(
                    "received other kind of event. discriminant: {:?}",
                    std::mem::discriminant(&event),
                ),
                None => false,
            }
        }

        let client_handle = ClientHandle::from_index(0).unwrap();
        let event = ClientEvent::Key(TargetClient::Sender, Key::Char(CHAR));
        let mut bytes = Vec::new();
        for _ in 0..EVENT_COUNT {
            event.serialize(&mut bytes);
        }
        assert_eq!(700, bytes.len());

        let mut event_count = 0;
        let mut receiver = ClientEventReceiver::default();

        let mut events = receiver.receive_events(client_handle, &bytes[..512]);
        while check_next_event(&mut events, &receiver) {
            event_count += 1;
        }
        assert_eq!(511, events.read_len);
        events.finish(&mut receiver);

        let mut events = receiver.receive_events(client_handle, &bytes[512..]);
        while check_next_event(&mut events, &receiver) {
            event_count += 1;
        }
        events.finish(&mut receiver);

        assert_eq!(0, receiver.bufs[client_handle.into_index()].len());
        assert_eq!(EVENT_COUNT, event_count);
    }
}
