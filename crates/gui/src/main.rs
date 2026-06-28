//! Iced frontend for Mailsweep.
//!
//! Note: iced's `tokio` feature is enabled (see workspace Cargo.toml) so that
//! `Task::perform` futures run on a tokio runtime — `reqwest` requires it.

use std::sync::Arc;

use iced::widget::{button, column, row, scrollable, text};
use iced::{Element, Length, Task};

use mailsweep_core::{
    config, group_by_domain, Cache, GmailAuth, GmailClient, MailProvider, SenderGroup,
};

const SCAN_LIMIT: usize = 1000;

pub fn main() -> iced::Result {
    iced::application("Mailsweep", App::update, App::view).run_with(App::new)
}

struct App {
    client: GmailClient,
    groups: Vec<SenderGroup>,
    status: String,
}

#[derive(Debug, Clone)]
enum Message {
    Scanned(Result<Vec<SenderGroup>, String>),
    Trash(usize),
    Spam(usize),
    Done(usize, Result<Outcome, String>),
}

#[derive(Debug, Clone)]
enum Outcome {
    Trashed(String, usize),
    Spammed(String, usize),
}

impl App {
    fn new() -> (Self, Task<Message>) {
        let auth = GmailAuth::new(config::secret_path(), config::token_cache_path(), config::SCOPES);
        let client = match Cache::open(config::cache_path()) {
            Ok(cache) => GmailClient::new(Arc::new(auth)).with_cache(cache),
            Err(e) => {
                eprintln!("mailsweep: metadata cache disabled: {e}");
                GmailClient::new(Arc::new(auth))
            }
        };
        let app = App {
            client: client.clone(),
            groups: Vec::new(),
            status: "Scanning inbox…".to_string(),
        };
        (app, Task::perform(scan(client), Message::Scanned))
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Scanned(Ok(groups)) => {
                self.status = format!("{} sender group(s)", groups.len());
                self.groups = groups;
                Task::none()
            }
            Message::Scanned(Err(e)) => {
                self.status = format!("Scan failed: {e}");
                Task::none()
            }
            Message::Trash(i) => self.action(i, true),
            Message::Spam(i) => self.action(i, false),
            Message::Done(i, Ok(outcome)) => {
                self.status = match &outcome {
                    Outcome::Trashed(d, n) => format!("Trashed {n} from {d}"),
                    Outcome::Spammed(d, n) => format!("Marked {n} from {d} as spam"),
                };
                if i < self.groups.len() {
                    self.groups.remove(i);
                }
                Task::none()
            }
            Message::Done(_, Err(e)) => {
                self.status = format!("Action failed: {e}");
                Task::none()
            }
        }
    }

    fn action(&mut self, i: usize, trash: bool) -> Task<Message> {
        let Some(g) = self.groups.get(i) else {
            return Task::none();
        };
        let client = self.client.clone();
        let ids = g.message_ids.clone();
        let domain = g.domain.clone();
        let n = ids.len();
        Task::perform(
            async move {
                if trash {
                    client
                        .trash(&ids)
                        .await
                        .map(|_| Outcome::Trashed(domain, n))
                        .map_err(|e| e.to_string())
                } else {
                    client
                        .mark_spam(&ids)
                        .await
                        .map(|_| Outcome::Spammed(domain, n))
                        .map_err(|e| e.to_string())
                }
            },
            move |r| Message::Done(i, r),
        )
    }

    fn view(&self) -> Element<'_, Message> {
        let mut list = column![].spacing(4);
        for (i, g) in self.groups.iter().enumerate() {
            let label = if g.unsubscribe.is_some() {
                format!("{} ✉", g.domain)
            } else {
                g.domain.clone()
            };
            list = list.push(
                row![
                    text(format!("{:>5}", g.count())).width(Length::Fixed(60.0)),
                    text(label).width(Length::Fill),
                    button("Trash").on_press(Message::Trash(i)),
                    button("Spam").on_press(Message::Spam(i)),
                ]
                .spacing(8),
            );
        }

        column![
            text(self.status.clone()).size(16),
            scrollable(list).height(Length::Fill),
        ]
        .spacing(8)
        .padding(12)
        .into()
    }
}

async fn scan(client: GmailClient) -> Result<Vec<SenderGroup>, String> {
    let ids = client
        .list_message_ids(Some("in:inbox"), SCAN_LIMIT)
        .await
        .map_err(|e| e.to_string())?;
    let metas = client
        .fetch_metadata(&ids)
        .await
        .map_err(|e| e.to_string())?;
    Ok(group_by_domain(&metas))
}
