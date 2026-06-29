//! Ratatui frontend for Mailsweep.
//!
//! Streams the inbox sync into a domain → sender → message tree. Panels:
//! `1` Accounts · `2` Config · `3` Domains · `4` Details. Adding an account and
//! entering provider credentials happen in an in-app modal wizard.
//!
//! Keys: `Tab`/`Shift-Tab` switch view · `o` sort · `j`/`k` move · `h`/`l`
//! collapse/expand · `Space` mark · `Enter` load attachments · `a` archive ·
//! `A` archive+delete · `d` trash · `s` spam · `u` unsubscribe · `q` quit.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tokio::time::sleep;

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use mailsweep_core::outlook::{MsAuth, OutlookClient};
use mailsweep_core::{
    accounts, archive_messages, config, group_messages, ArchiveItem, ArchiveScope, AttachmentInfo,
    AuthPrompt, Cache, DomainGroup, FetchProgress, GmailAuth, GmailClient, MailProvider,
    MessageMeta, Profile, SenderEntry, SyncResult, UnsubscribeInfo,
};

const HELP: &str = "/ search · o sort · j/k move · h/l fold · <Space> mark · ? help · q quit";

/// (keys, description) rows for the `?` help modal.
const HELP_KEYS: &[(&str, &str)] = &[
    ("1 2 3 4", "Focus Accounts / Config / Domains / Details"),
    (
        "Tab / S-Tab",
        "Switch view (All / Subscriptions / Attachments)",
    ),
    ("o", "Cycle sort (Messages / Size / Recent)"),
    ("/", "Search / filter the loaded list"),
    ("f", "Scan scope / query (Tab for examples)"),
    ("j / k", "Move down / up"),
    ("g g / G", "Jump to top / bottom"),
    ("h / l", "Collapse / expand the tree"),
    ("Enter", "Open the selected message"),
    ("Space", "Mark / unmark the selection"),
    ("c", "Clear all selections"),
    ("a / A", "Archive / archive + delete"),
    ("d / s", "Trash / spam"),
    ("r", "Mark read"),
    ("u / U", "Unsubscribe / unsubscribe + delete"),
    ("z", "Undo last delete"),
    ("O", "Overview / stats"),
    ("?", "Show this help"),
    ("q", "Quit"),
];

/// Config panel rows: set Gmail cred, set Outlook cred, add Gmail, add Outlook.
const CONFIG_ITEMS: usize = 4;
const CONFIG_LABELS: [&str; CONFIG_ITEMS] = [
    "Set Gmail credential",
    "Set Outlook credential",
    "+ Add Gmail account",
    "+ Add Outlook account",
];

/// Messages streamed from the background scan / archive tasks into the UI.
enum ScanEvent {
    Account(Profile),
    Status(String),
    Listed(usize),
    AttachmentIds(HashSet<String>),
    Progress {
        resolved: usize,
        total: usize,
        metas: Vec<MessageMeta>,
    },
    Done(String),
    Failed(String),
    Notice(String),
    /// Messages removed by a background task (e.g. archive-and-delete).
    Removed(Vec<String>),
    /// Actual attachment details for one message (background size fetch).
    AttachmentDetails(String, Vec<AttachmentInfo>),
    /// A sign-in prompt to show in the add-account modal.
    AuthPrompt(Vec<String>),
    /// Sign-in finished: the new account email, or an error.
    AuthDone(accounts::Provider, Result<String, String>),
    /// Progress of the background attachment-detail fetch.
    AttachmentProgress {
        done: usize,
        total: usize,
    },
    /// All attachment sizes are loaded — do a final re-sort.
    AttachmentsSettled,
    /// A fetched message body for the viewer: (subject, rendered lines) or error.
    MessageBody(Result<(String, Vec<String>), String>),
    /// A background operation (trash/spam/unsubscribe/archive) finished.
    OpDone,
}

/// Sends scan events tagged with the account "epoch" they belong to, so events
/// from a superseded account scan can be ignored after a switch.
#[derive(Clone)]
struct Emitter {
    tx: UnboundedSender<(u64, ScanEvent)>,
    epoch: u64,
}

impl Emitter {
    fn send(&self, event: ScanEvent) {
        let _ = self.tx.send((self.epoch, event));
    }
}

/// What a keypress asks the event loop to do (things needing account/terminal
/// access beyond mutating `App`).
enum KeyOutcome {
    None,
    Quit,
    Switch(usize),
    AddAccount(accounts::Provider),
    StartAuth(accounts::Provider),
    ConfirmRun,
    Rescan,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Panel {
    Accounts,
    Config,
    Domains,
    Details,
}

/// Add-account wizard overlay.
struct Modal {
    state: ModalState,
}

enum ModalState {
    /// Entering a provider credential (path or pasted value).
    Credential {
        provider: accounts::Provider,
        input: String,
        error: Option<String>,
        /// Continue to sign-in after saving (vs. just storing the credential).
        then_auth: bool,
    },
    /// Sign-in is running; `lines` are the current prompt/status.
    Working {
        provider: accounts::Provider,
        lines: Vec<String>,
    },
    /// Final message; dismiss with Enter/Esc.
    Message(String),
    /// Scrollable full-message viewer.
    MessageView {
        title: String,
        lines: Vec<String>,
        scroll: u16,
    },
    /// Key reference.
    Help,
    /// Yes/no confirmation for a destructive action.
    Confirm(String),
    /// Entering a server-side scan query (`help` describes the provider syntax;
    /// `examples` is shown when `expanded`).
    QueryInput {
        input: String,
        help: String,
        examples: Vec<(String, String)>,
        expanded: bool,
    },
}

impl Modal {
    fn credential(provider: accounts::Provider, then_auth: bool) -> Self {
        Modal {
            state: ModalState::Credential {
                provider,
                input: String::new(),
                error: None,
                then_auth,
            },
        }
    }

    fn working(provider: accounts::Provider) -> Self {
        Modal {
            state: ModalState::Working {
                provider,
                lines: vec!["Starting sign-in…".to_string()],
            },
        }
    }

    fn message(text: String) -> Self {
        Modal {
            state: ModalState::Message(text),
        }
    }

    fn message_loading() -> Self {
        Modal {
            state: ModalState::MessageView {
                title: "Message".to_string(),
                lines: vec!["Loading…".to_string()],
                scroll: 0,
            },
        }
    }

    fn message_view(title: String, lines: Vec<String>) -> Self {
        Modal {
            state: ModalState::MessageView {
                title,
                lines,
                scroll: 0,
            },
        }
    }

    fn help() -> Self {
        Modal {
            state: ModalState::Help,
        }
    }

    fn confirm(prompt: String) -> Self {
        Modal {
            state: ModalState::Confirm(prompt),
        }
    }

    fn query_input(prefill: String, help: String, examples: Vec<(String, String)>) -> Self {
        Modal {
            state: ModalState::QueryInput {
                input: prefill,
                help,
                examples,
                expanded: false,
            },
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    All,
    Subscriptions,
    Attachments,
}

impl View {
    const ALL: [View; 3] = [View::All, View::Subscriptions, View::Attachments];

    fn index(self) -> usize {
        View::ALL.iter().position(|v| *v == self).unwrap_or(0)
    }

    fn title(self) -> &'static str {
        match self {
            View::All => "All",
            View::Subscriptions => "Subscriptions",
            View::Attachments => "Attachments",
        }
    }

    fn next(self) -> Self {
        View::ALL[(self.index() + 1) % View::ALL.len()]
    }

    fn prev(self) -> Self {
        View::ALL[(self.index() + View::ALL.len() - 1) % View::ALL.len()]
    }
}

/// How the domain/sender/message tree is ordered.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SortMode {
    Messages,
    Size,
    Recent,
}

impl SortMode {
    fn label(self) -> &'static str {
        match self {
            SortMode::Messages => "Messages",
            SortMode::Size => "Size",
            SortMode::Recent => "Recent",
        }
    }

    fn next(self) -> Self {
        match self {
            SortMode::Messages => SortMode::Size,
            SortMode::Size => SortMode::Recent,
            SortMode::Recent => SortMode::Messages,
        }
    }
}

/// A flattened tree row at one of three depths.
enum Row<'a> {
    Domain(&'a DomainGroup),
    Sender(&'a DomainGroup, &'a SenderEntry),
    Message(&'a DomainGroup, &'a SenderEntry, &'a MessageMeta),
}

/// Owned identifier for the selected node (avoids borrow conflicts on mutation).
enum Node {
    Domain(String),
    Sender(String),
    Message,
}

/// Stable identity of a row, used to preserve the selection across regroups.
enum SelKey {
    Domain(String),
    Sender(String),
    Message(String),
}

/// What an action (trash/spam/unsubscribe) operates on.
struct Target {
    ids: Vec<String>,
    label: String,
    unsubscribe: Option<UnsubscribeInfo>,
}

#[derive(Default)]
struct SyncState {
    resolved: usize,
    total: usize,
    done: bool,
    message: String,
}

struct App {
    /// Configured account emails; `active` indexes the one being shown.
    accounts: Vec<String>,
    active: usize,
    /// Cursor within the Accounts panel (selecting a configured account).
    account_cursor: usize,
    /// Cursor within the Config panel.
    config_cursor: usize,
    /// Generation of the active account's scan; events from older epochs drop.
    epoch: u64,
    /// Pending `g` for the `gg` (jump-to-top) chord.
    pending_g: bool,
    /// Active search/filter query (empty = no filter).
    search: String,
    /// True while typing a search query.
    searching: bool,
    /// Server-side scan scope (None = inbox; Some = a provider query).
    scope_query: Option<String>,
    account: Option<Profile>,
    metas: Vec<MessageMeta>,
    attachment_ids: HashSet<String>,
    attachments: HashMap<String, Vec<AttachmentInfo>>,
    /// Message IDs marked for a bulk action.
    marked: HashSet<String>,
    view: View,
    sort: SortMode,
    groups: Vec<DomainGroup>,
    expanded_domains: HashSet<String>,
    expanded_senders: HashSet<String>,
    selected: usize,
    detail_scroll: u16,
    focus: Panel,
    sync: SyncState,
    /// Recent action/notice messages (newest last).
    history: Vec<String>,
    /// Number of in-flight background operations (drives the spinner).
    pending_ops: usize,
    /// Advances each frame to animate the spinner.
    tick: u64,
    /// Add-account wizard / confirm / message viewer, when open.
    modal: Option<Modal>,
    /// A destructive op awaiting confirmation.
    pending: Option<Pending>,
    /// The most recent removal, for undo.
    undo: Option<UndoBatch>,
    /// Throttles re-sorting while attachment sizes stream in.
    last_attach_sort: Instant,
    /// Background attachment-detail fetch progress.
    attach_active: bool,
    attach_done: usize,
    attach_total: usize,
}

impl App {
    fn new() -> Self {
        Self {
            accounts: Vec::new(),
            active: 0,
            account_cursor: 0,
            config_cursor: 0,
            epoch: 0,
            pending_g: false,
            search: String::new(),
            searching: false,
            scope_query: None,
            account: None,
            metas: Vec::new(),
            attachment_ids: HashSet::new(),
            attachments: HashMap::new(),
            marked: HashSet::new(),
            view: View::All,
            sort: SortMode::Messages,
            groups: Vec::new(),
            expanded_domains: HashSet::new(),
            expanded_senders: HashSet::new(),
            selected: 0,
            detail_scroll: 0,
            focus: Panel::Domains,
            sync: SyncState {
                message: "Starting…".to_string(),
                ..SyncState::default()
            },
            history: Vec::new(),
            pending_ops: 0,
            tick: 0,
            modal: None,
            pending: None,
            undo: None,
            last_attach_sort: Instant::now(),
            attach_active: false,
            attach_done: 0,
            attach_total: 0,
        }
    }

    /// Clear per-account state when switching accounts (keeps account list/focus).
    fn reset_for_account(&mut self) {
        self.account = None;
        self.metas.clear();
        self.attachment_ids.clear();
        self.attachments.clear();
        self.marked.clear();
        self.groups.clear();
        self.expanded_domains.clear();
        self.expanded_senders.clear();
        self.selected = 0;
        self.detail_scroll = 0;
        self.attach_active = false;
        self.attach_done = 0;
        self.attach_total = 0;
        self.sync = SyncState {
            message: "Starting…".to_string(),
            ..SyncState::default()
        };
        self.history.clear();
        self.pending_ops = 0;
        self.pending = None;
        self.undo = None;
    }

    /// Append a message to the activity history.
    fn notify(&mut self, msg: impl Into<String>) {
        self.history.push(msg.into());
        const MAX: usize = 200;
        if self.history.len() > MAX {
            self.history.drain(0..self.history.len() - MAX);
        }
    }

    /// Enter in the Accounts panel switches to the highlighted account.
    fn account_enter(&self) -> KeyOutcome {
        if self.account_cursor < self.accounts.len() && self.account_cursor != self.active {
            KeyOutcome::Switch(self.account_cursor)
        } else {
            KeyOutcome::None
        }
    }

    /// Enter in the Config panel: set a credential, or start adding an account.
    fn config_enter(&mut self) -> KeyOutcome {
        match self.config_cursor {
            0 => {
                self.modal = Some(Modal::credential(accounts::Provider::Gmail, false));
                KeyOutcome::None
            }
            1 => {
                self.modal = Some(Modal::credential(accounts::Provider::Outlook, false));
                KeyOutcome::None
            }
            2 => KeyOutcome::AddAccount(accounts::Provider::Gmail),
            3 => KeyOutcome::AddAccount(accounts::Provider::Outlook),
            _ => KeyOutcome::None,
        }
    }

    fn goto_top(&mut self) {
        match self.focus {
            Panel::Accounts => self.account_cursor = 0,
            Panel::Config => self.config_cursor = 0,
            Panel::Domains => {
                self.selected = 0;
                self.detail_scroll = 0;
            }
            Panel::Details => self.detail_scroll = 0,
        }
    }

    fn goto_bottom(&mut self) {
        match self.focus {
            Panel::Accounts => self.account_cursor = self.accounts.len().saturating_sub(1),
            Panel::Config => self.config_cursor = CONFIG_ITEMS - 1,
            Panel::Domains => {
                self.selected = self.rows().len().saturating_sub(1);
                self.detail_scroll = 0;
            }
            Panel::Details => {}
        }
    }

    // ---- scan event handling ------------------------------------------------

    fn apply(&mut self, event: ScanEvent) {
        match event {
            ScanEvent::Account(p) => self.account = Some(p),
            ScanEvent::Status(s) => self.sync.message = s,
            ScanEvent::Notice(s) => self.notify(s),
            ScanEvent::OpDone => self.pending_ops = self.pending_ops.saturating_sub(1),
            ScanEvent::Removed(ids) => self.remove_messages(&ids),
            ScanEvent::AttachmentDetails(id, list) => {
                self.attachments.insert(id, list);
                // Re-sort as sizes arrive, but throttle to avoid constant churn.
                if self.view == View::Attachments
                    && self.sort == SortMode::Size
                    && self.last_attach_sort.elapsed() > Duration::from_secs(1)
                {
                    self.rebuild_groups();
                    self.last_attach_sort = Instant::now();
                }
            }
            ScanEvent::AttachmentProgress { done, total } => {
                self.attach_done = done;
                self.attach_total = total;
                self.attach_active = total > 0 && done < total;
            }
            ScanEvent::AttachmentsSettled => {
                self.attach_active = false;
                if self.view == View::Attachments {
                    self.rebuild_groups();
                }
            }
            ScanEvent::MessageBody(res) => {
                // Only replace the modal if a viewer is still open for it.
                if matches!(
                    &self.modal,
                    Some(Modal {
                        state: ModalState::MessageView { .. }
                    })
                ) {
                    self.modal = Some(match res {
                        Ok((title, lines)) => Modal::message_view(title, lines),
                        Err(e) => Modal::message(format!("Could not load message: {e}")),
                    });
                }
            }
            ScanEvent::AuthPrompt(new_lines) => {
                if let Some(Modal {
                    state: ModalState::Working { lines, .. },
                }) = &mut self.modal
                {
                    *lines = new_lines;
                }
            }
            // Handled in the event loop (needs account context).
            ScanEvent::AuthDone(..) => {}
            ScanEvent::Listed(n) => {
                self.sync.total = n;
                self.sync.message = format!("Listed {n} messages");
            }
            ScanEvent::AttachmentIds(set) => {
                self.attachment_ids = set;
                if self.view == View::Attachments {
                    self.rebuild_groups();
                }
            }
            ScanEvent::Progress {
                resolved,
                total,
                metas,
            } => {
                self.sync.resolved = resolved;
                self.sync.total = total;
                self.sync.message = format!("Fetching metadata {resolved}/{total}");
                if !metas.is_empty() {
                    self.metas.extend(metas);
                    self.rebuild_groups();
                }
            }
            ScanEvent::Done(summary) => {
                self.sync.done = true;
                self.sync.message = summary;
            }
            ScanEvent::Failed(e) => {
                self.sync.done = true;
                self.sync.message = format!("Scan failed: {e}");
            }
        }
    }

    // ---- view / grouping ----------------------------------------------------

    fn set_view(&mut self, view: View) {
        self.view = view;
        self.rebuild_groups();
    }

    fn filtered_metas(&self) -> Vec<MessageMeta> {
        match self.view {
            View::All => self.metas.clone(),
            View::Subscriptions => self
                .metas
                .iter()
                .filter(|m| m.list_unsubscribe.is_some())
                .cloned()
                .collect(),
            View::Attachments => self
                .metas
                .iter()
                .filter(|m| self.attachment_ids.contains(&m.id))
                .cloned()
                .collect(),
        }
    }

    /// Effective size of a message for sorting/display: in the Attachments view
    /// this is the actual attachment total once known, otherwise Gmail's
    /// per-message size estimate.
    fn msg_size(&self, m: &MessageMeta) -> u64 {
        if self.view == View::Attachments {
            // Real attachment bytes once known; 0 (unknown) until the background
            // fetch loads them — never the whole-message estimate, which would
            // start high and visibly correct downward.
            return self
                .attachments
                .get(&m.id)
                .map(|atts| atts.iter().map(|a| a.size).sum())
                .unwrap_or(0);
        }
        m.size_estimate
    }

    fn sender_size(&self, s: &SenderEntry) -> u64 {
        s.messages.iter().map(|m| self.msg_size(m)).sum()
    }

    fn domain_size(&self, g: &DomainGroup) -> u64 {
        g.senders.iter().map(|s| self.sender_size(s)).sum()
    }

    fn rebuild_groups(&mut self) {
        // Preserve the selected node across regroups so it doesn't jump while
        // the tree re-sorts during an incremental sync.
        let anchor = self.selection_key();
        let mut groups = group_messages(&self.filtered_metas());
        let size_of = |m: &MessageMeta| self.msg_size(m);
        apply_sort(&mut groups, self.sort, &size_of);
        if !self.search.is_empty() {
            groups = filter_by_search(groups, &self.search);
        }
        self.groups = groups;
        match anchor.and_then(|key| self.find_row(&key)) {
            Some(pos) => self.selected = pos,
            None => self.clamp_selection(),
        }
    }

    fn selection_key(&self) -> Option<SelKey> {
        match self.rows().get(self.selected)? {
            Row::Domain(g) => Some(SelKey::Domain(g.domain.clone())),
            Row::Sender(_, s) => Some(SelKey::Sender(s.email.clone())),
            Row::Message(_, _, m) => Some(SelKey::Message(m.id.clone())),
        }
    }

    fn find_row(&self, key: &SelKey) -> Option<usize> {
        self.rows().iter().position(|r| match (r, key) {
            (Row::Domain(g), SelKey::Domain(d)) => g.domain == *d,
            (Row::Sender(_, s), SelKey::Sender(e)) => s.email == *e,
            (Row::Message(_, _, m), SelKey::Message(id)) => m.id == *id,
            _ => false,
        })
    }

    fn rows(&self) -> Vec<Row<'_>> {
        let mut rows = Vec::new();
        for g in &self.groups {
            rows.push(Row::Domain(g));
            if self.expanded_domains.contains(&g.domain) {
                for s in &g.senders {
                    rows.push(Row::Sender(g, s));
                    if self.expanded_senders.contains(&s.email) {
                        for m in &s.messages {
                            rows.push(Row::Message(g, s, m));
                        }
                    }
                }
            }
        }
        rows
    }

    fn clamp_selection(&mut self) {
        let n = self.rows().len();
        self.selected = if n == 0 { 0 } else { self.selected.min(n - 1) };
    }

    // ---- navigation ---------------------------------------------------------

    fn move_down(&mut self) {
        match self.focus {
            Panel::Details => self.detail_scroll = self.detail_scroll.saturating_add(1),
            Panel::Accounts => {
                if self.account_cursor + 1 < self.accounts.len() {
                    self.account_cursor += 1;
                }
            }
            Panel::Config => {
                if self.config_cursor + 1 < CONFIG_ITEMS {
                    self.config_cursor += 1;
                }
            }
            Panel::Domains => {
                let n = self.rows().len();
                if n > 0 {
                    self.selected = (self.selected + 1).min(n - 1);
                    self.detail_scroll = 0;
                }
            }
        }
    }

    fn move_up(&mut self) {
        match self.focus {
            Panel::Details => self.detail_scroll = self.detail_scroll.saturating_sub(1),
            Panel::Accounts => self.account_cursor = self.account_cursor.saturating_sub(1),
            Panel::Config => self.config_cursor = self.config_cursor.saturating_sub(1),
            Panel::Domains => {
                self.selected = self.selected.saturating_sub(1);
                self.detail_scroll = 0;
            }
        }
    }

    fn node(&self) -> Option<Node> {
        match self.rows().get(self.selected)? {
            Row::Domain(g) => Some(Node::Domain(g.domain.clone())),
            Row::Sender(_, s) => Some(Node::Sender(s.email.clone())),
            Row::Message(..) => Some(Node::Message),
        }
    }

    fn current_domain(&self) -> Option<String> {
        match self.rows().get(self.selected)? {
            Row::Domain(g) | Row::Sender(g, _) | Row::Message(g, _, _) => Some(g.domain.clone()),
        }
    }

    fn current_sender(&self) -> Option<String> {
        match self.rows().get(self.selected)? {
            Row::Sender(_, s) | Row::Message(_, s, _) => Some(s.email.clone()),
            Row::Domain(_) => None,
        }
    }

    fn expand(&mut self) {
        match self.node() {
            Some(Node::Domain(d)) => {
                self.expanded_domains.insert(d);
            }
            Some(Node::Sender(e)) => {
                self.expanded_senders.insert(e);
            }
            _ => {}
        }
    }

    fn collapse(&mut self) {
        match self.node() {
            Some(Node::Domain(d)) => {
                self.expanded_domains.remove(&d);
            }
            Some(Node::Sender(e)) => {
                if self.expanded_senders.remove(&e) {
                    // collapsed the sender in place
                } else if let Some(domain) = self.current_domain() {
                    self.expanded_domains.remove(&domain);
                    self.select_domain(&domain);
                }
            }
            Some(Node::Message) => {
                if let Some(email) = self.current_sender() {
                    self.expanded_senders.remove(&email);
                    self.select_sender(&email);
                }
            }
            None => {}
        }
    }

    fn select_domain(&mut self, domain: &str) {
        let pos = self
            .rows()
            .iter()
            .position(|r| matches!(r, Row::Domain(g) if g.domain == domain));
        if let Some(p) = pos {
            self.selected = p;
        }
    }

    fn select_sender(&mut self, email: &str) {
        let pos = self
            .rows()
            .iter()
            .position(|r| matches!(r, Row::Sender(_, s) if s.email == email));
        if let Some(p) = pos {
            self.selected = p;
        }
    }

    fn selected_message_id(&self) -> Option<String> {
        match self.rows().get(self.selected)? {
            Row::Message(_, _, m) => Some(m.id.clone()),
            _ => None,
        }
    }

    fn target(&self) -> Option<Target> {
        match self.rows().get(self.selected)? {
            Row::Domain(g) => Some(Target {
                ids: g.message_ids(),
                label: g.domain.clone(),
                unsubscribe: g.unsubscribe.clone(),
            }),
            Row::Sender(_, s) => Some(Target {
                ids: s.message_ids(),
                label: s.email.clone(),
                unsubscribe: s.unsubscribe.clone(),
            }),
            Row::Message(_, s, m) => Some(Target {
                ids: vec![m.id.clone()],
                label: m.subject.clone(),
                // A single message inherits its sender's unsubscribe handle.
                unsubscribe: s.unsubscribe.clone(),
            }),
        }
    }

    /// All message IDs under the selected node.
    fn selected_ids(&self) -> Vec<String> {
        match self.rows().get(self.selected) {
            Some(Row::Domain(g)) => g.message_ids(),
            Some(Row::Sender(_, s)) => s.message_ids(),
            Some(Row::Message(_, _, m)) => vec![m.id.clone()],
            None => Vec::new(),
        }
    }

    /// Toggle the mark on the selected node (marks/unmarks all its messages).
    fn toggle_mark(&mut self) {
        let ids = self.selected_ids();
        if ids.is_empty() {
            return;
        }
        if ids.iter().all(|id| self.marked.contains(id)) {
            for id in &ids {
                self.marked.remove(id);
            }
        } else {
            self.marked.extend(ids);
        }
    }

    /// Mark glyph for a set of IDs: all / some / none marked.
    fn mark_glyph(&self, ids: &[String]) -> char {
        let marked = ids.iter().filter(|id| self.marked.contains(*id)).count();
        if marked == 0 {
            ' '
        } else if marked == ids.len() {
            '●'
        } else {
            '◐'
        }
    }

    /// IDs a bulk action targets: the marked set if any, else the selection.
    fn action_ids(&self) -> (Vec<String>, String) {
        if self.marked.is_empty() {
            match self.target() {
                Some(t) => (t.ids, t.label),
                None => (Vec::new(), String::new()),
            }
        } else {
            (
                self.marked.iter().cloned().collect(),
                format!("{} marked", self.marked.len()),
            )
        }
    }

    /// Collect archivable messages (those with attachments) for the marked set,
    /// or — if nothing is marked — under the current selection.
    fn archive_items(&self) -> Vec<ArchiveItem> {
        let item = |g: &DomainGroup, s: &SenderEntry, m: &MessageMeta| ArchiveItem {
            message_id: m.id.clone(),
            domain: g.domain.clone(),
            sender: s.email.clone(),
            subject: m.subject.clone(),
            date_ms: m.internal_date,
        };

        if !self.marked.is_empty() {
            return self
                .metas
                .iter()
                .filter(|m| self.marked.contains(&m.id))
                .map(|m| ArchiveItem {
                    message_id: m.id.clone(),
                    domain: m.domain().to_string(),
                    sender: m.from_email.clone(),
                    subject: m.subject.clone(),
                    date_ms: m.internal_date,
                })
                .collect();
        }

        match self.rows().get(self.selected) {
            Some(Row::Domain(g)) => g
                .senders
                .iter()
                .flat_map(|s| s.messages.iter().map(move |m| item(g, s, m)))
                .collect(),
            Some(Row::Sender(g, s)) => s.messages.iter().map(|m| item(g, s, m)).collect(),
            Some(Row::Message(g, s, m)) => vec![item(g, s, m)],
            _ => Vec::new(),
        }
    }

    /// Drop messages from the model (after trash/spam) and regroup.
    fn remove_messages(&mut self, ids: &[String]) {
        let set: HashSet<&str> = ids.iter().map(String::as_str).collect();
        // Capture the removed messages for a one-level undo.
        let metas: Vec<MessageMeta> = self
            .metas
            .iter()
            .filter(|m| set.contains(m.id.as_str()))
            .cloned()
            .collect();
        if !metas.is_empty() {
            let ids = metas.iter().map(|m| m.id.clone()).collect();
            self.undo = Some(UndoBatch { ids, metas });
        }
        self.metas.retain(|m| !set.contains(m.id.as_str()));
        for id in ids {
            self.attachment_ids.remove(id);
            self.attachments.remove(id);
            self.marked.remove(id);
        }
        self.rebuild_groups();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    config::migrate_to_data_dir();
    // Refuse to start if another instance is live (shared caches/DBs).
    let _lock = mailsweep_core::lock::InstanceLock::acquire()?;
    let _ = accounts::migrate_legacy_if_needed().await;
    let mut list = accounts::list_accounts();
    if list.is_empty() {
        println!("No accounts configured. Authorizing your first (Gmail) account…");
        let on_prompt: accounts::PromptFn = Arc::new(|p| match p {
            AuthPrompt::Browser { url } => println!("Open this URL to authorize:\n  {url}"),
            AuthPrompt::DeviceCode {
                verification_uri,
                user_code,
                ..
            } => println!("Visit {verification_uri} and enter code {user_code}"),
        });
        let email = accounts::add_account(accounts::Provider::Gmail, on_prompt).await?;
        println!("Authorized {email}.");
        list = vec![accounts::Account {
            email,
            provider: accounts::Provider::Gmail,
        }];
    }
    run_app(list).await
}

/// A configured account and its live provider + cache.
struct AccountCtx {
    email: String,
    provider: Arc<dyn MailProvider>,
    cache: Cache,
}

fn build_account(account: &accounts::Account) -> Result<AccountCtx> {
    let email = account.email.clone();
    let cache = Cache::open(accounts::cache_path(&email))?;
    let provider: Arc<dyn MailProvider> = match account.provider {
        accounts::Provider::Gmail => {
            let auth = GmailAuth::new(
                config::secret_path(),
                accounts::token_path(&email),
                config::SCOPES,
            );
            Arc::new(GmailClient::new(Arc::new(auth)).with_cache(cache.clone()))
        }
        accounts::Provider::Outlook => {
            let client_id = config::ms_client_id()
                .context("set MAILSWEEP_MS_CLIENT_ID to your Azure app id for Outlook")?;
            let auth = MsAuth::new(client_id, accounts::token_path(&email));
            Arc::new(OutlookClient::new(Arc::new(auth)).with_cache(cache.clone()))
        }
    };
    Ok(AccountCtx {
        email,
        provider,
        cache,
    })
}

fn spawn_scan(
    epoch: u64,
    ctx: &AccountCtx,
    query: Option<String>,
    tx: &UnboundedSender<(u64, ScanEvent)>,
) -> JoinHandle<()> {
    let em = Emitter {
        tx: tx.clone(),
        epoch,
    };
    let provider = ctx.provider.clone();
    let cache = ctx.cache.clone();
    tokio::spawn(async move { run_scan(em, provider, cache, query).await })
}

async fn run_app(list: Vec<accounts::Account>) -> Result<()> {
    let mut accounts_ctx: Vec<AccountCtx> = Vec::new();
    for acct in &list {
        match build_account(acct) {
            Ok(ctx) => accounts_ctx.push(ctx),
            Err(e) => eprintln!("Skipping account {}: {e}", acct.email),
        }
    }
    if accounts_ctx.is_empty() {
        anyhow::bail!("no usable accounts");
    }

    let (tx, rx) = mpsc::unbounded_channel::<(u64, ScanEvent)>();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let mut app = App::new();
    app.accounts = accounts_ctx.iter().map(|c| c.email.clone()).collect();
    app.active = 0;
    app.account_cursor = 0;

    let mut epoch = 1u64;
    app.epoch = epoch;
    let mut handle = spawn_scan(epoch, &accounts_ctx[0], None, &tx);

    let result = event_loop(
        &mut terminal,
        &mut accounts_ctx,
        &mut app,
        &mut epoch,
        &mut handle,
        &tx,
        rx,
    )
    .await;

    handle.abort();
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

/// Abort the current scan and start a fresh one for the active account and the
/// current scope query.
fn restart_scan(
    accounts_ctx: &[AccountCtx],
    app: &mut App,
    epoch: &mut u64,
    handle: &mut JoinHandle<()>,
    tx: &UnboundedSender<(u64, ScanEvent)>,
) {
    handle.abort();
    *epoch += 1;
    app.reset_for_account();
    app.epoch = *epoch;
    *handle = spawn_scan(
        *epoch,
        &accounts_ctx[app.active],
        app.scope_query.clone(),
        tx,
    );
}

fn switch_account(
    i: usize,
    accounts_ctx: &[AccountCtx],
    app: &mut App,
    epoch: &mut u64,
    handle: &mut JoinHandle<()>,
    tx: &UnboundedSender<(u64, ScanEvent)>,
) {
    if i >= accounts_ctx.len() {
        return;
    }
    app.active = i;
    app.account_cursor = i;
    restart_scan(accounts_ctx, app, epoch, handle, tx);
}

async fn run_scan(
    em: Emitter,
    provider: Arc<dyn MailProvider>,
    cache: Cache,
    query: Option<String>,
) {
    em.send(ScanEvent::Status("Authenticating…".to_string()));
    match provider.profile().await {
        Ok(p) => em.send(ScanEvent::Account(p)),
        Err(e) => {
            em.send(ScanEvent::Failed(e.to_string()));
            return;
        }
    }

    let limit = config::scan_limit();

    // Attachment IDs first so the Attachments tab filters correctly as metadata
    // streams in (scoped to the query when one is set).
    let attachment_ids = match &query {
        None => provider.list_attachment_ids(limit).await,
        Some(q) => {
            provider
                .list_query_ids(&format!("{q} has:attachment"), limit)
                .await
        }
    }
    .unwrap_or_default();
    em.send(ScanEvent::AttachmentIds(
        attachment_ids.iter().cloned().collect(),
    ));

    match &query {
        // Custom scope: a full (non-incremental) scan of the query results.
        Some(q) => {
            em.send(ScanEvent::Status(format!("Scanning: {q}…")));
            match provider.list_query_ids(q, limit).await {
                Ok(ids) => full_sync(&em, provider.as_ref(), &ids).await,
                Err(e) => {
                    em.send(ScanEvent::Failed(e.to_string()));
                    return;
                }
            }
        }
        // Inbox: incremental sync from the stored checkpoint.
        None => {
            let token = cache.get_state("history_id").await.ok().flatten();
            em.send(ScanEvent::Status("Syncing inbox…".to_string()));
            let result = match provider.inbox_sync(token.as_deref(), limit).await {
                Ok(r) => r,
                Err(e) => {
                    em.send(ScanEvent::Failed(e.to_string()));
                    return;
                }
            };
            if result.full {
                full_sync(&em, provider.as_ref(), &result.added).await;
            } else {
                incremental_sync(&em, provider.as_ref(), &cache, &result).await;
            }
            let _ = cache.set_state("history_id", &result.next_token).await;
        }
    }

    // Attachment details: serve cached ones instantly, fetch only the rest
    // (and persist them, so reruns don't re-fetch).
    let any = !attachment_ids.is_empty();
    let cached = cache
        .get_attachments_many(&attachment_ids)
        .await
        .unwrap_or_default();
    for (id, list) in &cached {
        em.send(ScanEvent::AttachmentDetails(id.clone(), list.clone()));
    }
    let missing: Vec<String> = attachment_ids
        .iter()
        .filter(|id| !cached.contains_key(*id))
        .cloned()
        .collect();

    let total = missing.len();
    if total > 0 {
        em.send(ScanEvent::AttachmentProgress { done: 0, total });
    }
    for (i, id) in missing.into_iter().enumerate() {
        if let Ok(list) = provider.message_attachments(&id).await {
            let _ = cache.put_attachments(&id, &list).await;
            em.send(ScanEvent::AttachmentDetails(id, list));
        }
        em.send(ScanEvent::AttachmentProgress { done: i + 1, total });
        sleep(Duration::from_millis(120)).await;
    }
    if any {
        em.send(ScanEvent::Notice(format!(
            "Attachment sizes ready ({} messages)",
            attachment_ids.len()
        )));
        em.send(ScanEvent::AttachmentsSettled);
    }
}

/// Full snapshot: fetch metadata for every inbox message (cache-aware).
async fn full_sync(em: &Emitter, provider: &dyn MailProvider, ids: &[String]) {
    em.send(ScanEvent::Listed(ids.len()));
    let em2 = em.clone();
    let mut on_update = move |p: FetchProgress, batch: &[MessageMeta]| {
        em2.send(ScanEvent::Progress {
            resolved: p.resolved,
            total: p.total,
            metas: batch.to_vec(),
        });
    };
    match provider.fetch_metadata(ids, &mut on_update).await {
        Ok(report) => {
            let resolved = report.from_cache + report.fetched;
            let mut summary = format!("Synced · {resolved}/{} resolved", report.requested);
            if !report.batch_errors.is_empty() {
                summary.push_str(&format!(" · ⚠ {}", report.batch_errors[0]));
            }
            em.send(ScanEvent::Done(summary));
        }
        Err(e) => em.send(ScanEvent::Failed(e.to_string())),
    }
}

/// Incremental: rebuild from the cache plus the sync deltas.
async fn incremental_sync(
    em: &Emitter,
    provider: &dyn MailProvider,
    cache: &Cache,
    result: &SyncResult,
) {
    em.send(ScanEvent::Status("Incremental sync…".to_string()));

    let mut base = cache.all().await.unwrap_or_default();
    let removed_n = result.removed.len();
    if removed_n > 0 {
        let _ = cache.remove(&result.removed).await;
        let rem: HashSet<&str> = result.removed.iter().map(String::as_str).collect();
        base.retain(|m| !rem.contains(m.id.as_str()));
    }

    let have: HashSet<&str> = base.iter().map(|m| m.id.as_str()).collect();
    let to_fetch: Vec<String> = result
        .added
        .iter()
        .filter(|id| !have.contains(id.as_str()))
        .cloned()
        .collect();
    drop(have);

    let base_n = base.len();
    let fetch_n = to_fetch.len();
    em.send(ScanEvent::Progress {
        resolved: base_n,
        total: base_n + fetch_n,
        metas: base,
    });

    if fetch_n > 0 {
        let em2 = em.clone();
        let mut on_update = move |p: FetchProgress, batch: &[MessageMeta]| {
            em2.send(ScanEvent::Progress {
                resolved: base_n + p.resolved,
                total: base_n + fetch_n,
                metas: batch.to_vec(),
            });
        };
        if let Err(e) = provider.fetch_metadata(&to_fetch, &mut on_update).await {
            em.send(ScanEvent::Status(format!("Fetch error: {e}")));
        }
    }

    em.send(ScanEvent::Done(format!(
        "Incremental · {base_n} cached · {fetch_n} new · {removed_n} removed"
    )));
}

#[allow(clippy::too_many_arguments)]
async fn event_loop<B: Backend + io::Write>(
    terminal: &mut Terminal<B>,
    accounts_ctx: &mut Vec<AccountCtx>,
    app: &mut App,
    epoch: &mut u64,
    handle: &mut JoinHandle<()>,
    tx: &UnboundedSender<(u64, ScanEvent)>,
    mut rx: UnboundedReceiver<(u64, ScanEvent)>,
) -> Result<()> {
    loop {
        app.tick = app.tick.wrapping_add(1);
        while let Ok((ep, event)) = rx.try_recv() {
            match event {
                // Auth events are not account-scoped; always handle them.
                ScanEvent::AuthDone(provider, res) => {
                    handle_auth_done(provider, res, accounts_ctx, app, epoch, handle, tx)
                }
                ScanEvent::AuthPrompt(_) => app.apply(event),
                other => {
                    if ep == app.epoch {
                        app.apply(other);
                    }
                }
            }
        }
        terminal.draw(|f| ui(f, app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let provider = accounts_ctx[app.active].provider.clone();
                let em = Emitter {
                    tx: tx.clone(),
                    epoch: *epoch,
                };
                match handle_key(app, &provider, &em, key.code).await {
                    KeyOutcome::Quit => break,
                    KeyOutcome::None => {}
                    KeyOutcome::Switch(i) => {
                        switch_account(i, accounts_ctx, app, epoch, handle, tx)
                    }
                    KeyOutcome::AddAccount(provider) => {
                        if provider_configured(provider) {
                            app.modal = Some(Modal::working(provider));
                            start_auth(provider, tx);
                        } else {
                            app.modal = Some(Modal::credential(provider, true));
                        }
                    }
                    KeyOutcome::StartAuth(provider) => {
                        app.modal = Some(Modal::working(provider));
                        start_auth(provider, tx);
                    }
                    KeyOutcome::ConfirmRun => {
                        app.modal = None;
                        if let Some(pending) = app.pending.take() {
                            let provider = accounts_ctx[app.active].provider.clone();
                            let em = Emitter {
                                tx: tx.clone(),
                                epoch: *epoch,
                            };
                            run_pending(app, &provider, &em, pending);
                        }
                    }
                    KeyOutcome::Rescan => restart_scan(accounts_ctx, app, epoch, handle, tx),
                }
            }
        }
    }
    Ok(())
}

fn provider_configured(provider: accounts::Provider) -> bool {
    match provider {
        accounts::Provider::Gmail => config::gmail_configured(),
        accounts::Provider::Outlook => config::outlook_configured(),
    }
}

/// Spawn the background sign-in task; prompts/result flow back over the channel.
fn start_auth(provider: accounts::Provider, tx: &UnboundedSender<(u64, ScanEvent)>) {
    let prompt_tx = tx.clone();
    let on_prompt: accounts::PromptFn = Arc::new(move |p: AuthPrompt| {
        let lines = match p {
            AuthPrompt::Browser { url } => vec![
                "Opening your browser to sign in.".to_string(),
                "If it didn't open, visit:".to_string(),
                url,
            ],
            AuthPrompt::DeviceCode {
                verification_uri,
                user_code,
                ..
            } => vec![
                "In any browser, open:".to_string(),
                verification_uri,
                String::new(),
                format!("and enter code:  {user_code}"),
            ],
        };
        let _ = prompt_tx.send((0, ScanEvent::AuthPrompt(lines)));
    });
    let done_tx = tx.clone();
    tokio::spawn(async move {
        let res = accounts::add_account(provider, on_prompt)
            .await
            .map_err(|e| e.to_string());
        let _ = done_tx.send((0, ScanEvent::AuthDone(provider, res)));
    });
}

fn handle_auth_done(
    provider: accounts::Provider,
    res: Result<String, String>,
    accounts_ctx: &mut Vec<AccountCtx>,
    app: &mut App,
    epoch: &mut u64,
    handle: &mut JoinHandle<()>,
    tx: &UnboundedSender<(u64, ScanEvent)>,
) {
    match res {
        Ok(email) => {
            if let Some(i) = accounts_ctx.iter().position(|c| c.email == email) {
                switch_account(i, accounts_ctx, app, epoch, handle, tx);
                app.modal = Some(Modal::message(format!("{email} is already added")));
            } else {
                let account = accounts::Account {
                    email: email.clone(),
                    provider,
                };
                match build_account(&account) {
                    Ok(ctx) => {
                        accounts_ctx.push(ctx);
                        app.accounts.push(email.clone());
                        switch_account(
                            accounts_ctx.len() - 1,
                            accounts_ctx,
                            app,
                            epoch,
                            handle,
                            tx,
                        );
                        app.modal = Some(Modal::message(format!("Added {email}")));
                    }
                    Err(e) => app.modal = Some(Modal::message(format!("Failed: {e}"))),
                }
            }
        }
        Err(e) => app.modal = Some(Modal::message(format!("Sign-in failed: {e}"))),
    }
}

fn save_cred(provider: accounts::Provider, input: &str) -> Result<()> {
    match provider {
        accounts::Provider::Gmail => config::save_gmail_secret(input),
        accounts::Provider::Outlook => config::save_ms_client_id(input),
    }
}

/// Handle a keypress while the add-account modal is open.
fn modal_key(app: &mut App, code: KeyCode) -> KeyOutcome {
    enum Act {
        None,
        Close,
        Message(String),
        StartAuth(accounts::Provider),
        ConfirmRun,
        SetScope(Option<String>),
    }

    let act = {
        let Some(modal) = app.modal.as_mut() else {
            return KeyOutcome::None;
        };
        match &mut modal.state {
            ModalState::Credential {
                provider,
                input,
                error,
                then_auth,
            } => match code {
                KeyCode::Esc => Act::Close,
                KeyCode::Enter => match save_cred(*provider, input) {
                    Ok(()) if *then_auth => Act::StartAuth(*provider),
                    Ok(()) => Act::Message(format!("Saved {} credential", provider.label())),
                    Err(e) => {
                        *error = Some(e.to_string());
                        Act::None
                    }
                },
                KeyCode::Backspace => {
                    input.pop();
                    Act::None
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    Act::None
                }
                _ => Act::None,
            },
            ModalState::Working { .. } => match code {
                KeyCode::Esc => Act::Close,
                _ => Act::None,
            },
            ModalState::Message(_) => match code {
                KeyCode::Enter | KeyCode::Esc => Act::Close,
                _ => Act::None,
            },
            ModalState::MessageView { scroll, .. } => match code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => Act::Close,
                KeyCode::Char('j') | KeyCode::Down => {
                    *scroll = scroll.saturating_add(1);
                    Act::None
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    *scroll = scroll.saturating_sub(1);
                    Act::None
                }
                _ => Act::None,
            },
            ModalState::Help => match code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('?') => {
                    Act::Close
                }
                _ => Act::None,
            },
            ModalState::Confirm(_) => match code {
                KeyCode::Char('y') | KeyCode::Enter => Act::ConfirmRun,
                KeyCode::Char('n') | KeyCode::Esc => Act::Close,
                _ => Act::None,
            },
            ModalState::QueryInput {
                input, expanded, ..
            } => match code {
                KeyCode::Esc => Act::Close,
                KeyCode::Tab => {
                    *expanded = !*expanded;
                    Act::None
                }
                KeyCode::Enter => {
                    let q = input.trim();
                    let scope = if q.is_empty() || q == "in:inbox" {
                        None
                    } else {
                        Some(q.to_string())
                    };
                    Act::SetScope(scope)
                }
                KeyCode::Backspace => {
                    input.pop();
                    Act::None
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    Act::None
                }
                _ => Act::None,
            },
        }
    };

    match act {
        Act::None => KeyOutcome::None,
        Act::Close => {
            app.modal = None;
            app.pending = None;
            KeyOutcome::None
        }
        Act::Message(m) => {
            app.modal = Some(Modal::message(m));
            KeyOutcome::None
        }
        Act::StartAuth(p) => KeyOutcome::StartAuth(p),
        Act::ConfirmRun => KeyOutcome::ConfirmRun,
        Act::SetScope(scope) => {
            app.scope_query = scope;
            app.modal = None;
            KeyOutcome::Rescan
        }
    }
}

async fn handle_key(
    app: &mut App,
    provider: &Arc<dyn MailProvider>,
    em: &Emitter,
    code: KeyCode,
) -> KeyOutcome {
    if app.modal.is_some() {
        return modal_key(app, code);
    }
    if app.searching {
        match code {
            KeyCode::Enter => app.searching = false,
            KeyCode::Esc => {
                app.searching = false;
                app.search.clear();
                app.rebuild_groups();
            }
            KeyCode::Backspace => {
                app.search.pop();
                app.rebuild_groups();
            }
            KeyCode::Char(c) => {
                app.search.push(c);
                app.rebuild_groups();
            }
            _ => {}
        }
        return KeyOutcome::None;
    }

    // `gg` chord: remember a pending `g`, cleared by any other key.
    let was_pending_g = app.pending_g;
    app.pending_g = false;

    match code {
        KeyCode::Char('q') | KeyCode::Esc => return KeyOutcome::Quit,
        KeyCode::Char('1') => app.focus = Panel::Accounts,
        KeyCode::Char('2') => app.focus = Panel::Config,
        KeyCode::Char('3') => app.focus = Panel::Domains,
        KeyCode::Char('4') => app.focus = Panel::Details,
        KeyCode::Char('/') => app.searching = true,
        KeyCode::Char('f') => {
            let examples = provider
                .query_examples()
                .iter()
                .map(|(d, q)| (d.to_string(), q.to_string()))
                .collect();
            app.modal = Some(Modal::query_input(
                app.scope_query.clone().unwrap_or_default(),
                provider.query_help().to_string(),
                examples,
            ));
        }
        KeyCode::Tab => app.set_view(app.view.next()),
        KeyCode::BackTab => app.set_view(app.view.prev()),
        KeyCode::Char('l') | KeyCode::Right => app.expand(),
        KeyCode::Char('h') | KeyCode::Left => app.collapse(),
        KeyCode::Char('j') | KeyCode::Down => app.move_down(),
        KeyCode::Char('k') | KeyCode::Up => app.move_up(),
        KeyCode::Char('g') => {
            if was_pending_g {
                app.goto_top();
            } else {
                app.pending_g = true;
            }
        }
        KeyCode::Char('G') => app.goto_bottom(),
        KeyCode::Char(' ') => app.toggle_mark(),
        KeyCode::Char('o') => {
            app.sort = app.sort.next();
            app.rebuild_groups();
            app.notify(format!("Sort: {}", app.sort.label()));
        }
        KeyCode::Char('c') => {
            app.marked.clear();
            app.notify("Cleared all selections");
        }
        KeyCode::Char('?') => app.modal = Some(Modal::help()),
        KeyCode::Char('O') => {
            app.modal = Some(Modal::message_view("Overview".to_string(), overview_lines(app)));
        }
        KeyCode::Enter => match app.focus {
            Panel::Accounts => return app.account_enter(),
            Panel::Config => return app.config_enter(),
            _ => {
                if let Some(id) = app.selected_message_id() {
                    open_message(app, provider, em, id);
                }
            }
        },
        KeyCode::Char('a') => archive(app, provider, em, false),
        KeyCode::Char('A') => archive(app, provider, em, true),
        KeyCode::Char('d') => act(app, provider, em, Action::Trash),
        KeyCode::Char('s') => act(app, provider, em, Action::Spam),
        KeyCode::Char('r') => act(app, provider, em, Action::Read),
        KeyCode::Char('z') => undo_last(app, provider, em),
        KeyCode::Char('u') => unsubscribe(app, provider, em, false),
        KeyCode::Char('U') => unsubscribe(app, provider, em, true),
        _ => {}
    }
    KeyOutcome::None
}

/// Open the message viewer: show a loading modal and fetch the body in the
/// background; the result arrives as a `MessageBody` event.
fn open_message(app: &mut App, provider: &Arc<dyn MailProvider>, em: &Emitter, id: String) {
    app.modal = Some(Modal::message_loading());
    let provider = provider.clone();
    let em = em.clone();
    tokio::spawn(async move {
        let res = match provider.fetch_message_body(&id).await {
            Ok(b) => {
                let mut lines = vec![
                    format!("From: {}", b.from),
                    format!("To:   {}", b.to),
                    format!("Date: {}", fmt_date(b.date_ms)),
                    String::new(),
                ];
                lines.extend(b.text.lines().map(str::to_string));
                Ok((b.subject, lines))
            }
            Err(e) => Err(e.to_string()),
        };
        em.send(ScanEvent::MessageBody(res));
    });
}

fn archive(app: &mut App, provider: &Arc<dyn MailProvider>, em: &Emitter, delete_after: bool) {
    let items = app.archive_items();
    if items.is_empty() {
        app.notify("Nothing to archive in selection");
        return;
    }
    if delete_after && items.len() > CONFIRM_THRESHOLD {
        let prompt = format!("Archive and delete {} message(s)?", items.len());
        app.pending = Some(Pending::Archive(items, true));
        app.modal = Some(Modal::confirm(prompt));
    } else {
        run_archive(app, provider, em, items, delete_after);
    }
}

fn run_archive(
    app: &mut App,
    provider: &Arc<dyn MailProvider>,
    em: &Emitter,
    items: Vec<ArchiveItem>,
    delete_after: bool,
) {
    let account = app
        .account
        .as_ref()
        .map(|p| p.email.replace('/', "_"))
        .unwrap_or_else(|| "mailbox".to_string());
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = config::archive_dir().join(format!("{account}-{ts}.zip"));

    let verb = if delete_after {
        "Archiving + deleting"
    } else {
        "Archiving"
    };
    app.notify(format!("{verb} {} message(s)…", items.len()));
    app.pending_ops += 1;
    let provider = provider.clone();
    let em = em.clone();
    tokio::spawn(async move {
        let ids: Vec<String> = items.iter().map(|i| i.message_id.clone()).collect();
        let msg = match archive_messages(
            provider.as_ref(),
            &items,
            &path,
            ArchiveScope::MessagesAndAttachments,
        )
        .await
        {
            Ok(s) if delete_after => match provider.trash(&ids).await {
                Ok(()) => {
                    em.send(ScanEvent::Removed(ids.clone()));
                    format!(
                        "Archived {} file(s) ({}) and trashed {} message(s) → {}",
                        s.files,
                        human_bytes(s.bytes),
                        ids.len(),
                        s.path.display()
                    )
                }
                Err(e) => format!("Archived {} file(s) but trash failed: {e}", s.files),
            },
            Ok(s) => format!(
                "Archived {} file(s) ({}) from {} message(s) → {}",
                s.files,
                human_bytes(s.bytes),
                s.messages,
                s.path.display()
            ),
            Err(e) => format!("Archive failed: {e}"),
        };
        em.send(ScanEvent::Notice(msg));
        em.send(ScanEvent::OpDone);
    });
}

/// Bulk destructive actions above this many messages ask for confirmation.
const CONFIRM_THRESHOLD: usize = 100;

#[derive(Clone, Copy)]
enum Action {
    Trash,
    Spam,
    Read,
}

impl Action {
    fn present(self) -> &'static str {
        match self {
            Action::Trash => "Trashing",
            Action::Spam => "Marking as spam",
            Action::Read => "Marking read",
        }
    }
    fn past(self) -> &'static str {
        match self {
            Action::Trash => "Trashed",
            Action::Spam => "Marked as spam",
            Action::Read => "Marked read",
        }
    }
    fn confirm_verb(self) -> &'static str {
        match self {
            Action::Trash => "Trash",
            Action::Spam => "Mark as spam",
            Action::Read => "Mark read",
        }
    }
    /// Whether the action removes messages from the inbox (and is undoable).
    fn removes(self) -> bool {
        matches!(self, Action::Trash | Action::Spam)
    }
}

/// A destructive operation, possibly gated behind a confirmation.
enum Pending {
    Act(Action, Vec<String>, String),
    Archive(Vec<ArchiveItem>, bool),
    UnsubDelete(UnsubscribeInfo, Vec<String>, String),
}

/// The most recent removal, for one level of undo.
struct UndoBatch {
    ids: Vec<String>,
    metas: Vec<MessageMeta>,
}

fn run_pending(app: &mut App, provider: &Arc<dyn MailProvider>, em: &Emitter, pending: Pending) {
    match pending {
        Pending::Act(a, ids, label) => run_act(app, provider, em, a, ids, label),
        Pending::Archive(items, del) => run_archive(app, provider, em, items, del),
        Pending::UnsubDelete(info, ids, label) => {
            run_unsubscribe(app, provider, em, info, ids, label, true)
        }
    }
}

/// Trash/spam/mark-read in the background so the UI never blocks. Trash/spam
/// over the threshold confirm first; the model updates on the `Removed` event.
fn act(app: &mut App, provider: &Arc<dyn MailProvider>, em: &Emitter, action: Action) {
    let (ids, label) = app.action_ids();
    if ids.is_empty() {
        return;
    }
    if action.removes() && ids.len() > CONFIRM_THRESHOLD {
        let prompt = format!(
            "{} {} message(s) from {label}?",
            action.confirm_verb(),
            ids.len()
        );
        app.pending = Some(Pending::Act(action, ids, label));
        app.modal = Some(Modal::confirm(prompt));
    } else {
        run_act(app, provider, em, action, ids, label);
    }
}

fn run_act(
    app: &mut App,
    provider: &Arc<dyn MailProvider>,
    em: &Emitter,
    action: Action,
    ids: Vec<String>,
    label: String,
) {
    let n = ids.len();
    app.notify(format!("{} {n} message(s) from {label}…", action.present()));
    app.pending_ops += 1;
    let provider = provider.clone();
    let em = em.clone();
    tokio::spawn(async move {
        let result = match action {
            Action::Trash => provider.trash(&ids).await,
            Action::Spam => provider.mark_spam(&ids).await,
            Action::Read => provider.mark_read(&ids).await,
        };
        match result {
            Ok(()) => {
                if action.removes() {
                    em.send(ScanEvent::Removed(ids.clone()));
                }
                em.send(ScanEvent::Notice(format!(
                    "{} {n} message(s) from {label}",
                    action.past()
                )));
            }
            Err(e) => em.send(ScanEvent::Notice(format!("Action failed: {e}"))),
        }
        em.send(ScanEvent::OpDone);
    });
}

/// Undo the most recent removal: re-add the messages locally and restore them
/// to the inbox server-side.
fn undo_last(app: &mut App, provider: &Arc<dyn MailProvider>, em: &Emitter) {
    let Some(batch) = app.undo.take() else {
        app.notify("Nothing to undo");
        return;
    };
    let n = batch.ids.len();
    app.metas.extend(batch.metas);
    app.rebuild_groups();
    app.notify(format!("Restoring {n} message(s)…"));
    app.pending_ops += 1;
    let provider = provider.clone();
    let em = em.clone();
    let ids = batch.ids;
    tokio::spawn(async move {
        let msg = match provider.restore(&ids).await {
            Ok(()) => format!("Restored {n} message(s)"),
            Err(e) => format!("Restore failed: {e}"),
        };
        em.send(ScanEvent::Notice(msg));
        em.send(ScanEvent::OpDone);
    });
}

/// Unsubscribe, optionally trashing the target's messages afterward.
///
/// One-click POST and the (optional) trash run in the background; opening a
/// web/mail link is instant and done inline.
fn unsubscribe(app: &mut App, provider: &Arc<dyn MailProvider>, em: &Emitter, delete_after: bool) {
    let Some(target) = app.target() else {
        app.notify("Nothing selected");
        return;
    };
    let Some(info) = target.unsubscribe.clone() else {
        app.notify(format!("No unsubscribe info for {}", target.label));
        return;
    };
    let label = target.label.clone();
    let ids = target.ids.clone();

    if delete_after && ids.len() > CONFIRM_THRESHOLD {
        let prompt = format!(
            "Unsubscribe and delete {} message(s) from {label}?",
            ids.len()
        );
        app.pending = Some(Pending::UnsubDelete(info, ids, label));
        app.modal = Some(Modal::confirm(prompt));
        return;
    }
    run_unsubscribe(app, provider, em, info, ids, label, delete_after);
}

fn run_unsubscribe(
    app: &mut App,
    provider: &Arc<dyn MailProvider>,
    em: &Emitter,
    info: UnsubscribeInfo,
    ids: Vec<String>,
    label: String,
    delete_after: bool,
) {
    // Manual (non-one-click) methods open immediately.
    if !info.one_click {
        if info.http_url.is_some() {
            let _ = mailsweep_core::unsubscribe::open_in_browser(&info);
            app.notify(format!("Opened unsubscribe page for {label}"));
        } else if let Some(mailto) = &info.mailto {
            app.notify(format!("Unsubscribe by emailing: {mailto}"));
        } else {
            app.notify(format!("No usable unsubscribe method for {label}"));
        }
    }

    // Background: the one-click POST and/or the trash.
    if info.one_click || delete_after {
        if info.one_click {
            app.notify(format!("Unsubscribing from {label}…"));
        }
        if delete_after {
            app.notify(format!("Deleting {} message(s) from {label}…", ids.len()));
        }
        app.pending_ops += 1;
        let provider = provider.clone();
        let em = em.clone();
        tokio::spawn(async move {
            if info.one_click {
                let msg = match provider.unsubscribe_one_click(&info).await {
                    Ok(true) => format!("Unsubscribed from {label} (one-click)"),
                    Ok(false) => format!("One-click unsubscribe not accepted for {label}"),
                    Err(e) => format!("Unsubscribe failed: {e}"),
                };
                em.send(ScanEvent::Notice(msg));
            }
            if delete_after {
                match provider.trash(&ids).await {
                    Ok(()) => {
                        em.send(ScanEvent::Removed(ids.clone()));
                        em.send(ScanEvent::Notice(format!(
                            "Trashed {} message(s) from {label}",
                            ids.len()
                        )));
                    }
                    Err(e) => em.send(ScanEvent::Notice(format!("Trash failed: {e}"))),
                }
            }
            em.send(ScanEvent::OpDone);
        });
    }
}

// ---- rendering --------------------------------------------------------------

fn ui(f: &mut Frame, app: &App) {
    // Top row sized for accounts + sync line, or the 4 Config rows — whichever
    // is taller (both inside a 2-line border).
    let top_height = ((app.accounts.len() as u16 + 3).max(CONFIG_ITEMS as u16 + 2)).min(14);
    let rows = Layout::vertical([
        Constraint::Length(top_height),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(f.area());

    let top =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).split(rows[0]);
    render_accounts(f, app, top[0]);
    render_config(f, app, top[1]);

    let body =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).split(rows[1]);
    render_domains(f, app, body[0]);
    // Right column: Details on top, a small Activity panel at the bottom.
    let right = Layout::vertical([Constraint::Min(3), Constraint::Length(8)]).split(body[1]);
    render_details(f, app, right[0]);
    render_history(f, app, right[1]);

    let status = if app.searching {
        let mut spans = vec![Span::styled("/", Style::default().fg(Color::Yellow))];
        if app.search.is_empty() {
            // Placeholder: explain the filter and show a real example from the
            // loaded data so it's clear what you can type.
            let example = app
                .groups
                .first()
                .map(|g| g.domain.clone())
                .unwrap_or_else(|| "newsletter".to_string());
            spans.push(Span::styled(
                format!("filter loaded domains/senders/subjects — e.g. {example}"),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ));
        } else {
            spans.push(Span::raw(app.search.clone()));
            spans.push(Span::styled(
                "  (Enter keep · Esc clear)",
                Style::default().fg(Color::DarkGray),
            ));
        }
        Line::from(spans)
    } else {
        Line::from(Span::styled(HELP, Style::default().fg(Color::DarkGray)))
    };
    f.render_widget(Paragraph::new(status), rows[2]);

    if let Some(modal) = &app.modal {
        render_modal(f, modal);
    }
}

fn render_history(f: &mut Frame, app: &App, area: Rect) {
    let busy = app.pending_ops > 0;
    let title = if busy {
        format!("Activity ({} running)", app.pending_ops)
    } else {
        "Activity".to_string()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let height = inner.height as usize;
    let reserve = usize::from(busy);
    let avail = height.saturating_sub(reserve);
    let start = app.history.len().saturating_sub(avail);
    let mut lines: Vec<Line> = app.history[start..]
        .iter()
        .map(|m| Line::from(m.clone()))
        .collect();

    if busy {
        const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let frame = FRAMES[(app.tick as usize) % FRAMES.len()];
        lines.push(Line::from(Span::styled(
            format!("{frame} working…"),
            Style::default().fg(Color::Yellow),
        )));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_config(f: &mut Frame, app: &App, area: Rect) {
    let block = panel_block(app, Panel::Config, "[2] Config");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let status = |ok: bool| if ok { " [✓]" } else { " [✗ not set]" };
    let items: Vec<ListItem> = CONFIG_LABELS
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let suffix = match i {
                0 => status(config::gmail_configured()).to_string(),
                1 => status(config::outlook_configured()).to_string(),
                _ => String::new(),
            };
            let style = if i >= 2 {
                Style::default().fg(Color::Green)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(format!("{label}{suffix}"), style)))
        })
        .collect();

    let mut state = ListState::default();
    if app.focus == Panel::Config {
        state.select(Some(app.config_cursor.min(CONFIG_ITEMS - 1)));
    }
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, inner, &mut state);
}

/// A centered rectangle `px`%×`py`% of `r`.
fn centered_rect(px: u16, py: u16, r: Rect) -> Rect {
    let vert = Layout::vertical([
        Constraint::Percentage((100 - py) / 2),
        Constraint::Percentage(py),
        Constraint::Percentage((100 - py) / 2),
    ])
    .split(r);
    Layout::horizontal([
        Constraint::Percentage((100 - px) / 2),
        Constraint::Percentage(px),
        Constraint::Percentage((100 - px) / 2),
    ])
    .split(vert[1])[1]
}

fn render_modal(f: &mut Frame, modal: &Modal) {
    // The message viewer is large and scrollable.
    if let ModalState::MessageView {
        title,
        lines,
        scroll,
    } = &modal.state
    {
        let area = centered_rect(82, 84, f.area());
        f.render_widget(Clear, area);
        let body: Vec<Line> = lines.iter().map(|l| Line::from(l.clone())).collect();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(format!("{title}  ·  j/k scroll · Esc close"));
        f.render_widget(
            Paragraph::new(body)
                .block(block)
                .wrap(Wrap { trim: false })
                .scroll((*scroll, 0)),
            area,
        );
        return;
    }

    // The key reference is a fixed two-column table.
    if let ModalState::Help = &modal.state {
        let area = centered_rect(56, 90, f.area());
        f.render_widget(Clear, area);
        let mut lines: Vec<Line> = HELP_KEYS
            .iter()
            .map(|(k, d)| {
                Line::from(vec![
                    Span::styled(
                        format!(" {k:<11} "),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(*d),
                ])
            })
            .collect();
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Esc to close",
            Style::default().fg(Color::DarkGray),
        )));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title("Keys");
        f.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }

    // Scan-query input — grows when examples are expanded.
    if let ModalState::QueryInput {
        input,
        help,
        examples,
        expanded,
    } = &modal.state
    {
        let area = centered_rect(66, if *expanded { 82 } else { 46 }, f.area());
        f.render_widget(Clear, area);
        let mut lines = vec![
            Line::from("Scan query (empty = inbox):"),
            Line::from(""),
            Line::from(Span::styled(
                format!("> {input}"),
                Style::default().fg(Color::Cyan),
            )),
            Line::from(""),
        ];
        if *expanded {
            lines.push(Line::from(Span::styled(
                "Examples:",
                Style::default().add_modifier(Modifier::UNDERLINED),
            )));
            for (desc, q) in examples {
                lines.push(Line::from(vec![
                    Span::raw(format!("  {desc:<26}")),
                    Span::styled(q.clone(), Style::default().fg(Color::DarkGray)),
                ]));
            }
        } else {
            lines.push(Line::from(Span::styled(
                help.clone(),
                Style::default().fg(Color::DarkGray),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            if *expanded {
                "Enter to scan · Tab to collapse · Esc to cancel"
            } else {
                "Enter to scan · Tab for examples · Esc to cancel"
            },
            Style::default().fg(Color::DarkGray),
        )));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title("Scan scope");
        f.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }

    let area = centered_rect(64, 50, f.area());
    f.render_widget(Clear, area);

    let (title, lines): (&str, Vec<Line>) = match &modal.state {
        ModalState::MessageView { .. } | ModalState::Help | ModalState::QueryInput { .. } => {
            unreachable!("handled above")
        }
        ModalState::Credential {
            provider,
            input,
            error,
            ..
        } => {
            let hint = match provider {
                accounts::Provider::Gmail => {
                    "Paste your client_secret.json contents, or a path to the file:"
                }
                accounts::Provider::Outlook => {
                    "Paste your Azure app (client) id, or a path to a file with it:"
                }
            };
            let mut lines = vec![
                Line::from(format!("Set {} credential", provider.label())),
                Line::from(""),
                Line::from(hint),
                Line::from(""),
                Line::from(Span::styled(
                    format!("> {input}"),
                    Style::default().fg(Color::Cyan),
                )),
            ];
            if let Some(e) = error {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    e.clone(),
                    Style::default().fg(Color::Red),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Enter to save · Esc to cancel",
                Style::default().fg(Color::DarkGray),
            )));
            ("Add account", lines)
        }
        ModalState::Working { provider, lines } => {
            let mut out = vec![
                Line::from(format!("Signing in to {}…", provider.label())),
                Line::from(""),
            ];
            for l in lines {
                out.push(Line::from(l.clone()));
            }
            out.push(Line::from(""));
            out.push(Line::from(Span::styled(
                "Esc to cancel",
                Style::default().fg(Color::DarkGray),
            )));
            ("Sign in", out)
        }
        ModalState::Message(m) => (
            "Account",
            vec![
                Line::from(m.clone()),
                Line::from(""),
                Line::from(Span::styled(
                    "Enter to close",
                    Style::default().fg(Color::DarkGray),
                )),
            ],
        ),
        ModalState::Confirm(prompt) => (
            "Confirm",
            vec![
                Line::from(prompt.clone()),
                Line::from(""),
                Line::from(Span::styled(
                    "(y) yes    (n) no",
                    Style::default().fg(Color::DarkGray),
                )),
            ],
        ),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(title);
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn panel_block(app: &App, panel: Panel, title: impl Into<String>) -> Block<'static> {
    let border = if app.focus == panel {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(title.into())
}

fn render_accounts(f: &mut Frame, app: &App, area: Rect) {
    let block = panel_block(app, Panel::Accounts, "[1] Accounts");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);

    // One row per configured account.
    let items: Vec<ListItem> = app
        .accounts
        .iter()
        .enumerate()
        .map(|(i, email)| {
            let active = i == app.active;
            let marker = if active { "●" } else { " " };
            // Live count (decrements as you delete).
            let totals = if active {
                let what = if app.scope_query.is_some() {
                    "matched"
                } else {
                    "inbox"
                };
                format!("  ({} {what})", app.metas.len())
            } else {
                String::new()
            };
            let style = if active {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(
                format!("{marker} {email}{totals}"),
                style,
            )))
        })
        .collect();

    let mut state = ListState::default();
    if app.focus == Panel::Accounts && !items.is_empty() {
        state.select(Some(app.account_cursor.min(items.len() - 1)));
    }
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, rows[0], &mut state);

    f.render_widget(
        Paragraph::new(Span::styled(
            format!("Sync: {}", app.sync.message),
            Style::default().fg(Color::Cyan),
        )),
        rows[1],
    );
}

fn tabs_line(active: View) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, v) in View::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let style = if *v == active {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(format!("[{}]", v.title()), style));
    }
    Line::from(spans)
}

fn render_domains(f: &mut Frame, app: &App, area: Rect) {
    let marked = if app.marked.is_empty() {
        String::new()
    } else {
        format!(" · {} marked", app.marked.len())
    };
    let search = if app.search.is_empty() {
        String::new()
    } else {
        format!(" · /{}", app.search)
    };
    let scope = match &app.scope_query {
        Some(q) => format!(" · scope: {q}"),
        None => String::new(),
    };
    let title = format!(
        "[3] Domains ({}{}) · sort {}{}{}",
        app.groups.len(),
        marked,
        app.sort.label(),
        search,
        scope
    );
    let block = panel_block(app, Panel::Domains, title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // [tabs][column header][optional gauge][list]
    let show_gauge = !app.sync.done || app.attach_active;
    let mut constraints = vec![Constraint::Length(1), Constraint::Length(1)];
    if show_gauge {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(1));
    let chunks = Layout::vertical(constraints).split(inner);

    f.render_widget(Paragraph::new(tabs_line(app.view)), chunks[0]);

    let header = Line::from(Span::styled(
        format!(
            "{:4}{:>7} {:>8} {:>10}  {}",
            "", "Senders", "Messages", "Size", "Name"
        ),
        Style::default().add_modifier(Modifier::UNDERLINED),
    ));
    f.render_widget(Paragraph::new(header), chunks[1]);

    if show_gauge {
        let (done, total, label) = if !app.sync.done {
            (app.sync.resolved, app.sync.total, "sync")
        } else {
            (app.attach_done, app.attach_total, "attachments")
        };
        let ratio = if total > 0 {
            done as f64 / total as f64
        } else {
            0.0
        };
        let gauge = Gauge::default()
            .gauge_style(Style::default().fg(Color::Cyan))
            .ratio(ratio.clamp(0.0, 1.0))
            .label(format!("{label} {done}/{total}"));
        f.render_widget(gauge, chunks[2]);
    }

    let list_area = *chunks.last().expect("list chunk present");
    let items: Vec<ListItem> = app
        .rows()
        .iter()
        .map(|r| ListItem::new(row_line(app, r)))
        .collect();

    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(app.selected.min(items.len() - 1)));
    }
    let list = List::new(items)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("");
    f.render_stateful_widget(list, list_area, &mut state);
}

fn row_line(app: &App, row: &Row) -> Line<'static> {
    match row {
        Row::Domain(g) => {
            let marker = if app.expanded_domains.contains(&g.domain) {
                '▾'
            } else {
                '▸'
            };
            let mark = app.mark_glyph(&g.message_ids());
            let unsub = if g.unsubscribe.is_some() { " ✉" } else { "" };
            Line::from(format!(
                "{mark}{marker}  {:>7} {:>8} {:>10}  {}{unsub}",
                g.sender_count(),
                g.count(),
                human_bytes(app.domain_size(g)),
                g.domain
            ))
        }
        Row::Sender(_, s) => {
            let marker = if app.expanded_senders.contains(&s.email) {
                '▾'
            } else {
                '▸'
            };
            let mark = app.mark_glyph(&s.message_ids());
            let unsub = if s.unsubscribe.is_some() { " ✉" } else { "" };
            let who = s.name.clone().unwrap_or_else(|| s.email.clone());
            Line::from(format!(
                "{mark} {marker} {:>7} {:>8} {:>10}   {who} <{}>{unsub}",
                "",
                s.count(),
                human_bytes(app.sender_size(s)),
                s.email
            ))
        }
        Row::Message(_, _, m) => {
            let mark = app.mark_glyph(std::slice::from_ref(&m.id));
            Line::from(format!(
                "{mark}   {:>7} {:>8} {:>10}   {}  {}",
                "",
                "",
                human_bytes(app.msg_size(m)),
                fmt_date_short(m.internal_date),
                m.subject
            ))
        }
    }
}

fn render_details(f: &mut Frame, app: &App, area: Rect) {
    let widget = Paragraph::new(detail_lines(app))
        .block(panel_block(app, Panel::Details, "[4] Details"))
        .wrap(Wrap { trim: true })
        .scroll((app.detail_scroll, 0));
    f.render_widget(widget, area);
}

fn unsub_line(info: &UnsubscribeInfo) -> Line<'static> {
    let kind = if info.one_click {
        "one-click (press u)"
    } else if info.http_url.is_some() {
        "web link (press u to open)"
    } else {
        "email"
    };
    Line::from(format!("Unsubscribe: {kind}"))
}

fn detail_lines(app: &App) -> Vec<Line<'static>> {
    let rows = app.rows();
    let Some(row) = rows.get(app.selected) else {
        return vec![Line::from("No selection.")];
    };

    let bold = |s: String| {
        Line::from(Span::styled(
            s,
            Style::default().add_modifier(Modifier::BOLD),
        ))
    };
    let underline = |s: &str| {
        Line::from(Span::styled(
            s.to_string(),
            Style::default().add_modifier(Modifier::UNDERLINED),
        ))
    };

    match row {
        Row::Domain(g) => {
            let mut lines = vec![
                bold(g.domain.clone()),
                Line::from(format!(
                    "{} messages · {} senders",
                    g.count(),
                    g.sender_count()
                )),
                Line::from(""),
            ];
            if let Some(u) = &g.unsubscribe {
                lines.push(unsub_line(u));
                lines.push(Line::from(""));
            }
            lines.push(underline("Top senders (press l to expand the tree):"));
            for s in g.senders.iter().take(12) {
                lines.push(Line::from(format!("· {:>4}  {}", s.count(), s.email)));
            }
            lines
        }
        Row::Sender(_, s) => {
            let mut lines = vec![
                bold(s.name.clone().unwrap_or_else(|| s.email.clone())),
                Line::from(s.email.clone()),
                Line::from(format!("{} messages", s.count())),
                Line::from(""),
            ];
            if let Some(u) = &s.unsubscribe {
                lines.push(unsub_line(u));
                lines.push(Line::from(""));
            }
            lines.push(underline("Recent messages (press l to expand the tree):"));
            for m in s.messages.iter().take(12) {
                lines.push(Line::from(format!(
                    "· {}  {}",
                    fmt_date_short(m.internal_date),
                    m.subject
                )));
            }
            lines
        }
        Row::Message(_, s, m) => {
            let mut lines = vec![
                bold(m.subject.clone()),
                Line::from(format!("From: {}", s.email)),
                Line::from(format!("Date: {}", fmt_date(m.internal_date))),
                Line::from(format!("Size: {}", human_bytes(m.size_estimate))),
                Line::from(""),
            ];
            match app.attachments.get(&m.id) {
                Some(atts) if !atts.is_empty() => {
                    lines.push(underline("Attachments (press a to archive):"));
                    for a in atts {
                        lines.push(Line::from(format!(
                            "· {} ({}, {})",
                            a.filename,
                            a.mime_type,
                            human_bytes(a.size)
                        )));
                    }
                }
                Some(_) => lines.push(Line::from("No attachments on this message.")),
                None if app.attachment_ids.contains(&m.id) => {
                    lines.push(Line::from("Attachments loading…"))
                }
                None => {}
            }
            lines
        }
    }
}

/// Fuzzy-filter the tree by `query`, matching domains, senders, and subjects.
///
/// A domain-name match keeps the whole domain; a sender match keeps all of that
/// sender's messages; otherwise only messages whose subject matches are kept.
fn filter_by_search(groups: Vec<DomainGroup>, query: &str) -> Vec<DomainGroup> {
    let matcher = SkimMatcherV2::default();
    let hit = |hay: &str| matcher.fuzzy_match(hay, query).is_some();
    groups
        .into_iter()
        .filter_map(|mut g| {
            if hit(&g.domain) {
                return Some(g);
            }
            g.senders.retain_mut(|s| {
                if hit(&s.email) || s.name.as_deref().map_or(false, |n| hit(n)) {
                    return true;
                }
                s.messages.retain(|m| hit(&m.subject));
                !s.messages.is_empty()
            });
            if g.senders.is_empty() {
                None
            } else {
                Some(g)
            }
        })
        .collect()
}

/// Sort a tree of domains/senders/messages in place by the chosen mode.
fn apply_sort(groups: &mut [DomainGroup], sort: SortMode, size_of: &impl Fn(&MessageMeta) -> u64) {
    let sender_size = |s: &SenderEntry| -> u64 { s.messages.iter().map(|m| size_of(m)).sum() };
    let sender_recent = |s: &SenderEntry| {
        s.messages
            .iter()
            .map(|m| m.internal_date)
            .max()
            .unwrap_or(0)
    };
    let domain_size = |g: &DomainGroup| -> u64 { g.senders.iter().map(|s| sender_size(s)).sum() };
    let domain_recent = |g: &DomainGroup| {
        g.senders
            .iter()
            .flat_map(|s| &s.messages)
            .map(|m| m.internal_date)
            .max()
            .unwrap_or(0)
    };

    for g in groups.iter_mut() {
        for s in g.senders.iter_mut() {
            match sort {
                SortMode::Size => s.messages.sort_by(|a, b| size_of(b).cmp(&size_of(a))),
                _ => s
                    .messages
                    .sort_by(|a, b| b.internal_date.cmp(&a.internal_date)),
            }
        }
        match sort {
            SortMode::Messages => g.senders.sort_by(|a, b| b.count().cmp(&a.count())),
            SortMode::Size => g
                .senders
                .sort_by(|a, b| sender_size(b).cmp(&sender_size(a))),
            SortMode::Recent => g
                .senders
                .sort_by(|a, b| sender_recent(b).cmp(&sender_recent(a))),
        }
    }
    match sort {
        SortMode::Messages => groups.sort_by(|a, b| b.count().cmp(&a.count())),
        SortMode::Size => groups.sort_by(|a, b| domain_size(b).cmp(&domain_size(a))),
        SortMode::Recent => groups.sort_by(|a, b| domain_recent(b).cmp(&domain_recent(a))),
    }
}

/// Aggregate stats for the Overview modal, computed from the synced set.
fn overview_lines(app: &App) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(p) = &app.account {
        lines.push(format!("Account: {}", p.email));
    }
    if let Some(q) = &app.scope_query {
        lines.push(format!("Scope:   {q}"));
    }
    lines.push(String::new());

    let total_size: u64 = app.metas.iter().map(|m| m.size_estimate).sum();
    let domains: HashSet<&str> = app.metas.iter().map(|m| m.domain()).collect();
    let mut senders: HashMap<&str, (usize, u64)> = HashMap::new();
    for m in &app.metas {
        let e = senders.entry(m.from_email.as_str()).or_insert((0, 0));
        e.0 += 1;
        e.1 += m.size_estimate;
    }
    let sub_msgs = app
        .metas
        .iter()
        .filter(|m| m.list_unsubscribe.is_some())
        .count();
    let sub_senders: HashSet<&str> = app
        .metas
        .iter()
        .filter(|m| m.list_unsubscribe.is_some())
        .map(|m| m.from_email.as_str())
        .collect();
    let att_size: u64 = app.attachments.values().flatten().map(|a| a.size).sum();

    lines.push(format!("Messages        {}", app.metas.len()));
    lines.push(format!("Total size      {}", human_bytes(total_size)));
    lines.push(format!("Domains         {}", domains.len()));
    lines.push(format!("Senders         {}", senders.len()));
    lines.push(format!(
        "Subscriptions   {sub_msgs} messages from {} senders",
        sub_senders.len()
    ));
    lines.push(format!(
        "Attachments     {} messages · {} downloaded",
        app.attachment_ids.len(),
        human_bytes(att_size)
    ));
    lines.push(String::new());

    let mut ranked: Vec<(&str, usize, u64)> =
        senders.iter().map(|(e, (c, s))| (*e, *c, *s)).collect();

    ranked.sort_by_key(|(_, c, _)| std::cmp::Reverse(*c));
    lines.push("Top senders by message count:".to_string());
    for (e, c, _) in ranked.iter().take(10) {
        lines.push(format!("  {c:>6}  {e}"));
    }
    lines.push(String::new());

    ranked.sort_by_key(|(_, _, s)| std::cmp::Reverse(*s));
    lines.push("Top senders by total size:".to_string());
    for (e, _, s) in ranked.iter().take(10) {
        lines.push(format!("  {:>9}  {e}", human_bytes(*s)));
    }

    lines
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn fmt_date(ms: i64) -> String {
    if ms <= 0 {
        return "—".to_string();
    }
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "—".to_string())
}

fn fmt_date_short(ms: i64) -> String {
    if ms <= 0 {
        return "     ".to_string();
    }
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.format("%m-%d").to_string())
        .unwrap_or_else(|| "     ".to_string())
}
