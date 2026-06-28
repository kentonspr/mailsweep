//! Ratatui frontend for Mailsweep.
//!
//! Opens immediately and streams the inbox sync into the UI, populating a
//! domain → sender tree as messages arrive. Three numbered panels (Accounts,
//! Domains, Details); the Domains panel has tabbed views (All / Subscriptions /
//! Attachments).
//!
//! Keys: `1`/`2`/`3` focus a panel · `Tab`/`Shift-Tab` switch view · `j`/`k`
//! move · `h`/`l` collapse/expand · `d` trash · `s` spam · `u` unsubscribe ·
//! `q` quit.

use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::time::Duration;

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
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use mailsweep_core::{
    config, group_messages, Cache, DomainGroup, FetchProgress, GmailAuth, GmailClient,
    MailProvider, MessageMeta, Profile, SenderEntry, UnsubscribeInfo,
};

const SCAN_LIMIT: usize = 1000;
const HELP: &str = "1/2/3 focus · Tab view · j/k move · h/l collapse/expand · \
    d trash · s spam · u unsubscribe · q quit";

/// Messages streamed from the background scan task into the UI.
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
        match self {
            View::All => 0,
            View::Subscriptions => 1,
            View::Attachments => 2,
        }
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

/// A flattened tree row: either a domain or one of its senders (when expanded).
enum Row<'a> {
    Domain(&'a DomainGroup),
    Sender(&'a DomainGroup, &'a SenderEntry),
}

/// The thing the current selection acts on (a whole domain or a single sender).
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
    view: View,
    groups: Vec<DomainGroup>,
    expanded: HashSet<String>,
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
            view: View::All,
            groups: Vec::new(),
            expanded: HashSet::new(),
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

    fn rebuild_groups(&mut self) {
        self.groups = group_messages(&self.filtered_metas());
        self.clamp_selection();
    }

    fn rows(&self) -> Vec<Row<'_>> {
        let mut rows = Vec::new();
        for g in &self.groups {
            rows.push(Row::Domain(g));
            if self.expanded.contains(&g.domain) {
                for s in &g.senders {
                    rows.push(Row::Sender(g, s));
                }
            }
        }
        rows
    }

    fn row_count(&self) -> usize {
        self.groups
            .iter()
            .map(|g| 1 + if self.expanded.contains(&g.domain) { g.senders.len() } else { 0 })
            .sum()
    }

    fn clamp_selection(&mut self) {
        let n = self.row_count();
        self.selected = if n == 0 { 0 } else { self.selected.min(n - 1) };
    }

    // ---- navigation ---------------------------------------------------------

    fn move_down(&mut self) {
        if self.focus == Panel::Details {
            self.detail_scroll = self.detail_scroll.saturating_add(1);
            return;
        }
        let n = self.row_count();
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

    fn expand(&mut self) {
        if let Some(Target { label, .. }) = self.target() {
            if self.is_domain_row() {
                self.expanded.insert(label);
            }
        }
    }

    fn collapse(&mut self) {
        let domain = match self.selection() {
            Some((domain, is_domain)) => {
                if !is_domain {
                    // On a sender row: collapse the parent and reselect it.
                    let pos = self
                        .rows()
                        .iter()
                        .position(|r| matches!(r, Row::Domain(g) if g.domain == domain));
                    if let Some(p) = pos {
                        self.selected = p;
                    }
                }
                domain
            }
            None => return,
        };
        self.expanded.remove(&domain);
    }

    /// (domain name, is this row a domain row?)
    fn selection(&self) -> Option<(String, bool)> {
        match self.rows().get(self.selected)? {
            Row::Domain(g) => Some((g.domain.clone(), true)),
            Row::Sender(g, _) => Some((g.domain.clone(), false)),
        }
    }

    fn is_domain_row(&self) -> bool {
        matches!(self.rows().get(self.selected), Some(Row::Domain(_)))
    }

    fn target(&self) -> Option<Target> {
        match self.rows().get(self.selected)? {
            Row::Domain(g) => Some(Target {
                ids: g.message_ids.clone(),
                label: g.domain.clone(),
                unsubscribe: g.unsubscribe.clone(),
            }),
            Row::Sender(_, s) => Some(Target {
                ids: s.message_ids.clone(),
                label: s.email.clone(),
                unsubscribe: s.unsubscribe.clone(),
            }),
        }
    }

    /// Drop the given messages from the model (after trash/spam) and regroup.
    fn remove_messages(&mut self, ids: &[String]) {
        let set: HashSet<&str> = ids.iter().map(String::as_str).collect();
        self.metas.retain(|m| !set.contains(m.id.as_str()));
        self.rebuild_groups();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = setup()?;
    let (tx, rx) = mpsc::unbounded_channel();
    let scan_client = client.clone();
    tokio::spawn(async move { run_scan(scan_client, tx).await });
    run(client, rx).await
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

    // Cheap secondary query to power the Attachments view.
    if let Ok(att) = client
        .list_message_ids(Some("in:inbox has:attachment"), SCAN_LIMIT)
        .await
    {
        let _ = tx.send(ScanEvent::AttachmentIds(att.into_iter().collect()));
    }

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
}

async fn run(client: GmailClient, mut rx: UnboundedReceiver<ScanEvent>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut app = App::new();

    let result = event_loop(&mut terminal, &client, &mut app, &mut rx).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

async fn event_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    client: &GmailClient,
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
                if key.kind == KeyEventKind::Press && handle_key(app, client, key.code).await? {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Returns `Ok(true)` when the user asked to quit.
async fn handle_key(app: &mut App, client: &GmailClient, code: KeyCode) -> Result<bool> {
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
        KeyCode::Char('d') => act(app, client, Action::Trash).await,
        KeyCode::Char('s') => act(app, client, Action::Spam).await,
        KeyCode::Char('u') => app.status = unsubscribe(app, client).await,
        _ => {}
    }
    Ok(false)
}

#[derive(Clone, Copy)]
enum Action {
    Trash,
    Spam,
}

async fn act(app: &mut App, client: &GmailClient, action: Action) {
    let Some(target) = app.target() else { return };
    let n = target.ids.len();

    let result = match action {
        Action::Trash => client.trash(&target.ids).await,
        Action::Spam => client.mark_spam(&target.ids).await,
    };
    let verb = match action {
        Action::Trash => "Trashed",
        Action::Spam => "Marked as spam",
    };

    app.status = match result {
        Ok(()) => {
            app.remove_messages(&target.ids);
            format!("{verb} {n} messages from {}", target.label)
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

fn render_domains(f: &mut Frame, app: &App, area: Rect) {
    let block = panel_block(app, Panel::Domains, format!("2 Domains ({})", app.groups.len()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // [tabs][optional progress gauge][list]
    let mut constraints = vec![Constraint::Length(1)];
    let syncing = !app.sync.done;
    if syncing {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(1));
    let chunks = Layout::vertical(constraints).split(inner);

    let titles: Vec<Line> = View::ALL.iter().map(|v| Line::from(v.title())).collect();
    let tabs = Tabs::new(titles)
        .select(app.view.index())
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .divider(" ");
    f.render_widget(tabs, chunks[0]);

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
        f.render_widget(gauge, chunks[1]);
    }

    let list_area = *chunks.last().expect("list chunk present");
    let items: Vec<ListItem> = app
        .rows()
        .iter()
        .map(|row| match row {
            Row::Domain(g) => {
                let marker = if app.expanded.contains(&g.domain) {
                    "▾"
                } else {
                    "▸"
                };
                let unsub = if g.unsubscribe.is_some() { " ✉" } else { "" };
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{marker} ")),
                    Span::styled(
                        format!("{:>5}  ", g.count()),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(g.domain.clone()),
                    Span::raw(unsub),
                ]))
            }
            Row::Sender(_, s) => {
                let unsub = if s.unsubscribe.is_some() { " ✉" } else { "" };
                let who = s.name.clone().unwrap_or_else(|| s.email.clone());
                ListItem::new(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(
                        format!("{:>5}  ", s.count()),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(format!("{who} <{}>", s.email)),
                    Span::raw(unsub),
                ]))
            }
        })
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

    let bold = |s: String| Line::from(Span::styled(s, Style::default().add_modifier(Modifier::BOLD)));
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
                Line::from(format!("{} messages · {} senders", g.count(), g.senders.len())),
                Line::from(""),
            ];
            if let Some(u) = &g.unsubscribe {
                lines.push(unsub_line(u));
                lines.push(Line::from(""));
            }
            lines.push(underline("Top senders (press l to expand):"));
            for s in g.senders.iter().take(10) {
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
            lines.push(underline("Recent subjects:"));
            for subj in &s.sample_subjects {
                lines.push(Line::from(format!("· {subj}")));
            }
            lines
        }
    }
}
