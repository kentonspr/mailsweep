//! Parsing and acting on `List-Unsubscribe` headers (RFC 2369 / RFC 8058).

use anyhow::Result;
use reqwest::header::CONTENT_TYPE;

#[derive(Debug, Clone)]
pub struct UnsubscribeInfo {
    /// An `https://` unsubscribe endpoint, if advertised.
    pub http_url: Option<String>,
    /// A `mailto:` unsubscribe address, if advertised.
    pub mailto: Option<String>,
    /// True when the sender supports RFC 8058 one-click POST unsubscribe.
    pub one_click: bool,
}

/// Parse the `List-Unsubscribe` (and optional `List-Unsubscribe-Post`) headers.
///
/// `List-Unsubscribe` looks like: `<https://host/u?x>, <mailto:bye@host?subject=unsub>`.
pub fn parse(
    list_unsubscribe: Option<&str>,
    list_unsubscribe_post: Option<&str>,
) -> Option<UnsubscribeInfo> {
    let raw = list_unsubscribe?;

    let mut http_url = None;
    let mut mailto = None;
    for part in raw.split(',') {
        let p = part
            .trim()
            .trim_start_matches('<')
            .trim_end_matches('>')
            .trim();
        if p.starts_with("http") {
            http_url.get_or_insert_with(|| p.to_string());
        } else if p.starts_with("mailto:") {
            mailto.get_or_insert_with(|| p.to_string());
        }
    }

    if http_url.is_none() && mailto.is_none() {
        return None;
    }

    let one_click = list_unsubscribe_post
        .map(|v| v.to_ascii_lowercase().contains("one-click"))
        .unwrap_or(false);

    Some(UnsubscribeInfo {
        http_url,
        mailto,
        one_click,
    })
}

/// Perform an RFC 8058 one-click unsubscribe. Returns `Ok(true)` on HTTP success.
///
/// Only attempted when the sender advertised one-click support.
pub async fn one_click(http: &reqwest::Client, info: &UnsubscribeInfo) -> Result<bool> {
    if !info.one_click {
        return Ok(false);
    }
    let Some(url) = &info.http_url else {
        return Ok(false);
    };
    let resp = http
        .post(url)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body("List-Unsubscribe=One-Click")
        .send()
        .await?;
    Ok(resp.status().is_success())
}

/// Open the unsubscribe web page in the user's browser (manual fallback).
pub fn open_in_browser(info: &UnsubscribeInfo) -> Result<()> {
    if let Some(url) = &info.http_url {
        open::that(url)?;
    }
    Ok(())
}
