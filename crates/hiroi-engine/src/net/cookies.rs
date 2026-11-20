//! Cookies for one download.
//!
//! Two kinds of cookies meet here. The user's own (from `--cookie`, or
//! handed over by the browser) belong to the site the download started
//! on, and go only to that host and its subdomains, never to a mirror
//! or a CDN a redirect leads to. Cookies a server sets on the way, such
//! as the session cookie a download page sets before it redirects to
//! the file, are kept in a jar that lives in memory for this download
//! only, follows the usual domain and path rules, and is never written
//! anywhere.

use reqwest::Url;
use reqwest::cookie::{CookieStore, Jar};
use reqwest::header::HeaderValue;

#[derive(Debug, Default)]
pub(crate) struct DownloadCookies {
    /// The user's cookies, and the host they belong to.
    own: Option<(String, Vec<(String, String)>)>,
    /// Cookies servers set; `None` when the jar is turned off.
    jar: Option<Jar>,
}

impl DownloadCookies {
    /// `own` is a Cookie header value (`a=1; b=2`) for `site`.
    pub(crate) fn new(site: &Url, own: Option<&str>, jar: bool) -> Self {
        let own = own.and_then(|header| {
            let host = site.host_str()?.to_ascii_lowercase();
            Some((host, pairs(header)))
        });
        Self {
            own,
            jar: jar.then(Jar::default),
        }
    }

    fn own_for(&self, url: &Url) -> &[(String, String)] {
        let Some((site, cookies)) = &self.own else {
            return &[];
        };
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        let matches = host == *site || host.ends_with(&format!(".{site}"));
        if matches { cookies } else { &[] }
    }
}

impl CookieStore for DownloadCookies {
    fn set_cookies(&self, headers: &mut dyn Iterator<Item = &HeaderValue>, url: &Url) {
        if let Some(jar) = &self.jar {
            jar.set_cookies(headers, url);
        }
    }

    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        let mut merged: Vec<(String, String)> = self.own_for(url).to_vec();
        if let Some(header) = self.jar.as_ref().and_then(|jar| jar.cookies(url)) {
            // What the server set last wins over what we came with.
            for (name, value) in pairs(header.to_str().unwrap_or_default()) {
                match merged.iter_mut().find(|(known, _)| *known == name) {
                    Some(pair) => pair.1 = value,
                    None => merged.push((name, value)),
                }
            }
        }
        if merged.is_empty() {
            return None;
        }
        let text: Vec<String> = merged.iter().map(|(n, v)| format!("{n}={v}")).collect();
        HeaderValue::from_str(&text.join("; ")).ok()
    }
}

fn pairs(header: &str) -> Vec<(String, String)> {
    header
        .split(';')
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            let name = name.trim();
            (!name.is_empty()).then(|| (name.to_string(), value.trim().to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn own_cookies_stay_with_their_site() {
        let cookies = DownloadCookies::new(&url("https://site.example/f"), Some("a=1; b=2"), false);
        let sent = |u: &str| {
            cookies
                .cookies(&url(u))
                .map(|v| v.to_str().unwrap().to_string())
        };
        assert_eq!(sent("https://site.example/x").as_deref(), Some("a=1; b=2"));
        assert_eq!(
            sent("https://dl.site.example/x").as_deref(),
            Some("a=1; b=2")
        );
        assert_eq!(sent("https://cdn.other.example/x"), None);
        assert_eq!(sent("https://evilsite.example/x"), None);
    }

    #[test]
    fn set_cookies_are_kept_only_with_the_jar() {
        let page = url("https://site.example/start");
        let set = HeaderValue::from_static("session=abc; Path=/");
        let with = DownloadCookies::new(&page, Some("a=1"), true);
        with.set_cookies(&mut std::iter::once(&set), &page);
        let sent = with.cookies(&url("https://site.example/file")).unwrap();
        assert_eq!(sent.to_str().unwrap(), "a=1; session=abc");

        let without = DownloadCookies::new(&page, None, false);
        without.set_cookies(&mut std::iter::once(&set), &page);
        assert!(without.cookies(&url("https://site.example/file")).is_none());
    }
}
