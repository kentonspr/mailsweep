//! Ratatui frontend for Mailsweep.
//!
//! Streams the inbox sync into a domain → sender → message tree. Three numbered
//! panels (Accounts, Domains, Details); the Domains panel has tabbed views
//! (All / Subscriptions / Attachments).
//!
//! Keys: `1`/`2`/`3` focus · `Tab`/`Shift-Tab` switch view · `j`/`k` move ·
//! `h`/`l` collapse/expand · `Enter` load attachments · `a` archive
//! attachments · `d` trash · `s` spam · `u` unsubscribe · `q` quit.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::sleep;

use mailsweep_core::{
    archive_attachments, config, group_messages, ArchiveItem, AttachmentInfo, Cache, DomainGroup,
    FetchProgress, GmailAuth, GmailClient, MailProvider, MessageMeta, Profile, SenderEntry,
    UnsubscribeInfo,
};

const SCAN_LIMIT: usize = 1000;
const HELP: &str = "Tab view · o sort · j/k move · h/l fold · Space mark · c clear · \
    Enter attach · a archive · A archive+del · d trash · s spam · u unsub · q quit";

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
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Panel {
    Accounts,
    Domains,
    Details,
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
    status: String,
}

impl App {
    fn new() -> Self {
        Self {
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
            status: HELP.to_string(),
        }
    }

    // ---- scan event handling ------------------------------------------------

    fn apply(&mut self, event: ScanEvent) {
        match event {
            ScanEvent::Account(p) => self.account = Some(p),
            ScanEvent::Status(s) => self.sync.message = s,
            ScanEvent::Notice(s) => self.status = s,
            ScanEvent::Removed(ids) => self.remove_messages(&ids),
            ScanEvent::AttachmentDetails(id, list) => {
                self.attachments.insert(id, list);
                // Re-sort so newly-known sizes take effect in the Attachments view.
                if self.view == View::Attachments && self.sort == SortMode::Size {
                    self.rebuild_groups();
                }
            }
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
            if let Some(atts) = self.attachments.get(&m.id) {
                let total: u64 = atts.iter().map(|a| a.size).sum();
                if total > 0 {
                    return total;
                }
            }
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
        if self.focus == Panel::Details {
            self.detail_scroll = self.detail_scroll.saturating_add(1);
            return;
        }
        let n = self.rows().len();
        if n > 0 {
            self.selected = (self.selected + 1).min(n - 1);
            self.detail_scroll = 0;
        }
    }

    fn move_up(&mut self) {
        if self.focus == Panel::Details {
            self.detail_scroll = self.detail_scroll.saturating_sub(1);
            return;
        }
        self.selected = self.selected.saturating_sub(1);
        self.detail_scroll = 0;
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
        let has_attachment = |m: &MessageMeta| self.attachment_ids.contains(&m.id);
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
                .filter(|m| self.marked.contains(&m.id) && has_attachment(m))
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
                .flat_map(|s| {
                    s.messages
                        .iter()
                        .filter(|m| has_attachment(m))
                        .map(move |m| item(g, s, m))
                })
                .collect(),
            Some(Row::Sender(g, s)) => s
                .messages
                .iter()
                .filter(|m| has_attachment(m))
                .map(|m| item(g, s, m))
                .collect(),
            Some(Row::Message(g, s, m)) if has_attachment(m) => vec![item(g, s, m)],
            _ => Vec::new(),
        }
    }

    /// Drop messages from the model (after trash/spam) and regroup.
    fn remove_messages(&mut self, ids: &[String]) {
        let set: HashSet<&str> = ids.iter().map(String::as_str).collect();
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
    let client = setup()?;
    let (tx, rx) = mpsc::unbounded_channel();
    let scan_client = client.clone();
    let scan_tx = tx.clone();
    tokio::spawn(async move { run_scan(scan_client, scan_tx).await });
    run(client, rx, tx).await
}

fn setup() -> Result<GmailClient> {
    let auth = GmailAuth::new(config::secret_path(), config::token_cache_path(), config::SCOPES);
    let cache = Cache::open(config::cache_path())?;
    Ok(GmailClient::new(Arc::new(auth)).with_cache(cache))
}

async fn run_scan(client: GmailClient, tx: UnboundedSender<ScanEvent>) {
    let _ = tx.send(ScanEvent::Status("Authenticating…".to_string()));
    match client.profile().await {
        Ok(p) => {
            let _ = tx.send(ScanEvent::Account(p));
        }
        Err(e) => {
            let _ = tx.send(ScanEvent::Failed(e.to_string()));
            return;
        }
    }

    let _ = tx.send(ScanEvent::Status("Listing inbox…".to_string()));
    let ids = match client.list_message_ids(Some("in:inbox"), SCAN_LIMIT).await {
        Ok(v) => v,
        Err(e) => {
            let _ = tx.send(ScanEvent::Failed(e.to_string()));
            return;
        }
    };
    let _ = tx.send(ScanEvent::Listed(ids.len()));

    let attachment_ids = client
        .list_message_ids(Some("in:inbox has:attachment"), SCAN_LIMIT)
        .await
        .unwrap_or_default();
    let _ = tx.send(ScanEvent::AttachmentIds(
        attachment_ids.iter().cloned().collect(),
    ));

    let progress_tx = tx.clone();
    let report = client
        .fetch_metadata_with(&ids, |p: FetchProgress, batch: &[MessageMeta]| {
            let _ = progress_tx.send(ScanEvent::Progress {
                resolved: p.resolved,
                total: p.total,
                metas: batch.to_vec(),
            });
        })
        .await;
    let report = match report {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.send(ScanEvent::Failed(e.to_string()));
            return;
        }
    };

    let resolved = report.from_cache + report.fetched;
    let mut summary = format!("Synced · {resolved}/{} resolved", report.requested);
    if !report.batch_errors.is_empty() {
        summary.push_str(&format!(" · ⚠ {}", report.batch_errors[0]));
    }
    let _ = tx.send(ScanEvent::Done(summary));

    // Background: fetch actual attachment filenames/sizes for every attachment
    // message, paced to stay well under quota. Sizes fill in the Attachments
    // view (and make Enter instant) as they arrive.
    let total = attachment_ids.len();
    for (i, id) in attachment_ids.into_iter().enumerate() {
        if let Ok(list) = client.message_attachments(&id).await {
            let _ = tx.send(ScanEvent::AttachmentDetails(id, list));
        }
        if i % 25 == 0 {
            let _ = tx.send(ScanEvent::Notice(format!(
                "Loading attachment sizes… {}/{total}",
                i + 1
            )));
        }
        sleep(Duration::from_millis(120)).await;
    }
    if total > 0 {
        let _ = tx.send(ScanEvent::Notice(format!(
            "Attachment sizes loaded ({total} messages)"
        )));
    }
}

async fn run(
    client: GmailClient,
    mut rx: UnboundedReceiver<ScanEvent>,
    tx: UnboundedSender<ScanEvent>,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut app = App::new();

    let result = event_loop(&mut terminal, &client, &tx, &mut app, &mut rx).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

async fn event_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    client: &GmailClient,
    tx: &UnboundedSender<ScanEvent>,
    app: &mut App,
    rx: &mut UnboundedReceiver<ScanEvent>,
) -> Result<()> {
    loop {
        while let Ok(event) = rx.try_recv() {
            app.apply(event);
        }
        terminal.draw(|f| ui(f, app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press && handle_key(app, client, tx, key.code).await? {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Returns `Ok(true)` when the user asked to quit.
async fn handle_key(
    app: &mut App,
    client: &GmailClient,
    tx: &UnboundedSender<ScanEvent>,
    code: KeyCode,
) -> Result<bool> {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
        KeyCode::Char('1') => app.focus = Panel::Accounts,
        KeyCode::Char('2') => app.focus = Panel::Domains,
        KeyCode::Char('3') => app.focus = Panel::Details,
        KeyCode::Tab => app.set_view(app.view.next()),
        KeyCode::BackTab => app.set_view(app.view.prev()),
        KeyCode::Char('l') | KeyCode::Right => app.expand(),
        KeyCode::Char('h') | KeyCode::Left => app.collapse(),
        KeyCode::Char('j') | KeyCode::Down => app.move_down(),
        KeyCode::Char('k') | KeyCode::Up => app.move_up(),
        KeyCode::Char(' ') => app.toggle_mark(),
        KeyCode::Char('o') => {
            app.sort = app.sort.next();
            app.rebuild_groups();
            app.status = format!("Sort: {}", app.sort.label());
        }
        KeyCode::Char('c') => {
            app.marked.clear();
            app.status = "Cleared marks".to_string();
        }
        KeyCode::Enter => load_attachments(app, client).await,
        KeyCode::Char('a') => archive(app, client, tx, false),
        KeyCode::Char('A') => archive(app, client, tx, true),
        KeyCode::Char('d') => act(app, client, Action::Trash).await,
        KeyCode::Char('s') => act(app, client, Action::Spam).await,
        KeyCode::Char('u') => app.status = unsubscribe(app, client).await,
        _ => {}
    }
    Ok(false)
}

async fn load_attachments(app: &mut App, client: &GmailClient) {
    let Some(id) = app.selected_message_id() else { return };
    if app.attachments.contains_key(&id) {
        return;
    }
    app.status = "Loading attachments…".to_string();
    match client.message_attachments(&id).await {
        Ok(list) => {
            app.attachments.insert(id, list);
            app.status = HELP.to_string();
        }
        Err(e) => app.status = format!("Attachment load failed: {e}"),
    }
}

fn archive(app: &mut App, client: &GmailClient, tx: &UnboundedSender<ScanEvent>, delete_after: bool) {
    let items = app.archive_items();
    if items.is_empty() {
        app.status = "No attachments to archive in selection".to_string();
        return;
    }
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

    let verb = if delete_after { "Archiving + deleting" } else { "Archiving" };
    app.status = format!("{verb} attachments from {} message(s)…", items.len());
    let client = client.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let ids: Vec<String> = items.iter().map(|i| i.message_id.clone()).collect();
        let msg = match archive_attachments(&client, &items, &path).await {
            Ok(s) if delete_after => match client.trash(&ids).await {
                Ok(()) => {
                    let _ = tx.send(ScanEvent::Removed(ids.clone()));
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
        let _ = tx.send(ScanEvent::Notice(msg));
    });
}

#[derive(Clone, Copy)]
enum Action {
    Trash,
    Spam,
}

async fn act(app: &mut App, client: &GmailClient, action: Action) {
    let (ids, label) = app.action_ids();
    if ids.is_empty() {
        return;
    }
    let n = ids.len();

    let result = match action {
        Action::Trash => client.trash(&ids).await,
        Action::Spam => client.mark_spam(&ids).await,
    };
    let verb = match action {
        Action::Trash => "Trashed",
        Action::Spam => "Marked as spam",
    };

    app.status = match result {
        Ok(()) => {
            app.remove_messages(&ids);
            format!("{verb} {n} message(s) from {label}")
        }
        Err(e) => format!("Action failed: {e}"),
    };
}

async fn unsubscribe(app: &App, client: &GmailClient) -> String {
    let Some(target) = app.target() else {
        return "Nothing selected".to_string();
    };
    let Some(info) = &target.unsubscribe else {
        return format!("No unsubscribe info for {}", target.label);
    };

    if info.one_click {
        match client.unsubscribe_one_click(info).await {
            Ok(true) => return format!("Unsubscribed from {} (one-click)", target.label),
            Ok(false) => {}
            Err(e) => return format!("One-click unsubscribe failed: {e}"),
        }
    }

    if info.http_url.is_some() {
        match mailsweep_core::unsubscribe::open_in_browser(info) {
            Ok(()) => format!("Opened unsubscribe page for {}", target.label),
            Err(e) => format!("Could not open browser: {e}"),
        }
    } else if let Some(mailto) = &info.mailto {
        format!("Unsubscribe by emailing: {mailto}")
    } else {
        format!("No usable unsubscribe method for {}", target.label)
    }
}

// ---- rendering --------------------------------------------------------------

fn ui(f: &mut Frame, app: &App) {
    let rows = Layout::vertical([
        Constraint::Length(5),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(f.area());

    render_accounts(f, app, rows[0]);

    let body = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(rows[1]);
    render_domains(f, app, body[0]);
    render_details(f, app, body[1]);

    f.render_widget(Paragraph::new(app.status.clone()), rows[2]);
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
    let mut lines = match &app.account {
        Some(p) => vec![
            Line::from(Span::styled(
                format!("▶ {}", p.email),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                "{} messages · {} threads in mailbox",
                p.messages_total, p.threads_total
            )),
        ],
        None => vec![Line::from("Loading account…")],
    };
    lines.push(Line::from(Span::styled(
        format!("Sync: {}", app.sync.message),
        Style::default().fg(Color::Cyan),
    )));

    f.render_widget(
        Paragraph::new(lines).block(panel_block(app, Panel::Accounts, "1 Accounts")),
        area,
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
    let title = format!(
        "2 Domains ({}{}) · sort {}",
        app.groups.len(),
        marked,
        app.sort.label()
    );
    let block = panel_block(app, Panel::Domains, title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // [tabs][column header][optional gauge][list]
    let syncing = !app.sync.done;
    let mut constraints = vec![Constraint::Length(1), Constraint::Length(1)];
    if syncing {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(1));
    let chunks = Layout::vertical(constraints).split(inner);

    f.render_widget(Paragraph::new(tabs_line(app.view)), chunks[0]);

    let header = Line::from(Span::styled(
        format!("{:4}{:>7} {:>8}  {}", "", "Senders", "Messages", "Name"),
        Style::default().add_modifier(Modifier::UNDERLINED),
    ));
    f.render_widget(Paragraph::new(header), chunks[1]);

    if syncing {
        let ratio = if app.sync.total > 0 {
            app.sync.resolved as f64 / app.sync.total as f64
        } else {
            0.0
        };
        let gauge = Gauge::default()
            .gauge_style(Style::default().fg(Color::Cyan))
            .ratio(ratio.clamp(0.0, 1.0))
            .label(format!("{}/{}", app.sync.resolved, app.sync.total));
        f.render_widget(gauge, chunks[2]);
    }

    let list_area = *chunks.last().expect("list chunk present");
    let items: Vec<ListItem> = app.rows().iter().map(|r| ListItem::new(row_line(app, r))).collect();

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
    let show_size = app.sort == SortMode::Size;
    match row {
        Row::Domain(g) => {
            let marker = if app.expanded_domains.contains(&g.domain) {
                '▾'
            } else {
                '▸'
            };
            let mark = app.mark_glyph(&g.message_ids());
            let unsub = if g.unsubscribe.is_some() { " ✉" } else { "" };
            let size = if show_size {
                format!("  ({})", human_bytes(app.domain_size(g)))
            } else {
                String::new()
            };
            Line::from(format!(
                "{mark}{marker}  {:>7} {:>8}  {}{unsub}{size}",
                g.sender_count(),
                g.count(),
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
            let size = if show_size {
                format!("  ({})", human_bytes(app.sender_size(s)))
            } else {
                String::new()
            };
            Line::from(format!(
                "{mark} {marker} {:>7} {:>8}    {who} <{}>{unsub}{size}",
                "",
                s.count(),
                s.email
            ))
        }
        Row::Message(_, _, m) => {
            let mark = app.mark_glyph(std::slice::from_ref(&m.id));
            Line::from(format!(
                "{mark}   {:>7} {:>8}    {}  {:>8}  {}",
                "",
                "",
                fmt_date_short(m.internal_date),
                human_bytes(app.msg_size(m)),
                m.subject
            ))
        }
    }
}

fn render_details(f: &mut Frame, app: &App, area: Rect) {
    let widget = Paragraph::new(detail_lines(app))
        .block(panel_block(app, Panel::Details, "3 Details"))
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

    let bold =
        |s: String| Line::from(Span::styled(s, Style::default().add_modifier(Modifier::BOLD)));
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
                    lines.push(Line::from("Has attachments — press Enter to load details."))
                }
                None => {}
            }
            lines
        }
    }
}

/// Sort a tree of domains/senders/messages in place by the chosen mode.
fn apply_sort(groups: &mut [DomainGroup], sort: SortMode, size_of: &impl Fn(&MessageMeta) -> u64) {
    let sender_size = |s: &SenderEntry| -> u64 { s.messages.iter().map(|m| size_of(m)).sum() };
    let sender_recent = |s: &SenderEntry| s.messages.iter().map(|m| m.internal_date).max().unwrap_or(0);
    let domain_size = |g: &DomainGroup| -> u64 { g.senders.iter().map(|s| sender_size(s)).sum() };
    let domain_recent =
        |g: &DomainGroup| g.senders.iter().flat_map(|s| &s.messages).map(|m| m.internal_date).max().unwrap_or(0);

    for g in groups.iter_mut() {
        for s in g.senders.iter_mut() {
            match sort {
                SortMode::Size => s.messages.sort_by(|a, b| size_of(b).cmp(&size_of(a))),
                _ => s.messages.sort_by(|a, b| b.internal_date.cmp(&a.internal_date)),
            }
        }
        match sort {
            SortMode::Messages => g.senders.sort_by(|a, b| b.count().cmp(&a.count())),
            SortMode::Size => g.senders.sort_by(|a, b| sender_size(b).cmp(&sender_size(a))),
            SortMode::Recent => g.senders.sort_by(|a, b| sender_recent(b).cmp(&sender_recent(a))),
        }
    }
    match sort {
        SortMode::Messages => groups.sort_by(|a, b| b.count().cmp(&a.count())),
        SortMode::Size => groups.sort_by(|a, b| domain_size(b).cmp(&domain_size(a))),
        SortMode::Recent => groups.sort_by(|a, b| domain_recent(b).cmp(&domain_recent(a))),
    }
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
