use crate::{
    client::{ClientHandle, ClientManager},
    editor::{Editor, EditorControlFlow, KeysIterator},
    platform::Platform,
};

mod command;
mod insert;
mod normal;
pub(crate) mod picker;
pub(crate) mod read_line;

pub struct ModeContext<'a> {
    pub editor: &'a mut Editor,
    pub platform: &'a mut Platform,
    pub clients: &'a mut ClientManager,
    pub client_handle: ClientHandle,
}

pub(crate) trait ModeState {
    fn on_enter(ctx: &mut ModeContext);
    fn on_exit(ctx: &mut ModeContext);
    fn on_client_keys(ctx: &mut ModeContext, keys: &mut KeysIterator) -> Option<EditorControlFlow>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModeKind {
    Normal,
    Insert,
    Command,
    ReadLine,
    Picker,
}

impl Default for ModeKind {
    fn default() -> Self {
        Self::Normal
    }
}

#[derive(Default)]
pub struct Mode {
    kind: ModeKind,

    pub normal_state: normal::State,
    pub insert_state: insert::State,
    pub command_state: command::State,
    pub read_line_state: read_line::State,
    pub picker_state: picker::State,
}

impl Mode {
    pub fn kind(&self) -> ModeKind {
        self.kind
    }

    pub fn change_to(ctx: &mut ModeContext, next: ModeKind) {
        if ctx.editor.mode.kind == next {
            return;
        }

        match ctx.editor.mode.kind {
            ModeKind::Normal => normal::State::on_exit(ctx),
            ModeKind::Insert => insert::State::on_exit(ctx),
            ModeKind::Command => command::State::on_exit(ctx),
            ModeKind::ReadLine => read_line::State::on_exit(ctx),
            ModeKind::Picker => picker::State::on_exit(ctx),
        }

        ctx.editor.mode.kind = next;

        match ctx.editor.mode.kind {
            ModeKind::Normal => normal::State::on_enter(ctx),
            ModeKind::Insert => insert::State::on_enter(ctx),
            ModeKind::Command => command::State::on_enter(ctx),
            ModeKind::ReadLine => read_line::State::on_enter(ctx),
            ModeKind::Picker => picker::State::on_enter(ctx),
        }
    }

    pub(crate) fn on_client_keys(
        ctx: &mut ModeContext,
        keys: &mut KeysIterator,
    ) -> Option<EditorControlFlow> {
        match ctx.editor.mode.kind {
            ModeKind::Normal => normal::State::on_client_keys(ctx, keys),
            ModeKind::Insert => insert::State::on_client_keys(ctx, keys),
            ModeKind::Command => command::State::on_client_keys(ctx, keys),
            ModeKind::ReadLine => read_line::State::on_client_keys(ctx, keys),
            ModeKind::Picker => picker::State::on_client_keys(ctx, keys),
        }
    }
}
