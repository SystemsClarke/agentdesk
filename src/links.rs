//! Link hygiene for packets (docs/question-types.md, "Links"). Pure string work, no network: unwrap known wrappers, normalise,
//! de-duplicate, label with the domain, flag domains John has not seen, and cap the list. Resolving short links and opening pages in a
//! headless browser are separate steps that need the network and are not done here.

use serde_json::{json, Value};

/// A packet shows at most this many links.
pub const MAX_LINKS: usize = 12;

/// Domains John uses every day. Anything else is flagged "unknown" so a new link is marked, not trusted.
const KNOWN: [&str; 14] = [
    "github.com",
    "dev.azure.com",
    "visualstudio.com",
    "vispero.com",
    "microsoft.com",
    "office.com",
    "outlook.com",
    "sharepoint.com",
    "teams.microsoft.com",
    "slack.com",
    "claude.ai",
    "anthropic.com",
    "crowdin.com",
    "atlassian.net",
];

/// Wrappers whose real address sits in a query parameter, with that parameter's name.
const WRAPPERS: [(&str, &str); 3] = [("safelinks.protection.outlook.com", "url"), ("urldefense.com", "u"), ("l.facebook.com", "u")];

/// Parameters that only track where a click came from.
fn is_tracking(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("utm_") || ["fbclid", "gclid", "mc_cid", "mc_eid", "msclkid", "ocid", "ref_src", "_hsenc", "_hsmi"].contains(&n.as_str())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `(host, port, rest)` where `rest` begins at the path, or is empty. Only http and https links are accepted, and any `user:password@`
/// in front of the host is dropped, so a credential pasted into a link is never shown.
fn split(url: &str) -> Option<(String, Option<u16>, String)> {
    let u = url.trim();
    let lower = u.to_ascii_lowercase();
    let after = if lower.starts_with("https://") {
        &u[8..]
    } else if lower.starts_with("http://") {
        &u[7..]
    } else {
        return None;
    };
    let end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let authority = &after[..end];
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = match host_port.split_once(':') {
        Some((h, p)) => (h.to_ascii_lowercase(), Some(p.parse::<u16>().ok()?)),
        None => (host_port.to_ascii_lowercase(), None),
    };
    if host.is_empty() || !host.contains('.') || host.contains(' ') {
        return None;
    }
    Some((host, port, after[end..].to_string()))
}

/// The address inside a known wrapper (Outlook Safe Links and the like), if `url` is one. Applied until it stops changing.
pub fn unwrap(url: &str) -> String {
    let mut cur = url.trim().to_string();
    for _ in 0..4 {
        let Some((host, _, rest)) = split(&cur) else { return cur };
        let Some((_, param)) = WRAPPERS.iter().find(|(h, _)| host == *h || host.ends_with(&format!(".{h}"))) else { return cur };
        let query = rest.split_once('?').map(|(_, q)| q.split('#').next().unwrap_or("")).unwrap_or("");
        let inner = query.split('&').find_map(|kv| kv.split_once('=').filter(|(k, _)| k == param).map(|(_, v)| percent_decode(v)));
        match inner {
            Some(i) if split(&i).is_some() => cur = i,
            _ => return cur,
        }
    }
    cur
}

/// Strip tracking parameters and the fragment, lower-case the host, drop the default port and a trailing slash.
pub fn normalise(url: &str) -> Option<String> {
    let u = unwrap(url);
    let (host, port, rest) = split(&u)?;
    let https = u.trim().to_ascii_lowercase().starts_with("https://");
    // The default port for the scheme is dropped; any other is part of the address.
    let port = port.filter(|p| *p != if https { 443 } else { 80 }).map(|p| format!(":{p}")).unwrap_or_default();
    let rest = rest.split('#').next().unwrap_or("");
    let (path, query) = rest.split_once('?').map(|(p, q)| (p, Some(q))).unwrap_or((rest, None));
    let kept: Vec<&str> = query.map(|q| q.split('&').filter(|kv| !kv.is_empty() && !is_tracking(kv.split('=').next().unwrap_or(""))).collect()).unwrap_or_default();
    let path = if path == "/" { "" } else { path.strip_suffix('/').unwrap_or(path) };
    let mut out = format!("{}://{host}{port}{path}", if https { "https" } else { "http" });
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    Some(out)
}

/// The registrable-looking part of a host, for labelling: `dev.azure.com`, not `www.dev.azure.com`.
fn label_of(host: &str) -> String {
    host.strip_prefix("www.").unwrap_or(host).to_string()
}

fn is_known(host: &str) -> bool {
    KNOWN.iter().any(|k| host == *k || host.ends_with(&format!(".{k}")))
}

/// Cleans a packet's `found` links. Each input is a string, or an object with `url` and optional `label` / `note`. Returns the cleaned
/// list (at most `MAX_LINKS`), one entry per final address with `cited` (how many inputs pointed at it), plus what was dropped.
pub fn clean(found: &[Value]) -> Value {
    let mut out: Vec<Value> = Vec::new();
    let mut rejected: Vec<String> = Vec::new();
    for item in found {
        let (raw, label, note) = match item {
            Value::String(s) => (s.clone(), None, None),
            Value::Object(o) => (
                o.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                o.get("label").and_then(|v| v.as_str()).map(String::from),
                o.get("note").and_then(|v| v.as_str()).map(String::from),
            ),
            _ => continue,
        };
        let Some(url) = normalise(&raw) else {
            rejected.push(raw);
            continue;
        };
        if let Some(existing) = out.iter_mut().find(|e| e["url"] == url.as_str()) {
            existing["cited"] = json!(existing["cited"].as_i64().unwrap_or(1) + 1);
            if existing["label"].is_null() {
                existing["label"] = json!(label);
            }
            if existing["note"].is_null() {
                existing["note"] = json!(note);
            }
            continue;
        }
        let host = split(&url).map(|(h, _, _)| label_of(&h)).unwrap_or_default();
        out.push(json!({"url": url, "domain": host, "unknown_domain": !is_known(&host), "label": label, "note": note, "cited": 1}));
    }
    let over = out.len().saturating_sub(MAX_LINKS);
    out.truncate(MAX_LINKS);
    json!({"links": out, "rejected": rejected, "dropped_over_cap": over})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outlook_safe_links_are_unwrapped_without_a_request() {
        let wrapped = "https://nam11.safelinks.protection.outlook.com/?url=https%3A%2F%2Fdev.azure.com%2Fvispero%2Fx%2F_workitems%2Fedit%2F210148%3Futm_source%3Dmail&data=05%7C02&reserved=0";
        assert_eq!(normalise(wrapped).unwrap(), "https://dev.azure.com/vispero/x/_workitems/edit/210148");
    }

    #[test]
    fn nested_wrappers_unwrap_all_the_way() {
        let inner = "https://github.com/o/r/pull/7";
        let once = format!("https://urldefense.com/v3/__x__?u={}", inner.replace(':', "%3A").replace('/', "%2F"));
        let twice = format!("https://nam11.safelinks.protection.outlook.com/?url={}", once.replace(':', "%3A").replace('/', "%2F").replace('?', "%3F").replace('=', "%3D"));
        assert_eq!(unwrap(&twice), inner);
    }

    #[test]
    fn normalising_strips_tracking_fragments_ports_and_case() {
        assert_eq!(normalise("HTTPS://GitHub.com:443/o/r/pull/7/?utm_source=x&tab=files#diff").unwrap(), "https://github.com/o/r/pull/7?tab=files");
        assert_eq!(normalise("http://build.example.com:80/x").unwrap(), "http://build.example.com/x");
        assert_eq!(normalise("https://example.com/").unwrap(), "https://example.com");
        assert_eq!(normalise("https://example.com/a?fbclid=1&gclid=2").unwrap(), "https://example.com/a");
    }

    #[test]
    fn a_port_that_is_not_the_default_stays_and_a_pasted_credential_goes() {
        assert_eq!(normalise("https://gocd.example.com:8154/go/pipelines").unwrap(), "https://gocd.example.com:8154/go/pipelines");
        assert_eq!(normalise("https://john:hunter2@gocd.example.com/go").unwrap(), "https://gocd.example.com/go");
        assert!(normalise("https://gocd.example.com:notaport/go").is_none());
    }

    #[test]
    fn decoding_never_panics_on_odd_input() {
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("%é9"), "%é9");
        assert_eq!(percent_decode("a%20b+c"), "a b c");
        assert_eq!(unwrap("https://nam11.safelinks.protection.outlook.com/?url=%E9%80&x=1"), "https://nam11.safelinks.protection.outlook.com/?url=%E9%80&x=1");
    }

    #[test]
    fn only_web_links_are_accepted() {
        for bad in ["javascript:alert(1)", "file:///c:/x", "mailto:a@b.c", "ftp://x.y/z", "not a link", "https://", "https://localhost/x", ""] {
            assert!(normalise(bad).is_none(), "{bad} should be refused");
        }
    }

    #[test]
    fn the_same_page_is_listed_once_and_counted() {
        let r = clean(&[
            json!("https://github.com/o/r/pull/7?utm_source=a"),
            json!({"url": "https://GITHUB.com/o/r/pull/7/#files", "label": "the PR"}),
            json!("https://dev.azure.com/vispero/p/_workitems/edit/1"),
        ]);
        let links = r["links"].as_array().unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0]["cited"], 2);
        assert_eq!(links[0]["label"], "the PR", "a later label fills a missing one");
        assert_eq!(links[0]["unknown_domain"], false);
    }

    #[test]
    fn a_domain_john_has_not_seen_is_flagged() {
        let r = clean(&[json!("https://some-new-site.example/page")]);
        assert_eq!(r["links"][0]["unknown_domain"], true);
        assert_eq!(r["links"][0]["domain"], "some-new-site.example");
    }

    #[test]
    fn the_list_is_capped_and_bad_links_are_reported_not_hidden() {
        let many: Vec<Value> = (0..20).map(|i| json!(format!("https://github.com/o/r/issues/{i}"))).collect();
        let mut input = many;
        input.push(json!("javascript:alert(1)"));
        let r = clean(&input);
        assert_eq!(r["links"].as_array().unwrap().len(), MAX_LINKS);
        assert_eq!(r["dropped_over_cap"], 8);
        assert_eq!(r["rejected"][0], "javascript:alert(1)");
    }
}
