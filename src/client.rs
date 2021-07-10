use std::{fmt, str::FromStr};

use crate::{
    buffer::{BufferHandle, CharDisplayDistances},
    buffer_position::BufferPositionIndex,
    buffer_view::BufferViewHandle,
    editor::Editor,
    events::{EditorEvent, EditorEventQueue},
    navigation_history::{NavigationHistory, NavigationMovement},
    serialization::{DeserializeError, Deserializer, Serialize, Serializer},
    ui::draw_empty_view,
};

#[derive(Default, Clone, Copy, Eq, PartialEq)]
pub struct ClientHandle(u8);

impl ClientHandle {
    pub fn into_index(self) -> usize {
        self.0 as _
    }

    pub fn from_index(index: usize) -> Option<ClientHandle> {
        if index <= u8::MAX as usize {
            Some(ClientHandle(index as _))
        } else {
            None
        }
    }
}

impl<'de> Serialize<'de> for ClientHandle {
    fn serialize<S>(&self, serializer: &mut S)
    where
        S: Serializer,
    {
        self.0.serialize(serializer);
    }

    fn deserialize<D>(deserializer: &mut D) -> Result<Self, DeserializeError>
    where
        D: Deserializer<'de>,
    {
        Ok(Self(u8::deserialize(deserializer)?))
    }
}

pub struct ClientHandleFromStrError;
impl fmt::Display for ClientHandleFromStrError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("could not parse client index")
    }
}

impl FromStr for ClientHandle {
    type Err = ClientHandleFromStrError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.parse() {
            Ok(i) => Ok(Self(i)),
            Err(_) => Err(ClientHandleFromStrError),
        }
    }
}

pub struct CustomViewRenderContext<'a> {
    pub editor: &'a Editor,
    pub view_size: (u16, u16),
    pub scroll: u32,
}
pub type CustomViewRenderFn = fn(&CustomViewRenderContext, &mut Vec<u8>);

#[derive(Clone, Copy)]
pub enum ClientView {
    Buffer(BufferViewHandle),
    Custom(CustomViewRenderFn),
}
impl Default for ClientView {
    fn default() -> Self {
        Self::Custom(draw_empty_view)
    }
}
impl PartialEq for ClientView {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Buffer(a), Self::Buffer(b)) => a == b,
            _ => false,
        }
    }
}

#[derive(Default)]
pub struct Client {
    active: bool,
    handle: ClientHandle,

    pub suspended: bool,
    pub viewport_size: (u16, u16),
    pub scroll: (BufferPositionIndex, BufferPositionIndex),
    pub height: u16,
    pub navigation_history: NavigationHistory,

    view: ClientView,
}

impl Client {
    fn dispose(&mut self) {
        self.active = false;

        self.suspended = false;
        self.viewport_size = (0, 0);
        self.scroll = (0, 0);
        self.height = 0;
        self.navigation_history.clear();

        self.view = ClientView::default();
    }

    pub fn handle(&self) -> ClientHandle {
        self.handle
    }

    pub fn view(&self) -> ClientView {
        self.view
    }

    pub fn buffer_view_handle(&self) -> Option<BufferViewHandle> {
        match self.view {
            ClientView::Buffer(handle) => Some(handle),
            _ => None,
        }
    }

    pub fn on_buffer_close(&mut self, editor: &mut Editor, buffer_handle: BufferHandle) {
        self.navigation_history
            .remove_snapshots_with_buffer_handle(buffer_handle);
        NavigationHistory::move_in_history(self, editor, NavigationMovement::PreviousBuffer);
    }

    pub fn set_view(&mut self, view: ClientView, events: &mut EditorEventQueue) {
        match (self.view, view) {
            (ClientView::Buffer(current), ClientView::Buffer(next)) => {
                if current != next {
                    events.enqueue(EditorEvent::BufferViewLostFocus { handle: current });
                }
            }
            _ => (),
        }
        self.view = view;
    }

    pub fn has_ui(&self) -> bool {
        self.viewport_size.0 != 0 && self.viewport_size.0 != 0
    }

    pub fn update_view(&mut self, editor: &Editor, picker_height: u16) {
        fn calculate_scroll(
            this: &Client,
            editor: &Editor,
        ) -> Option<(BufferPositionIndex, BufferPositionIndex)> {
            let width = this.viewport_size.0 as BufferPositionIndex;
            if width == 0 {
                return None;
            }

            let buffer_view = editor.buffer_views.get(this.buffer_view_handle()?)?;
            let buffer = editor.buffers.get(buffer_view.buffer_handle)?.content();

            let position = buffer_view.cursors.main_cursor().position;

            let line_index = position.line_index;
            let line = buffer.line_at(line_index as _).as_str();
            let column_index = position.column_byte_index;

            let height = this.height as BufferPositionIndex;
            let half_height = height / 2;

            let (mut scroll_x, mut scroll_y) = this.scroll;

            if column_index < scroll_x {
                scroll_x = column_index
            } else {
                let index = column_index as usize;
                let (width, text) = match line[index..].chars().next() {
                    Some(c) => (width, &line[..index + c.len_utf8()]),
                    None => (width - 1, line),
                };

                if let Some(d) = CharDisplayDistances::new(text, editor.config.tab_size)
                    .rev()
                    .take_while(|d| d.distance <= width as _)
                    .last()
                {
                    scroll_x = scroll_x.max(d.char_index as _);
                }
            }

            if line_index < scroll_y.saturating_sub(half_height) {
                scroll_y = line_index.saturating_sub(half_height);
            } else if line_index < scroll_y {
                scroll_y = line_index;
            } else if line_index >= scroll_y + height + half_height {
                scroll_y = line_index + 1 - half_height;
            } else if line_index >= scroll_y + height {
                scroll_y = line_index + 1 - height;
            }

            Some((scroll_x, scroll_y))
        }

        self.height = self.viewport_size.1.saturating_sub(1 + picker_height);
        if let Some(scroll) = calculate_scroll(self, editor) {
            self.scroll = scroll;
        }
    }
}

#[derive(Default)]
pub struct ClientManager {
    focused_client: Option<ClientHandle>,
    previous_focused_client: Option<ClientHandle>,
    clients: Vec<Client>,
}

impl ClientManager {
    pub fn focused_client(&self) -> Option<ClientHandle> {
        self.focused_client
    }

    pub fn previous_focused_client(&self) -> Option<ClientHandle> {
        self.previous_focused_client
    }

    pub fn focus_client(&mut self, handle: ClientHandle) -> bool {
        if let Some(client) = self.get(handle) {
            if !client.has_ui() {
                return false;
            }
        }

        let changed = Some(handle) != self.focused_client;
        if changed {
            self.previous_focused_client = self.focused_client;
        }
        self.focused_client = Some(handle);
        changed
    }

    pub fn on_client_joined(&mut self, handle: ClientHandle) {
        let min_len = handle.into_index() + 1;
        if min_len > self.clients.len() {
            self.clients.resize_with(min_len, Default::default);
        }

        let client = &mut self.clients[handle.into_index()];
        client.active = true;
        client.handle = handle;
    }

    pub fn on_client_left(&mut self, handle: ClientHandle) {
        self.clients[handle.into_index()].dispose();
        if self.focused_client == Some(handle) {
            self.focused_client = None;
        }
    }

    pub fn get(&self, handle: ClientHandle) -> Option<&Client> {
        let client = &self.clients[handle.into_index()];
        if client.active {
            Some(client)
        } else {
            None
        }
    }

    pub fn get_mut(&mut self, handle: ClientHandle) -> Option<&mut Client> {
        let client = &mut self.clients[handle.into_index()];
        if client.active {
            Some(client)
        } else {
            None
        }
    }

    pub fn iter(&self) -> impl Clone + Iterator<Item = &Client> {
        self.clients.iter().filter(|c| c.active)
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Client> {
        self.clients.iter_mut().filter(|c| c.active)
    }
}

