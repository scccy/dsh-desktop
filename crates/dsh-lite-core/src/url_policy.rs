//! Policy for the tokenized `dsh web` URL.
//!
//! The `dsh web: <url>` line carries a one-shot process token in the `token`
//! query parameter. The shell loads that URL once; the server seeds its session
//! cookie and redirects to the clean URL. From that point on the token must not
//! leave the shell: every log line, error page and crash report is built from
//! [`redact_text`] output, and the window is only considered authenticated once
//! the loaded URL is [`is_token_free`].
//!
//! The token is compared case-insensitively by parameter *name* only; values are
//! never parsed, never trimmed and never compared.

use std::net::{Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

/// Replacement written in place of a token value.
const REDACTED: &str = "<redacted>";

/// Name of the query parameter that carries the one-shot token.
const TOKEN_KEY: &str = "token";

/// Value of the `token` query parameter, if the URL carries one.
///
/// The parameter name is matched ASCII-case-insensitively. An empty value still
/// counts as present, so [`is_token_free`] reports `false` for `?token=` and
/// [`clean_url`] strips it.
pub fn token_of(url: &Url) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key.eq_ignore_ascii_case(TOKEN_KEY))
        .map(|(_, value)| value.into_owned())
}

/// Copy of `url` with every `token` parameter removed.
///
/// Path, fragment and all other parameters keep their original order and
/// encoding, including a trailing empty query (`?`).
pub fn clean_url(url: &Url) -> Url {
    let mut cleaned = url.clone();
    let Some(query) = url.query() else {
        return cleaned;
    };
    let segments: Vec<&str> = query.split('&').collect();
    if !segments.iter().any(|segment| is_token_segment(segment)) {
        return cleaned;
    }
    let kept: Vec<&str> = segments
        .into_iter()
        .filter(|segment| !is_token_segment(segment))
        .collect();
    if kept.is_empty() {
        cleaned.set_query(None);
    } else {
        cleaned.set_query(Some(&kept.join("&")));
    }
    cleaned
}

/// Whether the URL carries no `token` query parameter.
pub fn is_token_free(url: &Url) -> bool {
    match url.query() {
        Some(query) => !query.split('&').any(is_token_segment),
        None => true,
    }
}

/// `raw` with the value of every `?token=`/`&token=` parameter replaced.
///
/// The input is treated as text, never parsed: malformed URLs, bare query
/// strings and log fragments are all handled without panicking. Values end at the
/// next `&` or `#`, matching URL parameter syntax.
pub fn redact_url(raw: &str) -> String {
    redact_token_values(raw, ValueEnd::Query)
}

/// `text` with every `token=VALUE` fragment replaced.
///
/// Values end at whitespace, quotes, `&`, `#` or common punctuation, so log
/// lines, error pages and crash reports can pass arbitrary text through it. The
/// function is idempotent: redacting twice changes nothing.
pub fn redact_text(text: &str) -> String {
    redact_token_values(text, ValueEnd::Text)
}

/// Whether `url` is plain HTTP(S) served by this machine's loopback interface.
///
/// Only the literal addresses `127.0.0.1` and `::1` qualify; `localhost` is
/// rejected because it resolves through the host resolver, which a local
/// `hosts` file or DNS suffix can redirect anywhere.
pub fn is_loopback_http(url: &Url) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    match url.host() {
        Some(Host::Ipv4(address)) => address == Ipv4Addr::LOCALHOST,
        Some(Host::Ipv6(address)) => address == Ipv6Addr::LOCALHOST,
        Some(Host::Domain(_)) | None => false,
    }
}

/// Whether `candidate` points at the same server as `current`.
///
/// Scheme, host and effective port (the default for the scheme when absent) must
/// match; path, query and fragment are ignored. Used to decide whether a
/// navigation still belongs to the `dsh web` instance the shell connected to.
pub fn shares_target(current: &Url, candidate: &Url) -> bool {
    current.scheme() == candidate.scheme()
        && current.host() == candidate.host()
        && current.port_or_known_default() == candidate.port_or_known_default()
}

/// Where a redacted value stops when scanning raw text.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ValueEnd {
    /// URL syntax: `&` and `#` only.
    Query,
    /// Free text: whitespace, quotes and punctuation in addition to URL syntax.
    Text,
}

impl ValueEnd {
    fn terminates(self, ch: char) -> bool {
        match self {
            Self::Query => matches!(ch, '&' | '#'),
            Self::Text => {
                ch.is_whitespace()
                    || matches!(
                        ch,
                        '&' | '#' | '"' | '\'' | ',' | ';' | ')' | ']' | '}' | '|'
                    )
            }
        }
    }
}

/// Whether a raw query segment (`key=value` or `key`) uses the token key.
fn is_token_segment(segment: &str) -> bool {
    segment
        .split('=')
        .next()
        .unwrap_or_default()
        .eq_ignore_ascii_case(TOKEN_KEY)
}

/// Redacts every `token=VALUE` assignment visible from `at`, honouring `value_end`.
fn redact_token_values(text: &str, value_end: ValueEnd) -> String {
    let mut output = String::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        let at_boundary = match value_end {
            ValueEnd::Query => index == 0 || matches!(text.as_bytes()[index - 1], b'?' | b'&'),
            ValueEnd::Text => true,
        };
        if at_boundary && let Some(end) = assignment_end(text, index, value_end) {
            output.push_str(&text[index..index + TOKEN_KEY.len()]);
            output.push('=');
            output.push_str(REDACTED);
            index = end;
            continue;
        }
        let Some(ch) = text[index..].chars().next() else {
            break;
        };
        output.push(ch);
        index += ch.len_utf8();
    }
    output
}

/// If `text[at..]` starts with `token=`, returns the index just past the value.
fn assignment_end(text: &str, at: usize, value_end: ValueEnd) -> Option<usize> {
    let rest = &text[at..];
    if !rest.get(..TOKEN_KEY.len())?.eq_ignore_ascii_case(TOKEN_KEY) {
        return None;
    }
    let value = rest[TOKEN_KEY.len()..].strip_prefix('=')?;
    let value_len = value
        .find(|ch| value_end.terminates(ch))
        .unwrap_or(value.len());
    Some(at + TOKEN_KEY.len() + 1 + value_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Url {
        Url::parse(raw).expect("test URL should parse")
    }

    #[test]
    fn token_of_reports_absent_parameter() {
        assert_eq!(token_of(&parse("http://127.0.0.1:4100/")), None);
        assert_eq!(
            token_of(&parse("http://127.0.0.1:4100/?port=1&profile=web")),
            None
        );
        assert_eq!(token_of(&parse("http://127.0.0.1:4100/?tokens=1")), None);
    }

    #[test]
    fn token_of_reads_value_among_many_parameters() {
        let url = parse("http://127.0.0.1:4100/app?port=1&token=abc123&profile=web#top");
        assert_eq!(token_of(&url).as_deref(), Some("abc123"));
    }

    #[test]
    fn token_of_accepts_empty_value() {
        let url = parse("http://127.0.0.1:4100/?token=");
        assert_eq!(token_of(&url).as_deref(), Some(""));
        assert!(!is_token_free(&url));
        assert_eq!(clean_url(&url).as_str(), "http://127.0.0.1:4100/");
    }

    #[test]
    fn token_of_matches_name_case_insensitively() {
        assert_eq!(
            token_of(&parse("http://127.0.0.1:4100/?TOKEN=abc")).as_deref(),
            Some("abc")
        );
        assert_eq!(
            token_of(&parse("http://127.0.0.1:4100/?ToKeN=abc")).as_deref(),
            Some("abc")
        );
        assert!(
            clean_url(&parse("http://127.0.0.1:4100/?TOKEN=abc"))
                .query()
                .is_none()
        );
    }

    #[test]
    fn token_of_decodes_percent_escapes() {
        assert_eq!(
            token_of(&parse("http://127.0.0.1:4100/?token=a%2Bb")).as_deref(),
            Some("a+b")
        );
    }

    #[test]
    fn clean_url_keeps_other_parameters_and_fragment() {
        let url = parse("http://127.0.0.1:4100/app?token=abc&port=1&profile=web#frag");
        assert_eq!(
            clean_url(&url).as_str(),
            "http://127.0.0.1:4100/app?port=1&profile=web#frag"
        );
    }

    #[test]
    fn clean_url_keeps_parameter_order_and_encoding() {
        let url = parse("http://127.0.0.1:4100/?b=2&token=abc&a=1%20x");
        assert_eq!(
            clean_url(&url).as_str(),
            "http://127.0.0.1:4100/?b=2&a=1%20x"
        );
    }

    #[test]
    fn clean_url_leaves_token_free_urls_untouched() {
        let url = parse("http://127.0.0.1:4100/app?port=1#frag");
        assert_eq!(clean_url(&url), url);
        assert!(is_token_free(&url));
    }

    #[test]
    fn is_token_free_is_false_when_a_token_parameter_exists() {
        assert!(!is_token_free(&parse("http://127.0.0.1:4100/?token=abc")));
        assert!(!is_token_free(&parse(
            "http://127.0.0.1:4100/?a=1&Token=abc"
        )));
        assert!(is_token_free(&parse("http://127.0.0.1:4100/?a=1")));
        assert!(is_token_free(&parse("http://127.0.0.1:4100/")));
    }

    #[test]
    fn redact_url_replaces_question_mark_and_ampersand_values() {
        assert_eq!(
            redact_url("http://127.0.0.1:4100/?token=supersecret"),
            "http://127.0.0.1:4100/?token=<redacted>"
        );
        assert_eq!(
            redact_url("http://127.0.0.1:4100/?a=1&token=supersecret&b=2"),
            "http://127.0.0.1:4100/?a=1&token=<redacted>&b=2"
        );
        assert_eq!(
            redact_url("http://127.0.0.1:4100/?token=supersecret#frag"),
            "http://127.0.0.1:4100/?token=<redacted>#frag"
        );
    }

    #[test]
    fn redact_url_handles_multiple_tokens() {
        assert_eq!(
            redact_url("http://127.0.0.1:4100/?token=one&x=1&token=two"),
            "http://127.0.0.1:4100/?token=<redacted>&x=1&token=<redacted>"
        );
    }

    #[test]
    fn redact_url_leaves_other_parameters_alone() {
        let raw = "http://127.0.0.1:4100/app?port=4100&profile=web";
        assert_eq!(redact_url(raw), raw);
        assert_eq!(redact_url("no parameters here"), "no parameters here");
    }

    #[test]
    fn redact_url_does_not_panic_on_invalid_input() {
        assert_eq!(redact_url("?token="), "?token=<redacted>");
        assert_eq!(
            redact_url("not a url ?token=abc"),
            "not a url ?token=<redacted>"
        );
        assert_eq!(redact_url("token=abc"), "token=<redacted>");
        assert_eq!(redact_url(""), "");
        assert_eq!(redact_url("?token"), "?token");
    }

    #[test]
    fn redact_url_preserves_key_spelling() {
        assert_eq!(redact_url("?TOKEN=abc"), "?TOKEN=<redacted>");
    }

    #[test]
    fn redact_text_redacts_any_embedded_assignment() {
        let line = "spawned dsh web at http://127.0.0.1:4100/?token=supersecret (pid 42)";
        let redacted = redact_text(line);
        assert!(!redacted.contains("supersecret"));
        assert_eq!(
            redacted,
            "spawned dsh web at http://127.0.0.1:4100/?token=<redacted> (pid 42)"
        );
    }

    #[test]
    fn redact_text_handles_several_occurrences_and_delimiters() {
        let text = "first token=one, then token=two&then=three";
        assert_eq!(
            redact_text(text),
            "first token=<redacted>, then token=<redacted>&then=three"
        );
    }

    #[test]
    fn redact_text_is_idempotent() {
        let once = redact_text("?token=supersecret&x=1");
        assert_eq!(once, "?token=<redacted>&x=1");
        assert_eq!(redact_text(&once), once);
    }

    #[test]
    fn redact_text_leaves_unrelated_text_alone() {
        let text = "profile=web tokens=2 tokenizer ready";
        assert_eq!(redact_text(text), text);
    }

    #[test]
    fn is_loopback_http_accepts_loopback_addresses() {
        assert!(is_loopback_http(&parse("http://127.0.0.1/")));
        assert!(is_loopback_http(&parse(
            "http://127.0.0.1:4100/app?token=abc"
        )));
        assert!(is_loopback_http(&parse("https://127.0.0.1:8443/")));
        assert!(is_loopback_http(&parse("http://[::1]:4100/")));
    }

    #[test]
    fn is_loopback_http_rejects_localhost_and_remote_hosts() {
        assert!(!is_loopback_http(&parse("http://localhost:4100/")));
        assert!(!is_loopback_http(&parse("http://127.0.0.2:4100/")));
        assert!(!is_loopback_http(&parse("http://10.0.0.5:4100/")));
        assert!(!is_loopback_http(&parse("https://example.com/")));
        assert!(!is_loopback_http(&parse("http://[::2]:4100/")));
    }

    #[test]
    fn is_loopback_http_rejects_other_schemes() {
        assert!(!is_loopback_http(&parse("ws://127.0.0.1:4100/")));
        assert!(!is_loopback_http(&parse("file:///tmp/index.html")));
    }

    #[test]
    fn shares_target_matches_scheme_host_and_port() {
        assert!(shares_target(
            &parse("http://127.0.0.1:4100/?token=abc"),
            &parse("http://127.0.0.1:4100/settings")
        ));
        assert!(!shares_target(
            &parse("http://127.0.0.1:4100/"),
            &parse("http://127.0.0.1:4101/")
        ));
    }

    #[test]
    fn shares_target_uses_default_ports() {
        assert!(shares_target(
            &parse("http://127.0.0.1/app"),
            &parse("http://127.0.0.1:80/")
        ));
        assert!(shares_target(
            &parse("https://127.0.0.1/app"),
            &parse("https://127.0.0.1:443/")
        ));
        assert!(!shares_target(
            &parse("http://127.0.0.1/app"),
            &parse("http://127.0.0.1:443/")
        ));
    }

    #[test]
    fn shares_target_separates_schemes_and_hosts() {
        assert!(!shares_target(
            &parse("http://127.0.0.1/"),
            &parse("https://127.0.0.1/")
        ));
        assert!(!shares_target(
            &parse("http://127.0.0.1/"),
            &parse("http://localhost/")
        ));
        assert!(!shares_target(
            &parse("http://127.0.0.1/"),
            &parse("http://[::1]/")
        ));
    }
}
