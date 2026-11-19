//! A small reader for `robots.txt`.
//!
//! Following links beyond the page the user gave us is crawling, and
//! site owners say in `robots.txt` what crawlers may visit. We read the
//! group for our product token (`hiroi`) or, failing that, `*`, and
//! apply the longest matching Allow or Disallow rule, with `*` and `$`
//! patterns as most crawlers support them.

#[derive(Debug, Clone, Default)]
pub(crate) struct Robots {
    rules: Vec<(bool, String)>,
    pub crawl_delay: Option<f64>,
}

impl Robots {
    pub fn parse(text: &str, agent: &str) -> Self {
        let agent = agent.to_ascii_lowercase();
        let mut specific = Robots::default();
        let mut general = Robots::default();
        let mut has_specific = false;
        // Which groups the current lines belong to.
        let (mut to_specific, mut to_general, mut reading_agents) = (false, false, false);
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
            match key.as_str() {
                "user-agent" => {
                    if !reading_agents {
                        to_specific = false;
                        to_general = false;
                    }
                    reading_agents = true;
                    let name = value.to_ascii_lowercase();
                    if name == "*" {
                        to_general = true;
                    } else if agent.contains(&name) {
                        to_specific = true;
                        has_specific = true;
                    }
                }
                "allow" | "disallow" | "crawl-delay" => {
                    reading_agents = false;
                    for (group, wanted) in
                        [(&mut specific, to_specific), (&mut general, to_general)]
                    {
                        if !wanted {
                            continue;
                        }
                        match key.as_str() {
                            "crawl-delay" => group.crawl_delay = value.parse().ok(),
                            // An empty Disallow allows everything.
                            "disallow" if value.is_empty() => {}
                            rule => group.rules.push((rule == "allow", value.to_string())),
                        }
                    }
                }
                _ => {}
            }
        }
        if has_specific { specific } else { general }
    }

    /// Whether `path` (with query) may be fetched.
    pub fn allows(&self, path: &str) -> bool {
        self.rules
            .iter()
            .filter(|(_, pattern)| matches(pattern, path))
            .max_by_key(|(allow, pattern)| (pattern.len(), *allow))
            .is_none_or(|(allow, _)| *allow)
    }
}

/// Matches a robots pattern: a prefix, with `*` for any run of
/// characters and a trailing `$` for "ends here".
fn matches(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(pattern) => (pattern, true),
        None => (pattern, false),
    };
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or("");
    let Some(mut rest) = path.strip_prefix(first) else {
        return false;
    };
    for part in parts {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    !anchored || rest.is_empty() || pattern.ends_with('*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_match_wins_and_groups_are_chosen() {
        let text = "User-agent: *\nDisallow: /private\nAllow: /private/public\nCrawl-delay: 2\n\n\
                    User-agent: OtherBot\nDisallow: /\n";
        let robots = Robots::parse(text, "HiroiDownloader/0.1");
        assert!(robots.allows("/index.html"));
        assert!(!robots.allows("/private/secret"));
        assert!(robots.allows("/private/public/a"));
        assert_eq!(robots.crawl_delay, Some(2.0));
        assert!(!Robots::parse(text, "OtherBot/1").allows("/index.html"));
    }

    #[test]
    fn specific_group_replaces_the_general_one() {
        let text = "User-agent: *\nDisallow: /\n\nUser-agent: hiroi\nDisallow: /tmp\n";
        let robots = Robots::parse(text, "hiroi");
        assert!(robots.allows("/files/a.zip"));
        assert!(!robots.allows("/tmp/x"));
    }

    #[test]
    fn wildcards_and_anchors() {
        let robots = Robots::parse(
            "User-agent: *\nDisallow: /*.php$\nDisallow: /search*q=\n",
            "hiroi",
        );
        assert!(!robots.allows("/index.php"));
        assert!(robots.allows("/index.php?x=1"));
        assert!(!robots.allows("/search?q=rust"));
        assert!(Robots::parse("User-agent: *\nDisallow:\n", "hiroi").allows("/anything"));
    }
}
