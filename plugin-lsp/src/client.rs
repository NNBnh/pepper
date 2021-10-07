use std::{
    cmp::Ord,
    fmt,
    fs::File,
    io,
    ops::Range,
    path::{Path, PathBuf},
    str::FromStr,
};

use pepper::{
    buffer::{BufferContent, BufferHandle, BufferProperties},
    buffer_position::{BufferPosition, BufferRange},
    buffer_view::BufferViewHandle,
    client,
    cursor::Cursor,
    editor::Editor,
    editor_utils::{MessageKind, StatusBar},
    events::{EditorEvent, EditorEventIter},
    glob::Glob,
    mode::{ModeContext, ModeKind},
    navigation_history::NavigationHistory,
    picker::Picker,
    platform::{Platform, ProcessId},
    word_database::{WordIndicesIter, WordKind},
};

use crate::{
    capabilities,
    json::{
        FromJson, Json, JsonArray, JsonConvertError, JsonInteger, JsonObject, JsonString, JsonValue,
    },
    mode::{picker, read_line},
    protocol::{
        self, DocumentCodeAction, DocumentCompletionItem, DocumentDiagnostic, DocumentLocation,
        DocumentPosition, DocumentRange, DocumentSymbolInformation, PendingRequestColection,
        Protocol, ProtocolError, ResponseError, ServerNotification, ServerRequest, ServerResponse,
        TextEdit, Uri, WorkspaceEdit,
    },
};

#[derive(Default)]
struct GenericCapability(pub bool);
impl<'json> FromJson<'json> for GenericCapability {
    fn from_json(value: JsonValue, _: &'json Json) -> Result<Self, JsonConvertError> {
        match value {
            JsonValue::Null => Ok(Self(false)),
            JsonValue::Boolean(b) => Ok(Self(b)),
            JsonValue::Object(_) => Ok(Self(true)),
            _ => Err(JsonConvertError),
        }
    }
}

#[derive(Default)]
struct TriggerCharactersCapability {
    pub on: bool,
    pub trigger_characters: String,
}
impl<'json> FromJson<'json> for TriggerCharactersCapability {
    fn from_json(value: JsonValue, json: &'json Json) -> Result<Self, JsonConvertError> {
        match value {
            JsonValue::Null => Ok(Self {
                on: false,
                trigger_characters: String::new(),
            }),
            JsonValue::Object(options) => {
                let mut trigger_characters = String::new();
                for c in options.get("triggerCharacters", json).elements(json) {
                    if let JsonValue::String(c) = c {
                        let c = c.as_str(json);
                        trigger_characters.push_str(c);
                    }
                }
                Ok(Self {
                    on: true,
                    trigger_characters,
                })
            }
            _ => Err(JsonConvertError),
        }
    }
}

#[derive(Default)]
struct RenameCapability {
    pub on: bool,
    pub prepare_provider: bool,
}
impl<'json> FromJson<'json> for RenameCapability {
    fn from_json(value: JsonValue, json: &'json Json) -> Result<Self, JsonConvertError> {
        match value {
            JsonValue::Null => Ok(Self {
                on: false,
                prepare_provider: false,
            }),
            JsonValue::Boolean(b) => Ok(Self {
                on: b,
                prepare_provider: false,
            }),
            JsonValue::Object(options) => Ok(Self {
                on: true,
                prepare_provider: matches!(
                    options.get("prepareProvider", &json),
                    JsonValue::Boolean(true)
                ),
            }),
            _ => Err(JsonConvertError),
        }
    }
}

enum TextDocumentSyncKind {
    None,
    Full,
    Incremental,
}
struct TextDocumentSyncCapability {
    pub open_close: bool,
    pub change: TextDocumentSyncKind,
    pub save: TextDocumentSyncKind,
}
impl Default for TextDocumentSyncCapability {
    fn default() -> Self {
        Self {
            open_close: false,
            change: TextDocumentSyncKind::None,
            save: TextDocumentSyncKind::None,
        }
    }
}
impl<'json> FromJson<'json> for TextDocumentSyncCapability {
    fn from_json(value: JsonValue, json: &'json Json) -> Result<Self, JsonConvertError> {
        match value {
            JsonValue::Integer(0) => Ok(Self {
                open_close: false,
                change: TextDocumentSyncKind::None,
                save: TextDocumentSyncKind::None,
            }),
            JsonValue::Integer(1) => Ok(Self {
                open_close: true,
                change: TextDocumentSyncKind::Full,
                save: TextDocumentSyncKind::Full,
            }),
            JsonValue::Integer(2) => Ok(Self {
                open_close: true,
                change: TextDocumentSyncKind::Incremental,
                save: TextDocumentSyncKind::Incremental,
            }),
            JsonValue::Object(options) => {
                let mut open_close = false;
                let mut change = TextDocumentSyncKind::None;
                let mut save = TextDocumentSyncKind::None;
                for (key, value) in options.members(json) {
                    match key {
                        "change" => {
                            change = match value {
                                JsonValue::Integer(0) => TextDocumentSyncKind::None,
                                JsonValue::Integer(1) => TextDocumentSyncKind::Full,
                                JsonValue::Integer(2) => TextDocumentSyncKind::Incremental,
                                _ => return Err(JsonConvertError),
                            }
                        }
                        "openClose" => {
                            open_close = match value {
                                JsonValue::Boolean(b) => b,
                                _ => return Err(JsonConvertError),
                            }
                        }
                        "save" => {
                            save = match value {
                                JsonValue::Boolean(false) => TextDocumentSyncKind::None,
                                JsonValue::Boolean(true) => TextDocumentSyncKind::Incremental,
                                JsonValue::Object(options) => {
                                    match options.get("includeText", json) {
                                        JsonValue::Boolean(true) => TextDocumentSyncKind::Full,
                                        _ => TextDocumentSyncKind::Incremental,
                                    }
                                }
                                _ => return Err(JsonConvertError),
                            }
                        }
                        _ => (),
                    }
                }
                Ok(Self {
                    open_close,
                    change,
                    save,
                })
            }
            _ => Err(JsonConvertError),
        }
    }
}

#[derive(Default)]
struct ServerCapabilities {
    text_document_sync: TextDocumentSyncCapability,
    completion_provider: TriggerCharactersCapability,
    hover_provider: GenericCapability,
    signature_help_provider: TriggerCharactersCapability,
    declaration_provider: GenericCapability,
    definition_provider: GenericCapability,
    implementation_provider: GenericCapability,
    references_provider: GenericCapability,
    document_symbol_provider: GenericCapability,
    code_action_provider: GenericCapability,
    document_formatting_provider: GenericCapability,
    rename_provider: RenameCapability,
    workspace_symbol_provider: GenericCapability,
}
impl<'json> FromJson<'json> for ServerCapabilities {
    fn from_json(value: JsonValue, json: &'json Json) -> Result<Self, JsonConvertError> {
        let mut this = Self::default();
        for (key, value) in value.members(json) {
            match key {
                "textDocumentSync" => this.text_document_sync = FromJson::from_json(value, json)?,
                "completionProvider" => {
                    this.completion_provider = FromJson::from_json(value, json)?
                }
                "hoverProvider" => this.hover_provider = FromJson::from_json(value, json)?,
                "signatureHelpProvider" => {
                    this.signature_help_provider = FromJson::from_json(value, json)?
                }
                "declarationProvider" => {
                    this.declaration_provider = FromJson::from_json(value, json)?
                }
                "definitionProvider" => {
                    this.definition_provider = FromJson::from_json(value, json)?
                }
                "implementationProvider" => {
                    this.implementation_provider = FromJson::from_json(value, json)?
                }
                "referencesProvider" => {
                    this.references_provider = FromJson::from_json(value, json)?
                }
                "documentSymbolProvider" => {
                    this.document_symbol_provider = FromJson::from_json(value, json)?
                }
                "codeActionProvider" => {
                    this.code_action_provider = FromJson::from_json(value, json)?
                }
                "documentFormattingProvider" => {
                    this.document_formatting_provider = FromJson::from_json(value, json)?
                }
                "renameProvider" => this.rename_provider = FromJson::from_json(value, json)?,
                "workspaceSymbolProvider" => {
                    this.workspace_symbol_provider = FromJson::from_json(value, json)?
                }
                _ => (),
            }
        }
        Ok(this)
    }
}

pub struct Diagnostic {
    pub message: String,
    pub range: BufferRange,
    data: Vec<u8>,
}
impl Diagnostic {
    pub fn as_document_diagnostic(&self, json: &mut Json) -> DocumentDiagnostic {
        let mut reader = io::Cursor::new(&self.data);
        let data = match json.read(&mut reader) {
            Ok(value) => value,
            Err(_) => JsonValue::Null,
        };
        let range = DocumentRange::from_buffer_range(self.range);
        DocumentDiagnostic {
            message: json.create_string(&self.message),
            range,
            data,
        }
    }
}

struct BufferDiagnosticCollection {
    path: PathBuf,
    buffer_handle: Option<BufferHandle>,
    diagnostics: Vec<Diagnostic>,
    len: usize,
}
impl BufferDiagnosticCollection {
    pub fn add(&mut self, diagnostic: DocumentDiagnostic, json: &Json) {
        let message = diagnostic.message.as_str(json);
        let range = diagnostic.range.into_buffer_range();

        if self.len < self.diagnostics.len() {
            let diagnostic = &mut self.diagnostics[self.len];
            diagnostic.message.clear();
            diagnostic.message.push_str(message);
            diagnostic.range = range;
            diagnostic.data.clear();
        } else {
            self.diagnostics.push(Diagnostic {
                message: message.into(),
                range,
                data: Vec::new(),
            });
        }

        let _ = json.write(&mut self.diagnostics[self.len].data, &diagnostic.data);
        self.len += 1;
    }

    pub fn sort(&mut self) {
        self.diagnostics.sort_unstable_by_key(|d| d.range.from);
    }
}

fn is_editor_path_equals_to_lsp_path(
    editor_root: &Path,
    editor_path: &Path,
    lsp_root: &Path,
    lsp_path: &Path,
) -> bool {
    let lsp_components = lsp_root.components().chain(lsp_path.components());
    if editor_path.is_absolute() {
        editor_path.components().eq(lsp_components)
    } else {
        editor_root
            .components()
            .chain(editor_path.components())
            .eq(lsp_components)
    }
}

struct VersionedBufferEdit {
    buffer_range: BufferRange,
    text_range: Range<u32>,
}
struct VersionedBuffer {
    version: usize,
    texts: String,
    pending_edits: Vec<VersionedBufferEdit>,
}
impl VersionedBuffer {
    pub fn new() -> Self {
        Self {
            version: 2,
            texts: String::new(),
            pending_edits: Vec::new(),
        }
    }

    pub fn flush(&mut self) {
        self.texts.clear();
        self.pending_edits.clear();
        self.version += 1;
    }

    pub fn dispose(&mut self) {
        self.flush();
        self.version = 1;
    }
}
#[derive(Default)]
struct VersionedBufferCollection {
    buffers: Vec<VersionedBuffer>,
}
impl VersionedBufferCollection {
    pub fn add_edit(&mut self, buffer_handle: BufferHandle, range: BufferRange, text: &str) {
        let index = buffer_handle.0 as usize;
        if index >= self.buffers.len() {
            self.buffers.resize_with(index + 1, VersionedBuffer::new);
        }
        let buffer = &mut self.buffers[index];
        let text_range_start = buffer.texts.len();
        buffer.texts.push_str(text);
        buffer.pending_edits.push(VersionedBufferEdit {
            buffer_range: range,
            text_range: text_range_start as u32..buffer.texts.len() as u32,
        });
    }

    pub fn dispose(&mut self, buffer_handle: BufferHandle) {
        if let Some(buffer) = self.buffers.get_mut(buffer_handle.0 as usize) {
            buffer.dispose();
        }
    }

    pub fn iter_pending_mut(
        &mut self,
    ) -> impl Iterator<Item = (BufferHandle, &mut VersionedBuffer)> {
        self.buffers
            .iter_mut()
            .enumerate()
            .filter(|(_, e)| !e.pending_edits.is_empty())
            .map(|(i, e)| (BufferHandle(i as _), e))
    }
}

#[derive(Default)]
pub struct DiagnosticCollection {
    buffer_diagnostics: Vec<BufferDiagnosticCollection>,
}
impl DiagnosticCollection {
    pub fn buffer_diagnostics(&self, buffer_handle: BufferHandle) -> &[Diagnostic] {
        for diagnostics in &self.buffer_diagnostics {
            if diagnostics.buffer_handle == Some(buffer_handle) {
                return &diagnostics.diagnostics[..diagnostics.len];
            }
        }
        &[]
    }

    pub fn iter(
        &self,
    ) -> impl DoubleEndedIterator<Item = (&Path, Option<BufferHandle>, &[Diagnostic])> {
        self.buffer_diagnostics
            .iter()
            .map(|d| (d.path.as_path(), d.buffer_handle, &d.diagnostics[..d.len]))
    }

    pub(self) fn diagnostics_at_path(
        &mut self,
        editor: &Editor,
        root: &Path,
        path: &Path,
    ) -> &mut BufferDiagnosticCollection {
        fn find_buffer_with_path(
            editor: &Editor,
            root: &Path,
            path: &Path,
        ) -> Option<BufferHandle> {
            for buffer in editor.buffers.iter() {
                if is_editor_path_equals_to_lsp_path(
                    &editor.current_directory,
                    &buffer.path,
                    root,
                    path,
                ) {
                    return Some(buffer.handle());
                }
            }
            None
        }

        for i in 0..self.buffer_diagnostics.len() {
            if self.buffer_diagnostics[i].path == path {
                let diagnostics = &mut self.buffer_diagnostics[i];
                diagnostics.len = 0;

                if diagnostics.buffer_handle.is_none() {
                    diagnostics.buffer_handle = find_buffer_with_path(editor, root, path);
                }

                return diagnostics;
            }
        }

        let end_index = self.buffer_diagnostics.len();
        self.buffer_diagnostics.push(BufferDiagnosticCollection {
            path: path.into(),
            buffer_handle: find_buffer_with_path(editor, root, path),
            diagnostics: Vec::new(),
            len: 0,
        });
        &mut self.buffer_diagnostics[end_index]
    }

    pub(self) fn clear_empty(&mut self) {
        for i in (0..self.buffer_diagnostics.len()).rev() {
            if self.buffer_diagnostics[i].len == 0 {
                self.buffer_diagnostics.swap_remove(i);
            }
        }
    }

    pub(self) fn on_load_buffer(
        &mut self,
        editor: &Editor,
        buffer_handle: BufferHandle,
        root: &Path,
    ) {
        let buffer_path = &editor.buffers.get(buffer_handle).path;
        for diagnostics in &mut self.buffer_diagnostics {
            if diagnostics.buffer_handle.is_none()
                && is_editor_path_equals_to_lsp_path(
                    &editor.current_directory,
                    buffer_path,
                    root,
                    &diagnostics.path,
                )
            {
                diagnostics.buffer_handle = Some(buffer_handle);
                return;
            }
        }
    }

    pub(self) fn on_save_buffer(
        &mut self,
        editor: &Editor,
        buffer_handle: BufferHandle,
        root: &Path,
    ) {
        let buffer_path = &editor.buffers.get(buffer_handle).path;
        for diagnostics in &mut self.buffer_diagnostics {
            if diagnostics.buffer_handle == Some(buffer_handle) {
                diagnostics.buffer_handle = None;
                if is_editor_path_equals_to_lsp_path(
                    &editor.current_directory,
                    buffer_path,
                    root,
                    &diagnostics.path,
                ) {
                    diagnostics.buffer_handle = Some(buffer_handle);
                    return;
                }
            }
        }
    }

    pub(self) fn on_close_buffer(&mut self, buffer_handle: BufferHandle) {
        for diagnostics in &mut self.buffer_diagnostics {
            if diagnostics.buffer_handle == Some(buffer_handle) {
                diagnostics.buffer_handle = None;
                return;
            }
        }
    }
}

enum RequestState {
    Idle,
    Definition {
        client_handle: client::ClientHandle,
    },
    Declaration {
        client_handle: client::ClientHandle,
    },
    Implementation {
        client_handle: client::ClientHandle,
    },
    References {
        client_handle: client::ClientHandle,
        context_len: usize,
        auto_close_buffer: bool,
    },
    Rename {
        client_handle: client::ClientHandle,
        buffer_handle: BufferHandle,
        buffer_position: BufferPosition,
    },
    FinishRename {
        buffer_handle: BufferHandle,
        buffer_position: BufferPosition,
    },
    CodeAction {
        client_handle: client::ClientHandle,
    },
    FinishCodeAction,
    DocumentSymbols {
        client_handle: client::ClientHandle,
        buffer_view_handle: BufferViewHandle,
    },
    FinishDocumentSymbols {
        buffer_view_handle: BufferViewHandle,
    },
    WorkspaceSymbols {
        client_handle: client::ClientHandle,
    },
    FinishWorkspaceSymbols,
    Formatting {
        buffer_handle: BufferHandle,
    },
    Completion {
        client_handle: client::ClientHandle,
        buffer_handle: BufferHandle,
    },
}
impl RequestState {
    pub fn is_idle(&self) -> bool {
        matches!(self, RequestState::Idle)
    }
}

pub enum ClientOperation {
    None,
    EnteredReadLineMode,
    EnteredPickerMode,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ClientHandle(pub(crate) u8);
impl fmt::Display for ClientHandle {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl FromStr for ClientHandle {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.parse() {
            Ok(i) => Ok(Self(i)),
            Err(_) => Err(()),
        }
    }
}

pub struct Client {
    handle: ClientHandle,
    process_id: ProcessId,
    pub(crate) protocol: Protocol,
    pub(crate) json: Json,
    root: PathBuf,
    pending_requests: PendingRequestColection,

    initialized: bool,
    server_capabilities: ServerCapabilities,

    document_selectors: Vec<Glob>,
    versioned_buffers: VersionedBufferCollection,
    diagnostics: DiagnosticCollection,

    temp_edits: Vec<(BufferRange, BufferRange)>,

    request_state: RequestState,
    request_raw_json: Vec<u8>,

    log_file_path: String,
    log_file: Option<io::BufWriter<File>>,
}

impl Client {
    pub(crate) fn new(
        handle: ClientHandle,
        process_id: ProcessId,
        root: PathBuf,
        log_file_path: Option<String>,
    ) -> Self {
        let (log_file_path, log_file) = match log_file_path {
            Some(path) => match File::create(&path) {
                Ok(file) => (path, Some(io::BufWriter::new(file))),
                Err(_) => (String::new(), None),
            },
            None => (String::new(), None),
        };

        Self {
            handle,
            process_id,
            protocol: Protocol::new(),
            json: Json::new(),
            root,
            pending_requests: PendingRequestColection::default(),

            initialized: false,
            server_capabilities: ServerCapabilities::default(),

            document_selectors: Vec::new(),
            versioned_buffers: VersionedBufferCollection::default(),
            diagnostics: DiagnosticCollection::default(),

            request_state: RequestState::Idle,
            request_raw_json: Vec::new(),
            temp_edits: Vec::new(),

            log_file_path,
            log_file,
        }
    }

    pub fn handle(&self) -> ClientHandle {
        self.handle
    }

    pub fn process_id(&self) -> ProcessId {
        self.process_id
    }

    pub fn handles_path(&self, path: &str) -> bool {
        if self.document_selectors.is_empty() {
            true
        } else {
            self.document_selectors.iter().any(|g| g.matches(path))
        }
    }

    pub fn log_file_path(&self) -> Option<&str> {
        if self.log_file_path.is_empty() {
            None
        } else {
            Some(&self.log_file_path)
        }
    }

    pub fn diagnostics(&self) -> &DiagnosticCollection {
        &self.diagnostics
    }

    pub fn signature_help_triggers(&self) -> &str {
        &self
            .server_capabilities
            .signature_help_provider
            .trigger_characters
    }

    pub fn completion_triggers(&self) -> &str {
        &self
            .server_capabilities
            .completion_provider
            .trigger_characters
    }

    pub fn cancel_current_request(&mut self) {
        self.request_state = RequestState::Idle;
    }

    pub fn hover(
        &mut self,
        editor: &Editor,
        platform: &mut Platform,
        buffer_handle: BufferHandle,
        buffer_position: BufferPosition,
    ) -> ClientOperation {
        if !self.server_capabilities.hover_provider.0 {
            return ClientOperation::None;
        }

        util::send_pending_did_change(self, editor, platform);

        let buffer = editor.buffers.get(buffer_handle);
        let text_document = util::text_document_with_id(&self.root, &buffer.path, &mut self.json);
        let position = DocumentPosition::from_buffer_position(buffer_position);

        let mut params = JsonObject::default();
        params.set("textDocument".into(), text_document.into(), &mut self.json);
        params.set(
            "position".into(),
            position.to_json_value(&mut self.json),
            &mut self.json,
        );

        self.request(platform, "textDocument/hover", params);

        ClientOperation::None
    }

    pub fn signature_help(
        &mut self,
        editor: &Editor,
        platform: &mut Platform,
        buffer_handle: BufferHandle,
        buffer_position: BufferPosition,
    ) -> ClientOperation {
        if !self.server_capabilities.signature_help_provider.on {
            return ClientOperation::None;
        }

        util::send_pending_did_change(self, editor, platform);

        let buffer = editor.buffers.get(buffer_handle);
        let text_document = util::text_document_with_id(&self.root, &buffer.path, &mut self.json);
        let position = DocumentPosition::from_buffer_position(buffer_position);

        let mut params = JsonObject::default();
        params.set("textDocument".into(), text_document.into(), &mut self.json);
        params.set(
            "position".into(),
            position.to_json_value(&mut self.json),
            &mut self.json,
        );

        self.request(platform, "textDocument/signatureHelp", params);

        ClientOperation::None
    }

    pub fn definition(
        &mut self,
        editor: &Editor,
        platform: &mut Platform,
        buffer_handle: BufferHandle,
        buffer_position: BufferPosition,
        client_handle: client::ClientHandle,
    ) {
        if !self.server_capabilities.definition_provider.0 || !self.request_state.is_idle() {
            return;
        }

        let params =
            util::create_definition_params(self, editor, platform, buffer_handle, buffer_position);
        self.request_state = RequestState::Definition { client_handle };
        self.request(platform, "textDocument/definition", params);
    }

    pub fn declaration(
        &mut self,
        editor: &Editor,
        platform: &mut Platform,
        buffer_handle: BufferHandle,
        buffer_position: BufferPosition,
        client_handle: client::ClientHandle,
    ) {
        if !self.server_capabilities.declaration_provider.0 || !self.request_state.is_idle() {
            return;
        }

        let params =
            util::create_definition_params(self, editor, platform, buffer_handle, buffer_position);
        self.request_state = RequestState::Declaration { client_handle };
        self.request(platform, "textDocument/declaration", params);
    }

    pub fn implementation(
        &mut self,
        editor: &Editor,
        platform: &mut Platform,
        buffer_handle: BufferHandle,
        buffer_position: BufferPosition,
        client_handle: client::ClientHandle,
    ) {
        if !self.server_capabilities.implementation_provider.0 || !self.request_state.is_idle() {
            return;
        }

        let params =
            util::create_definition_params(self, editor, platform, buffer_handle, buffer_position);
        self.request_state = RequestState::Implementation { client_handle };
        self.request(platform, "textDocument/implementation", params);
    }

    pub fn references(
        &mut self,
        editor: &Editor,
        platform: &mut Platform,
        buffer_handle: BufferHandle,
        buffer_position: BufferPosition,
        context_len: usize,
        auto_close_buffer: bool,
        client_handle: client::ClientHandle,
    ) {
        if !self.server_capabilities.references_provider.0 || !self.request_state.is_idle() {
            return;
        }

        util::send_pending_did_change(self, editor, platform);

        let buffer = editor.buffers.get(buffer_handle);
        let text_document = util::text_document_with_id(&self.root, &buffer.path, &mut self.json);
        let position = DocumentPosition::from_buffer_position(buffer_position);

        let mut context = JsonObject::default();
        context.set("includeDeclaration".into(), true.into(), &mut self.json);

        let mut params = JsonObject::default();
        params.set("textDocument".into(), text_document.into(), &mut self.json);
        params.set(
            "position".into(),
            position.to_json_value(&mut self.json),
            &mut self.json,
        );
        params.set("context".into(), context.into(), &mut self.json);

        self.request_state = RequestState::References {
            client_handle,
            context_len,
            auto_close_buffer,
        };
        self.request(platform, "textDocument/references", params);
    }

    pub fn rename(
        &mut self,
        editor: &mut Editor,
        platform: &mut Platform,
        clients: &mut client::ClientManager,
        client_handle: client::ClientHandle,
        buffer_handle: BufferHandle,
        buffer_position: BufferPosition,
    ) -> ClientOperation {
        if !self.server_capabilities.rename_provider.on || !self.request_state.is_idle() {
            return ClientOperation::None;
        }

        util::send_pending_did_change(self, editor, platform);

        let buffer = editor.buffers.get(buffer_handle);
        let text_document = util::text_document_with_id(&self.root, &buffer.path, &mut self.json);
        let position = DocumentPosition::from_buffer_position(buffer_position);

        let mut params = JsonObject::default();
        params.set("textDocument".into(), text_document.into(), &mut self.json);
        params.set(
            "position".into(),
            position.to_json_value(&mut self.json),
            &mut self.json,
        );

        if self.server_capabilities.rename_provider.prepare_provider {
            self.request_state = RequestState::Rename {
                client_handle,
                buffer_handle,
                buffer_position,
            };
            self.request(platform, "textDocument/prepareRename", params);

            ClientOperation::None
        } else {
            self.request_state = RequestState::FinishRename {
                buffer_handle,
                buffer_position,
            };
            let mut ctx = ModeContext {
                editor,
                platform,
                clients,
                client_handle,
            };

            read_line::enter_rename_mode(&mut ctx, "")
        }
    }

    pub(crate) fn finish_rename(&mut self, editor: &Editor, platform: &mut Platform) {
        let (buffer_handle, buffer_position) = match self.request_state {
            RequestState::FinishRename {
                buffer_handle,
                buffer_position,
            } => (buffer_handle, buffer_position),
            _ => return,
        };
        self.request_state = RequestState::Idle;
        if !self.server_capabilities.rename_provider.on {
            return;
        }

        util::send_pending_did_change(self, editor, platform);

        let buffer = editor.buffers.get(buffer_handle);
        let text_document = util::text_document_with_id(&self.root, &buffer.path, &mut self.json);
        let position = DocumentPosition::from_buffer_position(buffer_position);
        let new_name = self.json.create_string(editor.read_line.input());

        let mut params = JsonObject::default();
        params.set("textDocument".into(), text_document.into(), &mut self.json);
        params.set(
            "position".into(),
            position.to_json_value(&mut self.json),
            &mut self.json,
        );
        params.set("newName".into(), new_name.into(), &mut self.json);

        self.request(platform, "textDocument/rename", params);
    }

    pub fn code_action(
        &mut self,
        editor: &Editor,
        platform: &mut Platform,
        client_handle: client::ClientHandle,
        buffer_handle: BufferHandle,
        range: BufferRange,
    ) {
        if !self.server_capabilities.code_action_provider.0 || !self.request_state.is_idle() {
            return;
        }

        util::send_pending_did_change(self, editor, platform);

        let buffer = editor.buffers.get(buffer_handle);
        let text_document = util::text_document_with_id(&self.root, &buffer.path, &mut self.json);

        let mut diagnostics = JsonArray::default();
        for diagnostic in self.diagnostics.buffer_diagnostics(buffer_handle) {
            if diagnostic.range.from <= range.from && range.from < diagnostic.range.to
                || diagnostic.range.from <= range.to && range.to < diagnostic.range.to
            {
                let diagnostic = diagnostic.as_document_diagnostic(&mut self.json);
                diagnostics.push(diagnostic.to_json_value(&mut self.json), &mut self.json);
            }
        }

        let mut context = JsonObject::default();
        context.set("diagnostics".into(), diagnostics.into(), &mut self.json);

        let mut params = JsonObject::default();
        params.set("textDocument".into(), text_document.into(), &mut self.json);
        params.set(
            "range".into(),
            DocumentRange::from_buffer_range(range).to_json_value(&mut self.json),
            &mut self.json,
        );
        params.set("context".into(), context.into(), &mut self.json);

        self.request_state = RequestState::CodeAction { client_handle };
        self.request(platform, "textDocument/codeAction", params);
    }

    pub(crate) fn finish_code_action(&mut self, editor: &mut Editor, index: usize) {
        match self.request_state {
            RequestState::FinishCodeAction => (),
            _ => return,
        }
        self.request_state = RequestState::Idle;
        if !self.server_capabilities.code_action_provider.0 {
            return;
        }

        let mut reader = io::Cursor::new(&self.request_raw_json);
        let code_actions = match self.json.read(&mut reader) {
            Ok(actions) => actions,
            Err(_) => return,
        };
        if let Some(edit) = code_actions
            .elements(&self.json)
            .filter_map(|a| DocumentCodeAction::from_json(a, &self.json).ok())
            .filter(|a| !a.disabled)
            .map(|a| a.edit)
            .nth(index)
        {
            edit.apply(editor, &mut self.temp_edits, &self.root, &self.json);
        }
    }

    pub fn document_symbols(
        &mut self,
        editor: &Editor,
        platform: &mut Platform,
        client_handle: client::ClientHandle,
        buffer_view_handle: BufferViewHandle,
    ) {
        if !self.server_capabilities.document_symbol_provider.0 || !self.request_state.is_idle() {
            return;
        }

        util::send_pending_did_change(self, editor, platform);

        let buffer_handle = editor.buffer_views.get(buffer_view_handle).buffer_handle;
        let buffer_path = &editor.buffers.get(buffer_handle).path;
        let text_document = util::text_document_with_id(&self.root, buffer_path, &mut self.json);

        let mut params = JsonObject::default();
        params.set("textDocument".into(), text_document.into(), &mut self.json);

        self.request_state = RequestState::DocumentSymbols {
            client_handle,
            buffer_view_handle,
        };
        self.request(platform, "textDocument/documentSymbol", params);
    }

    pub(crate) fn finish_document_symbols(
        &mut self,
        editor: &mut Editor,
        clients: &mut client::ClientManager,
        client_handle: client::ClientHandle,
        index: usize,
    ) {
        let buffer_view_handle = match self.request_state {
            RequestState::FinishDocumentSymbols { buffer_view_handle } => buffer_view_handle,
            _ => return,
        };
        self.request_state = RequestState::Idle;
        if !self.server_capabilities.document_symbol_provider.0 {
            return;
        }

        let mut reader = io::Cursor::new(&self.request_raw_json);
        let symbols = match self.json.read(&mut reader) {
            Ok(symbols) => match symbols {
                JsonValue::Array(symbols) => symbols,
                _ => return,
            },
            Err(_) => return,
        };

        fn find_symbol_position(
            symbols: JsonArray,
            json: &Json,
            mut index: usize,
        ) -> Result<DocumentPosition, usize> {
            for symbol in symbols
                .elements(json)
                .filter_map(|s| DocumentSymbolInformation::from_json(s, json).ok())
            {
                if index == 0 {
                    return Ok(symbol.range.start);
                } else {
                    match find_symbol_position(symbol.children.clone(), json, index - 1) {
                        Ok(position) => return Ok(position),
                        Err(i) => index = i,
                    }
                }
            }
            Err(index)
        }

        if let Ok(position) = find_symbol_position(symbols, &self.json, index) {
            NavigationHistory::save_snapshot(clients.get_mut(client_handle), &editor.buffer_views);

            let buffer_view = editor.buffer_views.get_mut(buffer_view_handle);
            let position = position.into_buffer_position();
            let mut cursors = buffer_view.cursors.mut_guard();
            cursors.clear();
            cursors.add(Cursor {
                anchor: position,
                position,
            });
        }
    }

    pub fn workspace_symbols(
        &mut self,
        editor: &Editor,
        platform: &mut Platform,
        client_handle: client::ClientHandle,
        query: &str,
    ) {
        if !self.server_capabilities.workspace_symbol_provider.0 || !self.request_state.is_idle() {
            return;
        }

        util::send_pending_did_change(self, editor, platform);

        let query = self.json.create_string(query);
        let mut params = JsonObject::default();
        params.set("query".into(), query.into(), &mut self.json);

        self.request_state = RequestState::WorkspaceSymbols { client_handle };
        self.request(platform, "workspace/symbol", params);
    }

    pub(crate) fn finish_workspace_symbols(
        &mut self,
        editor: &mut Editor,
        clients: &mut client::ClientManager,
        client_handle: client::ClientHandle,
        index: usize,
    ) {
        self.request_state = RequestState::Idle;
        if !self.server_capabilities.workspace_symbol_provider.0 {
            return;
        }

        let mut reader = io::Cursor::new(&self.request_raw_json);
        let symbols = match self.json.read(&mut reader) {
            Ok(symbols) => symbols,
            Err(_) => return,
        };
        if let Some(symbol) = symbols
            .elements(&self.json)
            .filter_map(|s| DocumentSymbolInformation::from_json(s, &self.json).ok())
            .nth(index)
        {
            let path = match Uri::parse(&self.root, symbol.uri.as_str(&self.json)) {
                Ok(Uri::Path(path)) => path,
                Err(_) => return,
            };

            match editor.buffer_view_handle_from_path(
                client_handle,
                path,
                BufferProperties::text(),
                false,
            ) {
                Ok(buffer_view_handle) => {
                    let client = clients.get_mut(client_handle);
                    client.set_buffer_view_handle(
                        Some(buffer_view_handle),
                        &editor.buffer_views,
                        &mut editor.events,
                    );

                    let buffer_view = editor.buffer_views.get_mut(buffer_view_handle);
                    let position = symbol.range.start.into_buffer_position();
                    let mut cursors = buffer_view.cursors.mut_guard();
                    cursors.clear();
                    cursors.add(Cursor {
                        anchor: position,
                        position,
                    });
                }
                Err(error) => editor
                    .status_bar
                    .write(MessageKind::Error)
                    .fmt(format_args!("{}", error)),
            }
        }
    }

    pub fn formatting(
        &mut self,
        editor: &Editor,
        platform: &mut Platform,
        buffer_handle: BufferHandle,
    ) {
        if !self.server_capabilities.document_formatting_provider.0 || !self.request_state.is_idle()
        {
            return;
        }

        util::send_pending_did_change(self, editor, platform);

        let buffer_path = &editor.buffers.get(buffer_handle).path;
        let text_document = util::text_document_with_id(&self.root, buffer_path, &mut self.json);
        let mut options = JsonObject::default();
        options.set(
            "tabSize".into(),
            JsonValue::Integer(editor.config.tab_size.get() as _),
            &mut self.json,
        );
        options.set(
            "insertSpaces".into(),
            (!editor.config.indent_with_tabs).into(),
            &mut self.json,
        );
        options.set("trimTrailingWhitespace".into(), true.into(), &mut self.json);
        options.set("trimFinalNewlines".into(), true.into(), &mut self.json);

        let mut params = JsonObject::default();
        params.set("textDocument".into(), text_document.into(), &mut self.json);
        params.set("options".into(), options.into(), &mut self.json);

        self.request_state = RequestState::Formatting { buffer_handle };
        self.request(platform, "textDocument/formatting", params);
    }

    pub fn completion(
        &mut self,
        editor: &Editor,
        platform: &mut Platform,
        client_handle: client::ClientHandle,
        buffer_handle: BufferHandle,
        buffer_position: BufferPosition,
    ) {
        if !self.server_capabilities.completion_provider.on || !self.request_state.is_idle() {
            return;
        }

        util::send_pending_did_change(self, editor, platform);

        let buffer = editor.buffers.get(buffer_handle);
        let text_document = util::text_document_with_id(&self.root, &buffer.path, &mut self.json);
        let position = DocumentPosition::from_buffer_position(buffer_position);

        let mut params = JsonObject::default();
        params.set("textDocument".into(), text_document.into(), &mut self.json);
        params.set(
            "position".into(),
            position.to_json_value(&mut self.json),
            &mut self.json,
        );

        self.request_state = RequestState::Completion {
            client_handle,
            buffer_handle,
        };

        self.request(platform, "textDocument/completion", params);
    }

    pub(crate) fn write_to_log_file<F>(&mut self, writer: F)
    where
        F: FnOnce(&mut io::BufWriter<File>, &mut Json),
    {
        if let Some(ref mut buf) = self.log_file {
            use io::Write;
            writer(buf, &mut self.json);
            let _ = buf.write_all(b"\n\n");
            let _ = buf.flush();
        }
    }

    pub(crate) fn on_request(
        &mut self,
        editor: &mut Editor,
        clients: &mut client::ClientManager,
        request: ServerRequest,
    ) -> Result<JsonValue, ProtocolError> {
        self.write_to_log_file(|buf, json| {
            use io::Write;
            let _ = write!(buf, "receive request\nid: ");
            let _ = json.write(buf, &request.id);
            let _ = write!(
                buf,
                "\nmethod: '{}'\nparams:\n",
                request.method.as_str(json)
            );
            let _ = json.write(buf, &request.params);
        });

        match request.method.as_str(&self.json) {
            "client/registerCapability" => {
                for registration in request
                    .params
                    .get("registrations", &self.json)
                    .elements(&self.json)
                {
                    #[derive(Default)]
                    struct Registration {
                        method: JsonString,
                        register_options: JsonObject,
                    }
                    impl<'json> FromJson<'json> for Registration {
                        fn from_json(
                            value: JsonValue,
                            json: &'json Json,
                        ) -> Result<Self, JsonConvertError> {
                            let mut this = Self::default();
                            for (key, value) in value.members(json) {
                                match key {
                                    "method" => this.method = JsonString::from_json(value, json)?,
                                    "registerOptions" => {
                                        this.register_options = JsonObject::from_json(value, json)?
                                    }
                                    _ => (),
                                }
                            }
                            Ok(this)
                        }
                    }

                    struct Filter {
                        pattern: Option<JsonString>,
                    }
                    impl<'json> FromJson<'json> for Filter {
                        fn from_json(
                            value: JsonValue,
                            json: &'json Json,
                        ) -> Result<Self, JsonConvertError> {
                            let pattern = value.get("pattern", json);
                            Ok(Self {
                                pattern: FromJson::from_json(pattern, json)?,
                            })
                        }
                    }

                    let registration = Registration::from_json(registration, &self.json)?;
                    match registration.method.as_str(&self.json) {
                        "textDocument/didSave" => {
                            self.document_selectors.clear();
                            for filter in registration
                                .register_options
                                .get("documentSelector", &self.json)
                                .elements(&self.json)
                            {
                                let filter = Filter::from_json(filter, &self.json)?;
                                let pattern = match filter.pattern {
                                    Some(pattern) => pattern.as_str(&self.json),
                                    None => continue,
                                };
                                let mut glob = Glob::default();
                                glob.compile(pattern)?;
                                self.document_selectors.push(glob);
                            }
                        }
                        _ => (),
                    }
                }
                Ok(JsonValue::Null)
            }
            "window/showMessage" => {
                fn parse_params(
                    params: JsonValue,
                    json: &Json,
                ) -> Result<(MessageKind, &str), JsonConvertError> {
                    let params = match params {
                        JsonValue::Object(object) => object,
                        _ => return Err(JsonConvertError),
                    };
                    let mut kind = MessageKind::Info;
                    let mut message = "";
                    for (key, value) in params.members(json) {
                        match key {
                            "type" => {
                                kind = match value {
                                    JsonValue::Integer(1) => MessageKind::Error,
                                    JsonValue::Integer(2..=4) => MessageKind::Info,
                                    _ => return Err(JsonConvertError),
                                }
                            }
                            "message" => {
                                message = match value {
                                    JsonValue::String(string) => string.as_str(json),
                                    _ => return Err(JsonConvertError),
                                }
                            }
                            _ => (),
                        }
                    }

                    Ok((kind, message))
                }

                let (kind, message) = parse_params(request.params, &self.json)?;
                editor.status_bar.write(kind).str(message);
                Ok(JsonValue::Null)
            }
            "window/showDocument" => {
                #[derive(Default)]
                struct ShowDocumentParams {
                    uri: JsonString,
                    external: Option<bool>,
                    take_focus: Option<bool>,
                    selection: Option<DocumentRange>,
                }
                impl<'json> FromJson<'json> for ShowDocumentParams {
                    fn from_json(
                        value: JsonValue,
                        json: &'json Json,
                    ) -> Result<Self, JsonConvertError> {
                        let mut this = Self::default();
                        for (key, value) in value.members(json) {
                            match key {
                                "key" => this.uri = JsonString::from_json(value, json)?,
                                "external" => this.external = FromJson::from_json(value, json)?,
                                "takeFocus" => this.take_focus = FromJson::from_json(value, json)?,
                                "selection" => this.selection = FromJson::from_json(value, json)?,
                                _ => (),
                            }
                        }
                        Ok(this)
                    }
                }

                let params = ShowDocumentParams::from_json(request.params, &self.json)?;
                let Uri::Path(path) = Uri::parse(&self.root, params.uri.as_str(&self.json))?;

                let success = if let Some(true) = params.external {
                    false
                } else if let Some(client_handle) = clients.focused_client() {
                    match editor.buffer_view_handle_from_path(
                        client_handle,
                        path,
                        BufferProperties::text(),
                        false,
                    ) {
                        Ok(buffer_view_handle) => {
                            if let Some(true) = params.take_focus {
                                let client = clients.get_mut(client_handle);
                                client.set_buffer_view_handle(
                                    Some(buffer_view_handle),
                                    &editor.buffer_views,
                                    &mut editor.events,
                                );
                            }
                            if let Some(range) = params.selection {
                                let buffer_view = editor.buffer_views.get_mut(buffer_view_handle);
                                let mut cursors = buffer_view.cursors.mut_guard();
                                cursors.clear();
                                cursors.add(Cursor {
                                    anchor: range.start.into_buffer_position(),
                                    position: range.end.into_buffer_position(),
                                });
                            }
                            true
                        }
                        Err(error) => {
                            editor
                                .status_bar
                                .write(MessageKind::Error)
                                .fmt(format_args!("{}", error));
                            false
                        }
                    }
                } else {
                    false
                };

                let mut result = JsonObject::default();
                result.set("success".into(), success.into(), &mut self.json);
                Ok(result.into())
            }
            _ => Err(ProtocolError::MethodNotFound),
        }
    }

    pub(crate) fn on_notification(
        &mut self,
        editor: &mut Editor,
        notification: ServerNotification,
    ) -> Result<(), ProtocolError> {
        self.write_to_log_file(|buf, json| {
            use io::Write;
            let _ = write!(
                buf,
                "receive notification\nmethod: '{}'\nparams:\n",
                notification.method.as_str(json)
            );
            let _ = json.write(buf, &notification.params);
        });

        match notification.method.as_str(&self.json) {
            "window/showMessage" => {
                let mut message_type: JsonInteger = 0;
                let mut message = JsonString::default();
                for (key, value) in notification.params.members(&self.json) {
                    match key {
                        "type" => message_type = JsonInteger::from_json(value, &self.json)?,
                        "value" => message = JsonString::from_json(value, &self.json)?,
                        _ => (),
                    }
                }
                let message = message.as_str(&self.json);
                match message_type {
                    1 => editor.status_bar.write(MessageKind::Error).str(message),
                    2 => editor
                        .status_bar
                        .write(MessageKind::Info)
                        .fmt(format_args!("warning: {}", message)),
                    3 => editor
                        .status_bar
                        .write(MessageKind::Info)
                        .fmt(format_args!("info: {}", message)),
                    4 => editor.status_bar.write(MessageKind::Info).str(message),
                    _ => (),
                }
                Ok(())
            }
            "textDocument/publishDiagnostics" => {
                #[derive(Default)]
                struct Params {
                    uri: JsonString,
                    diagnostics: JsonArray,
                }
                impl<'json> FromJson<'json> for Params {
                    fn from_json(
                        value: JsonValue,
                        json: &'json Json,
                    ) -> Result<Self, JsonConvertError> {
                        let mut this = Self::default();
                        for (key, value) in value.members(json) {
                            match key {
                                "uri" => this.uri = JsonString::from_json(value, json)?,
                                "diagnostics" => {
                                    this.diagnostics = JsonArray::from_json(value, json)?
                                }
                                _ => (),
                            }
                        }
                        Ok(this)
                    }
                }

                let params = Params::from_json(notification.params, &self.json)?;
                let uri = params.uri.as_str(&self.json);
                let Uri::Path(path) = Uri::parse(&self.root, uri)?;

                let diagnostics = self
                    .diagnostics
                    .diagnostics_at_path(editor, &self.root, path);
                for diagnostic in params.diagnostics.elements(&self.json) {
                    let diagnostic = DocumentDiagnostic::from_json(diagnostic, &self.json)?;
                    diagnostics.add(diagnostic, &self.json);
                }
                diagnostics.sort();
                self.diagnostics.clear_empty();
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn on_response(
        &mut self,
        editor: &mut Editor,
        platform: &mut Platform,
        clients: &mut client::ClientManager,
        response: ServerResponse,
    ) -> Result<ClientOperation, ProtocolError> {
        let method = match self.pending_requests.take(response.id) {
            Some(method) => method,
            None => return Ok(ClientOperation::None),
        };

        self.write_to_log_file(|buf, json| {
            use io::Write;
            let _ = write!(
                buf,
                "receive response\nid: {}\nmethod: '{}'\n",
                response.id.0, method
            );
            match &response.result {
                Ok(result) => {
                    let _ = buf.write_all(b"result:\n");
                    let _ = json.write(buf, result);
                }
                Err(error) => {
                    let _ = write!(
                        buf,
                        "error_code: {}\nerror_message: '{}'\nerror_data:\n",
                        error.code,
                        error.message.as_str(json)
                    );
                    let _ = json.write(buf, &error.data);
                }
            }
        });

        let result = match response.result {
            Ok(result) => result,
            Err(error) => {
                self.request_state = RequestState::Idle;
                util::write_response_error(&mut editor.status_bar, error, &self.json);
                return Ok(ClientOperation::None);
            }
        };

        match method {
            "initialize" => {
                let mut server_name = "";
                for (key, value) in result.members(&self.json) {
                    match key {
                        "capabilities" => {
                            self.server_capabilities =
                                ServerCapabilities::from_json(value, &self.json)?
                        }
                        "serverInfo" => {
                            if let JsonValue::String(name) = value.get("name", &self.json) {
                                server_name = name.as_str(&self.json);
                            }
                        }
                        _ => (),
                    }
                }

                match server_name {
                    "" => editor
                        .status_bar
                        .write(MessageKind::Info)
                        .str("lsp server started"),
                    _ => editor
                        .status_bar
                        .write(MessageKind::Info)
                        .fmt(format_args!("lsp server '{}' started", server_name)),
                }

                self.initialized = true;
                self.notify(platform, "initialized", JsonObject::default());

                for buffer in editor.buffers.iter() {
                    util::send_did_open(self, editor, platform, buffer.handle());
                }

                Ok(ClientOperation::None)
            }
            "textDocument/hover" => {
                let contents = result.get("contents", &self.json);
                let info = util::extract_markup_content(contents, &self.json);
                editor.status_bar.write(MessageKind::Info).str(info);
                Ok(ClientOperation::None)
            }
            "textDocument/signatureHelp" => {
                #[derive(Default)]
                struct SignatureHelp {
                    active_signature: usize,
                    signatures: JsonArray,
                }
                impl<'json> FromJson<'json> for SignatureHelp {
                    fn from_json(
                        value: JsonValue,
                        json: &'json Json,
                    ) -> Result<Self, JsonConvertError> {
                        let mut this = Self::default();
                        for (key, value) in value.members(json) {
                            match key {
                                "activeSignature" => {
                                    this.active_signature = usize::from_json(value, json)?;
                                }
                                "signatures" => {
                                    this.signatures = JsonArray::from_json(value, json)?;
                                }
                                _ => (),
                            }
                        }
                        Ok(this)
                    }
                }

                #[derive(Default)]
                struct SignatureInformation<'a> {
                    label: JsonString,
                    documentation: &'a str,
                }
                impl<'json> FromJson<'json> for SignatureInformation<'json> {
                    fn from_json(
                        value: JsonValue,
                        json: &'json Json,
                    ) -> Result<Self, JsonConvertError> {
                        let mut this = Self::default();
                        for (key, value) in value.members(json) {
                            match key {
                                "label" => this.label = JsonString::from_json(value, json)?,
                                "documentation" => {
                                    this.documentation = util::extract_markup_content(value, json);
                                }
                                _ => (),
                            }
                        }
                        Ok(this)
                    }
                }

                let signature_help: Option<SignatureHelp> =
                    FromJson::from_json(result, &self.json)?;
                let signature = match signature_help
                    .and_then(|sh| sh.signatures.elements(&self.json).nth(sh.active_signature))
                {
                    Some(signature) => signature,
                    None => return Ok(ClientOperation::None),
                };
                let signature = SignatureInformation::from_json(signature, &self.json)?;
                let label = signature.label.as_str(&self.json);

                if signature.documentation.is_empty() {
                    editor.status_bar.write(MessageKind::Info).str(label);
                } else {
                    editor
                        .status_bar
                        .write(MessageKind::Info)
                        .fmt(format_args!("{}\n{}", signature.documentation, label));
                }

                Ok(ClientOperation::None)
            }
            "textDocument/definition" => {
                let client_handle = match self.request_state {
                    RequestState::Definition { client_handle } => client_handle,
                    _ => return Ok(ClientOperation::None),
                };
                self.goto_definition(editor, platform, clients, client_handle, result)
            }
            "textDocument/declaration" => {
                let client_handle = match self.request_state {
                    RequestState::Declaration { client_handle } => client_handle,
                    _ => return Ok(ClientOperation::None),
                };
                self.goto_definition(editor, platform, clients, client_handle, result)
            }
            "textDocument/implementation" => {
                let client_handle = match self.request_state {
                    RequestState::Implementation { client_handle } => client_handle,
                    _ => return Ok(ClientOperation::None),
                };
                self.goto_definition(editor, platform, clients, client_handle, result)
            }
            "textDocument/references" => {
                let (client_handle, auto_close_buffer, context_len) = match self.request_state {
                    RequestState::References {
                        client_handle,
                        auto_close_buffer,
                        context_len,
                    } => (client_handle, auto_close_buffer, context_len),
                    _ => return Ok(ClientOperation::None),
                };
                self.request_state = RequestState::Idle;
                let locations = match result {
                    JsonValue::Array(locations) => locations,
                    _ => return Ok(ClientOperation::None),
                };

                let mut buffer_name = editor.string_pool.acquire();
                for location in locations.clone().elements(&self.json) {
                    let location = DocumentLocation::from_json(location, &self.json)?;
                    let Uri::Path(path) = Uri::parse(&self.root, location.uri.as_str(&self.json))?;

                    if let Some(buffer) = editor
                        .buffers
                        .find_with_path(&editor.current_directory, path)
                        .map(|h| editor.buffers.get(h))
                    {
                        let range = location.range.into_buffer_range();
                        for text in buffer.content().text_range(range) {
                            buffer_name.push_str(text);
                        }
                        break;
                    }
                }
                if buffer_name.is_empty() {
                    buffer_name.push_str("lsp");
                }
                buffer_name.push_str(".refs");

                let buffer_view_handle = editor.buffer_view_handle_from_path(
                    client_handle,
                    Path::new(&buffer_name),
                    BufferProperties::text(),
                    true,
                );
                editor.string_pool.release(buffer_name);
                let buffer_view_handle = match buffer_view_handle {
                    Ok(handle) => handle,
                    Err(error) => {
                        editor
                            .status_bar
                            .write(MessageKind::Error)
                            .fmt(format_args!("{}", error));
                        return Ok(ClientOperation::None);
                    }
                };

                let mut count = 0;
                let mut context_buffer = BufferContent::new();

                let buffer_view = editor.buffer_views.get(buffer_view_handle);
                let buffer = editor.buffers.get_mut(buffer_view.buffer_handle);

                buffer.properties = BufferProperties::log();
                buffer.properties.auto_close = auto_close_buffer;

                let range = BufferRange::between(BufferPosition::zero(), buffer.content().end());
                buffer.delete_range(&mut editor.word_database, range, &mut editor.events);

                let mut text = editor.string_pool.acquire();
                let mut last_path = "";
                for location in locations.elements(&self.json) {
                    let location = match DocumentLocation::from_json(location, &self.json) {
                        Ok(location) => location,
                        Err(_) => continue,
                    };
                    let path = match Uri::parse(&self.root, location.uri.as_str(&self.json)) {
                        Ok(Uri::Path(path)) => path,
                        Err(_) => continue,
                    };
                    let path = match path.to_str() {
                        Some(path) => path,
                        None => continue,
                    };

                    use fmt::Write;
                    let position = location.range.start.into_buffer_position();
                    let _ = writeln!(
                        text,
                        "{}:{},{}",
                        path,
                        position.line_index + 1,
                        position.column_byte_index + 1,
                    );

                    if context_len > 0 {
                        if last_path != path {
                            context_buffer.clear();
                            if let Ok(file) = File::open(path) {
                                let mut reader = io::BufReader::new(file);
                                let _ = context_buffer.read(&mut reader);
                            }
                        }

                        let surrounding_len = context_len - 1;
                        let start =
                            (location.range.start.line as usize).saturating_sub(surrounding_len);
                        let end = location.range.end.line as usize + surrounding_len;
                        let len = end - start + 1;

                        for line in context_buffer
                            .lines()
                            .skip(start)
                            .take(len)
                            .skip_while(|l| l.as_str().is_empty())
                        {
                            text.push_str(line.as_str());
                            text.push('\n');
                        }
                        text.push('\n');
                    }

                    let position = buffer.content().end();
                    buffer.insert_text(
                        &mut editor.word_database,
                        position,
                        &text,
                        &mut editor.events,
                    );
                    text.clear();

                    count += 1;
                    last_path = path;
                }

                if count == 1 {
                    text.push_str("1 reference found\n");
                } else {
                    use fmt::Write;
                    let _ = writeln!(text, "{} references found\n", count);
                }

                buffer.insert_text(
                    &mut editor.word_database,
                    BufferPosition::zero(),
                    &text,
                    &mut editor.events,
                );
                editor.string_pool.release(text);

                let client = clients.get_mut(client_handle);
                client.set_buffer_view_handle(
                    Some(buffer_view_handle),
                    &editor.buffer_views,
                    &mut editor.events,
                );
                editor.trigger_event_handlers(platform, clients);

                let mut cursors = editor
                    .buffer_views
                    .get_mut(buffer_view_handle)
                    .cursors
                    .mut_guard();
                cursors.clear();
                cursors.add(Cursor {
                    anchor: BufferPosition::zero(),
                    position: BufferPosition::zero(),
                });

                Ok(ClientOperation::None)
            }
            "textDocument/prepareRename" => {
                let (client_handle, buffer_handle, buffer_position) = match self.request_state {
                    RequestState::Rename {
                        client_handle,
                        buffer_handle,
                        buffer_position,
                    } => (client_handle, buffer_handle, buffer_position),
                    _ => return Ok(ClientOperation::None),
                };
                self.request_state = RequestState::Idle;
                let result = match result {
                    JsonValue::Null => {
                        editor
                            .status_bar
                            .write(MessageKind::Error)
                            .str("could not rename item under cursor");
                        return Ok(ClientOperation::None);
                    }
                    JsonValue::Object(result) => result,
                    _ => return Ok(ClientOperation::None),
                };
                let mut range = DocumentRange::default();
                let mut placeholder: Option<JsonString> = None;
                let mut default_behaviour: Option<bool> = None;
                for (key, value) in result.members(&self.json) {
                    match key {
                        "start" => range.start = DocumentPosition::from_json(value, &self.json)?,
                        "end" => range.end = DocumentPosition::from_json(value, &self.json)?,
                        "range" => range = DocumentRange::from_json(value, &self.json)?,
                        "placeholder" => placeholder = FromJson::from_json(value, &self.json)?,
                        "defaultBehavior" => {
                            default_behaviour = FromJson::from_json(value, &self.json)?
                        }
                        _ => (),
                    }
                }

                let buffer = editor.buffers.get(buffer_handle);

                let mut range = range.into_buffer_range();
                if let Some(true) = default_behaviour {
                    let word = buffer.content().word_at(buffer_position);
                    range = BufferRange::between(word.position, word.end_position());
                }

                let mut input = editor.string_pool.acquire();
                match placeholder {
                    Some(text) => input.push_str(text.as_str(&self.json)),
                    None => {
                        for text in buffer.content().text_range(range) {
                            input.push_str(text);
                        }
                    }
                }

                let mut ctx = ModeContext {
                    editor,
                    platform,
                    clients,
                    client_handle,
                };
                let op = read_line::enter_rename_mode(&mut ctx, &input);
                editor.string_pool.release(input);

                self.request_state = RequestState::FinishRename {
                    buffer_handle,
                    buffer_position,
                };
                Ok(op)
            }
            "textDocument/rename" => {
                let edit = WorkspaceEdit::from_json(result, &self.json)?;
                edit.apply(editor, &mut self.temp_edits, &self.root, &self.json);
                Ok(ClientOperation::None)
            }
            "textDocument/codeAction" => {
                let client_handle = match self.request_state {
                    RequestState::CodeAction { client_handle } => client_handle,
                    _ => return Ok(ClientOperation::None),
                };
                self.request_state = RequestState::Idle;
                let actions = match result {
                    JsonValue::Array(actions) => actions,
                    _ => return Ok(ClientOperation::None),
                };

                editor.picker.clear();
                for action in actions
                    .clone()
                    .elements(&self.json)
                    .filter_map(|a| DocumentCodeAction::from_json(a, &self.json).ok())
                    .filter(|a| !a.disabled)
                {
                    editor
                        .picker
                        .add_custom_entry(action.title.as_str(&self.json));
                }

                let mut ctx = ModeContext {
                    editor,
                    platform,
                    clients,
                    client_handle,
                };
                let op = picker::enter_code_action_mode(&mut ctx, self.handle());

                self.request_state = RequestState::FinishCodeAction;
                self.request_raw_json.clear();
                let _ = self.json.write(&mut self.request_raw_json, &actions.into());

                Ok(op)
            }
            "textDocument/documentSymbol" => {
                let (client_handle, buffer_view_handle) = match self.request_state {
                    RequestState::DocumentSymbols {
                        client_handle,
                        buffer_view_handle,
                    } => (client_handle, buffer_view_handle),
                    _ => return Ok(ClientOperation::None),
                };
                self.request_state = RequestState::Idle;
                let symbols = match result {
                    JsonValue::Array(symbols) => symbols,
                    _ => return Ok(ClientOperation::None),
                };

                fn add_symbols(picker: &mut Picker, depth: usize, symbols: JsonArray, json: &Json) {
                    let indent_buf = [b' '; 32];
                    let indent_len = indent_buf.len().min(depth * 2);

                    for symbol in symbols
                        .elements(json)
                        .filter_map(|s| DocumentSymbolInformation::from_json(s, json).ok())
                    {
                        let indent =
                            unsafe { std::str::from_utf8_unchecked(&indent_buf[..indent_len]) };

                        let name = symbol.name.as_str(json);
                        match symbol.container_name {
                            Some(container_name) => {
                                let container_name = container_name.as_str(json);
                                picker.add_custom_entry_fmt(format_args!(
                                    "{}{} ({})",
                                    indent, name, container_name,
                                ));
                            }
                            None => {
                                picker.add_custom_entry_fmt(format_args!("{}{}", indent, name,))
                            }
                        }

                        add_symbols(picker, depth + 1, symbol.children.clone(), json);
                    }
                }

                editor.picker.clear();
                add_symbols(&mut editor.picker, 0, symbols.clone(), &self.json);

                let mut ctx = ModeContext {
                    editor,
                    platform,
                    clients,
                    client_handle,
                };
                let op = picker::enter_document_symbol_mode(&mut ctx, self.handle());

                self.request_state = RequestState::FinishDocumentSymbols { buffer_view_handle };
                self.request_raw_json.clear();
                let _ = self.json.write(&mut self.request_raw_json, &symbols.into());

                Ok(op)
            }
            "workspace/symbol" => {
                let client_handle = match self.request_state {
                    RequestState::WorkspaceSymbols { client_handle } => client_handle,
                    _ => return Ok(ClientOperation::None),
                };
                self.request_state = RequestState::Idle;
                let symbols = match result {
                    JsonValue::Array(symbols) => symbols,
                    _ => return Ok(ClientOperation::None),
                };

                editor.picker.clear();
                for symbol in symbols
                    .clone()
                    .elements(&self.json)
                    .filter_map(|s| DocumentSymbolInformation::from_json(s, &self.json).ok())
                {
                    let name = symbol.name.as_str(&self.json);
                    match symbol.container_name {
                        Some(container_name) => {
                            let container_name = container_name.as_str(&self.json);
                            editor.picker.add_custom_entry_fmt(format_args!(
                                "{} ({})",
                                name, container_name,
                            ));
                        }
                        None => editor.picker.add_custom_entry(name),
                    }
                }

                let mut ctx = ModeContext {
                    editor,
                    platform,
                    clients,
                    client_handle,
                };
                let op = picker::enter_workspace_symbol_mode(&mut ctx, self.handle());

                self.request_state = RequestState::FinishWorkspaceSymbols;
                self.request_raw_json.clear();
                let _ = self.json.write(&mut self.request_raw_json, &symbols.into());

                Ok(op)
            }
            "textDocument/formatting" => {
                let buffer_handle = match self.request_state {
                    RequestState::Formatting { buffer_handle } => buffer_handle,
                    _ => return Ok(ClientOperation::None),
                };
                self.request_state = RequestState::Idle;
                let edits = match result {
                    JsonValue::Array(edits) => edits,
                    _ => return Ok(ClientOperation::None),
                };
                TextEdit::apply_edits(
                    editor,
                    buffer_handle,
                    &mut self.temp_edits,
                    edits,
                    &self.json,
                );

                Ok(ClientOperation::None)
            }
            "textDocument/completion" => {
                let (client_handle, buffer_handle) = match self.request_state {
                    RequestState::Completion {
                        client_handle,
                        buffer_handle,
                    } => (client_handle, buffer_handle),
                    _ => return Ok(ClientOperation::None),
                };
                self.request_state = RequestState::Idle;

                if editor.mode.kind() != ModeKind::Insert {
                    return Ok(ClientOperation::None);
                }

                let buffer_view_handle = match clients.get(client_handle).buffer_view_handle() {
                    Some(handle) => handle,
                    None => return Ok(ClientOperation::None),
                };
                let buffer_view = editor.buffer_views.get(buffer_view_handle);
                if buffer_view.buffer_handle != buffer_handle {
                    return Ok(ClientOperation::None);
                }
                let buffer = editor.buffers.get(buffer_handle).content();

                let completions = match result {
                    JsonValue::Array(completions) => completions,
                    JsonValue::Object(completions) => match completions.get("items", &self.json) {
                        JsonValue::Array(completions) => completions,
                        _ => return Ok(ClientOperation::None),
                    },
                    _ => return Ok(ClientOperation::None),
                };

                editor.picker.clear();
                for completion in completions.elements(&self.json) {
                    if let Ok(completion) =
                        DocumentCompletionItem::from_json(completion, &self.json)
                    {
                        let text = completion.text.as_str(&self.json);
                        editor.picker.add_custom_entry(text);
                    }
                }

                let position = buffer_view.cursors.main_cursor().position;
                let position = buffer.position_before(position);
                let word = buffer.word_at(position);
                let filter = match word.kind {
                    WordKind::Identifier => word.text,
                    _ => "",
                };
                editor.picker.filter(WordIndicesIter::empty(), filter);

                Ok(ClientOperation::None)
            }
            _ => Ok(ClientOperation::None),
        }
    }

    pub(crate) fn on_editor_events(&mut self, editor: &Editor, platform: &mut Platform) {
        if !self.initialized {
            return;
        }

        let mut events = EditorEventIter::new();
        while let Some(event) = events.next(&editor.events) {
            match *event {
                EditorEvent::Idle => {
                    util::send_pending_did_change(self, editor, platform);
                }
                EditorEvent::BufferRead { handle } => {
                    let handle = handle;
                    self.versioned_buffers.dispose(handle);
                    self.diagnostics.on_load_buffer(editor, handle, &self.root);
                    util::send_did_open(self, editor, platform, handle);
                }
                EditorEvent::BufferInsertText {
                    handle,
                    range,
                    text,
                    ..
                } => {
                    let text = text.as_str(&editor.events);
                    let range = BufferRange::between(range.from, range.from);
                    self.versioned_buffers.add_edit(handle, range, text);
                }
                EditorEvent::BufferDeleteText { handle, range, .. } => {
                    self.versioned_buffers.add_edit(handle, range, "");
                }
                EditorEvent::BufferWrite { handle, .. } => {
                    self.diagnostics.on_save_buffer(editor, handle, &self.root);
                    util::send_pending_did_change(self, editor, platform);
                    util::send_did_save(self, editor, platform, handle);
                }
                EditorEvent::BufferClose { handle } => {
                    self.versioned_buffers.dispose(handle);
                    self.diagnostics.on_close_buffer(handle);
                    util::send_pending_did_change(self, editor, platform);
                    util::send_did_close(self, editor, platform, handle);
                }
                EditorEvent::FixCursors { .. } => (),
                EditorEvent::BufferViewLostFocus { .. } => (),
            }
        }
    }

    fn request(&mut self, platform: &mut Platform, method: &'static str, params: JsonObject) {
        if !self.initialized {
            return;
        }

        let params = params.into();
        self.write_to_log_file(|buf, json| {
            use io::Write;
            let _ = write!(buf, "send request\nmethod: '{}'\nparams:\n", method);
            let _ = json.write(buf, &params);
        });
        let id = self
            .protocol
            .request(platform, &mut self.json, method, params);
        self.json.clear();

        self.pending_requests.add(id, method);
    }

    pub(crate) fn respond(
        &mut self,
        platform: &mut Platform,
        request_id: JsonValue,
        result: Result<JsonValue, ResponseError>,
    ) {
        self.write_to_log_file(|buf, json| {
            use io::Write;
            let _ = write!(buf, "send response\nid: ");
            let _ = json.write(buf, &request_id);
            match &result {
                Ok(result) => {
                    let _ = write!(buf, "\nresult:\n");
                    let _ = json.write(buf, result);
                }
                Err(error) => {
                    let _ = write!(
                        buf,
                        "\nerror.code: {}\nerror.message: {}\nerror.data:\n",
                        error.code,
                        error.message.as_str(json)
                    );
                    let _ = json.write(buf, &error.data);
                }
            }
        });
        self.protocol
            .respond(platform, &mut self.json, request_id, result);
        self.json.clear();
    }

    pub(crate) fn notify(
        &mut self,
        platform: &mut Platform,
        method: &'static str,
        params: JsonObject,
    ) {
        let params = params.into();
        self.write_to_log_file(|buf, json| {
            use io::Write;
            let _ = write!(buf, "send notification\nmethod: '{}'\nparams:\n", method);
            let _ = json.write(buf, &params);
        });
        self.protocol
            .notify(platform, &mut self.json, method, params);
        self.json.clear();
    }

    pub fn initialize(&mut self, platform: &mut Platform) {
        let mut params = JsonObject::default();
        params.set(
            "processId".into(),
            JsonValue::Integer(std::process::id() as _),
            &mut self.json,
        );

        let mut client_info = JsonObject::default();
        client_info.set("name".into(), env!("CARGO_PKG_NAME").into(), &mut self.json);
        client_info.set(
            "version".into(),
            env!("CARGO_PKG_VERSION").into(),
            &mut self.json,
        );
        params.set("clientInfo".into(), client_info.into(), &mut self.json);

        let root = self
            .json
            .fmt_string(format_args!("{}", Uri::Path(&self.root)));
        params.set("rootUri".into(), root.into(), &mut self.json);

        params.set(
            "capabilities".into(),
            capabilities::client_capabilities(&mut self.json),
            &mut self.json,
        );

        self.initialized = true;
        self.request(platform, "initialize", params);
        self.initialized = false;
    }

    fn goto_definition(
        &mut self,
        editor: &mut Editor,
        platform: &mut Platform,
        clients: &mut client::ClientManager,
        client_handle: client::ClientHandle,
        result: JsonValue,
    ) -> Result<ClientOperation, ProtocolError> {
        enum DefinitionLocation {
            Single(DocumentLocation),
            Many(JsonArray),
            Invalid,
        }
        impl DefinitionLocation {
            pub fn parse(value: JsonValue, json: &Json) -> Self {
                match value {
                    JsonValue::Object(_) => match DocumentLocation::from_json(value, json) {
                        Ok(location) => Self::Single(location),
                        Err(_) => Self::Invalid,
                    },
                    JsonValue::Array(array) => {
                        let mut locations = array
                            .clone()
                            .elements(json)
                            .filter_map(move |l| DocumentLocation::from_json(l, json).ok());
                        let location = match locations.next() {
                            Some(location) => location,
                            None => return Self::Invalid,
                        };
                        match locations.next() {
                            Some(_) => Self::Many(array),
                            None => Self::Single(location),
                        }
                    }
                    _ => Self::Invalid,
                }
            }
        }

        self.request_state = RequestState::Idle;
        match DefinitionLocation::parse(result, &self.json) {
            DefinitionLocation::Single(location) => {
                let Uri::Path(path) = Uri::parse(&self.root, location.uri.as_str(&self.json))?;

                match editor.buffer_view_handle_from_path(
                    client_handle,
                    path,
                    BufferProperties::text(),
                    false,
                ) {
                    Ok(buffer_view_handle) => {
                        let client = clients.get_mut(client_handle);
                        client.set_buffer_view_handle(
                            Some(buffer_view_handle),
                            &editor.buffer_views,
                            &mut editor.events,
                        );

                        let buffer_view = editor.buffer_views.get_mut(buffer_view_handle);
                        let position = location.range.start.into_buffer_position();
                        let mut cursors = buffer_view.cursors.mut_guard();
                        cursors.clear();
                        cursors.add(Cursor {
                            anchor: position,
                            position,
                        });
                    }
                    Err(error) => editor
                        .status_bar
                        .write(MessageKind::Error)
                        .fmt(format_args!("{}", error)),
                }

                Ok(ClientOperation::None)
            }
            DefinitionLocation::Many(locations) => {
                editor.picker.clear();
                for location in locations
                    .elements(&self.json)
                    .filter_map(|l| DocumentLocation::from_json(l, &self.json).ok())
                {
                    let path = match Uri::parse(&self.root, location.uri.as_str(&self.json)) {
                        Ok(Uri::Path(path)) => path,
                        Err(_) => continue,
                    };
                    let path = match path.to_str() {
                        Some(path) => path,
                        None => continue,
                    };

                    let position = location.range.start.into_buffer_position();
                    editor.picker.add_custom_entry_fmt(format_args!(
                        "{}:{},{}",
                        path,
                        position.line_index + 1,
                        position.column_byte_index + 1
                    ));
                }

                let mut ctx = ModeContext {
                    editor,
                    platform,
                    clients,
                    client_handle,
                };
                let op = picker::enter_definition_mode(&mut ctx, self.handle());
                Ok(op)
            }
            DefinitionLocation::Invalid => Ok(ClientOperation::None),
        }
    }
}

mod util {
    use super::*;

    pub fn write_response_error(status_bar: &mut StatusBar, error: ResponseError, json: &Json) {
        status_bar
            .write(MessageKind::Error)
            .str(error.message.as_str(json));
    }

    pub fn text_document_with_id(root: &Path, path: &Path, json: &mut Json) -> JsonObject {
        let uri = if path.is_absolute() {
            json.fmt_string(format_args!("{}", Uri::Path(path)))
        } else {
            match path.to_str() {
                Some(path) => json.fmt_string(format_args!("{}/{}", Uri::Path(root), path)),
                None => return JsonObject::default(),
            }
        };
        let mut id = JsonObject::default();
        id.set("uri".into(), uri.into(), json);
        id
    }

    pub fn extract_markup_content(content: JsonValue, json: &Json) -> &str {
        match content {
            JsonValue::String(s) => s.as_str(json),
            JsonValue::Object(o) => match o.get("value", json) {
                JsonValue::String(s) => s.as_str(json),
                _ => "",
            },
            _ => "",
        }
    }

    pub fn create_definition_params(
        client: &mut Client,
        editor: &Editor,
        platform: &mut Platform,
        buffer_handle: BufferHandle,
        buffer_position: BufferPosition,
    ) -> JsonObject {
        util::send_pending_did_change(client, editor, platform);

        let buffer = editor.buffers.get(buffer_handle);
        let text_document =
            util::text_document_with_id(&client.root, &buffer.path, &mut client.json);
        let position = DocumentPosition::from_buffer_position(buffer_position);

        let mut params = JsonObject::default();
        params.set(
            "textDocument".into(),
            text_document.into(),
            &mut client.json,
        );
        params.set(
            "position".into(),
            position.to_json_value(&mut client.json),
            &mut client.json,
        );

        params
    }

    pub fn send_did_open(
        client: &mut Client,
        editor: &Editor,
        platform: &mut Platform,
        buffer_handle: BufferHandle,
    ) {
        if !client.server_capabilities.text_document_sync.open_close {
            return;
        }

        let buffer = editor.buffers.get(buffer_handle);
        if !buffer.properties.can_save {
            return;
        }

        let mut text_document = text_document_with_id(&client.root, &buffer.path, &mut client.json);
        let language_id = client
            .json
            .create_string(protocol::path_to_language_id(&buffer.path));
        text_document.set("languageId".into(), language_id.into(), &mut client.json);
        text_document.set("version".into(), JsonValue::Integer(1), &mut client.json);
        let text = client.json.fmt_string(format_args!("{}", buffer.content()));
        text_document.set("text".into(), text.into(), &mut client.json);

        let mut params = JsonObject::default();
        params.set(
            "textDocument".into(),
            text_document.into(),
            &mut client.json,
        );

        client.notify(platform, "textDocument/didOpen", params);
    }

    pub fn send_pending_did_change(client: &mut Client, editor: &Editor, platform: &mut Platform) {
        let mut versioned_buffers = std::mem::take(&mut client.versioned_buffers);
        for (buffer_handle, versioned_buffer) in versioned_buffers.iter_pending_mut() {
            if versioned_buffer.pending_edits.is_empty() {
                continue;
            }
            let buffer = editor.buffers.get(buffer_handle);
            if !buffer.properties.can_save {
                versioned_buffer.flush();
                continue;
            }

            let mut text_document =
                text_document_with_id(&client.root, &buffer.path, &mut client.json);
            text_document.set(
                "version".into(),
                JsonValue::Integer(versioned_buffer.version as _),
                &mut client.json,
            );

            let mut params = JsonObject::default();
            params.set(
                "textDocument".into(),
                text_document.into(),
                &mut client.json,
            );

            let mut content_changes = JsonArray::default();
            match client.server_capabilities.text_document_sync.change {
                TextDocumentSyncKind::None => (),
                TextDocumentSyncKind::Full => {
                    let text = client.json.fmt_string(format_args!("{}", buffer.content()));
                    let mut change_event = JsonObject::default();
                    change_event.set("text".into(), text.into(), &mut client.json);
                    content_changes.push(change_event.into(), &mut client.json);
                }
                TextDocumentSyncKind::Incremental => {
                    for edit in &versioned_buffer.pending_edits {
                        let mut change_event = JsonObject::default();

                        let edit_range = DocumentRange::from_buffer_range(edit.buffer_range)
                            .to_json_value(&mut client.json);
                        change_event.set("range".into(), edit_range, &mut client.json);

                        let edit_text_range =
                            edit.text_range.start as usize..edit.text_range.end as usize;
                        let text = &versioned_buffer.texts[edit_text_range];
                        let text = client.json.create_string(text);
                        change_event.set("text".into(), text.into(), &mut client.json);

                        content_changes.push(change_event.into(), &mut client.json);
                    }
                }
            }

            params.set(
                "contentChanges".into(),
                content_changes.into(),
                &mut client.json,
            );

            versioned_buffer.flush();
            client.notify(platform, "textDocument/didChange", params);
        }
        std::mem::swap(&mut client.versioned_buffers, &mut versioned_buffers);
    }

    pub fn send_did_save(
        client: &mut Client,
        editor: &Editor,
        platform: &mut Platform,
        buffer_handle: BufferHandle,
    ) {
        if let TextDocumentSyncKind::None = client.server_capabilities.text_document_sync.save {
            return;
        }

        let buffer = editor.buffers.get(buffer_handle);
        if !buffer.properties.can_save {
            return;
        }

        let text_document = text_document_with_id(&client.root, &buffer.path, &mut client.json);
        let mut params = JsonObject::default();
        params.set(
            "textDocument".into(),
            text_document.into(),
            &mut client.json,
        );

        if let TextDocumentSyncKind::Full = client.server_capabilities.text_document_sync.save {
            let text = client.json.fmt_string(format_args!("{}", buffer.content()));
            params.set("text".into(), text.into(), &mut client.json);
        }

        client.notify(platform, "textDocument/didSave", params)
    }

    pub fn send_did_close(
        client: &mut Client,
        editor: &Editor,
        platform: &mut Platform,
        buffer_handle: BufferHandle,
    ) {
        if !client.server_capabilities.text_document_sync.open_close {
            return;
        }

        let buffer = editor.buffers.get(buffer_handle);
        if !buffer.properties.can_save {
            return;
        }

        let text_document = text_document_with_id(&client.root, &buffer.path, &mut client.json);
        let mut params = JsonObject::default();
        params.set(
            "textDocument".into(),
            text_document.into(),
            &mut client.json,
        );

        client.notify(platform, "textDocument/didClose", params);
    }
}

