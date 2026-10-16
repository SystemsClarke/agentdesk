//! Desktop toasts, one per new question (and per follow-up/"Done" from the agent that asked). Never fails the caller.
//! A toast that belongs to a thread opens that thread in a terminal when it is clicked (Windows and Linux; macOS toasts carry no click).

/// Parses a thread id as typed on a command line or in a link: `12`, `#12`, ` 12 `. Zero and negatives are not thread ids.
pub fn parse_thread_id(s: &str) -> Result<i64, String> {
    let t = s.trim();
    let t = t.strip_prefix('#').unwrap_or(t);
    match t.parse::<i64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!("`{s}` is not a thread id (use a number such as 12 or #12)")),
    }
}

/// The arguments a clicked toast hands the terminal: `agentdesk open <id>`.
pub fn open_args(thread: i64) -> Vec<String> {
    vec!["open".to_string(), thread.to_string()]
}

/// Shows a toast. With `thread`, clicking it opens that thread; the wait for the click runs on its own thread, so this returns at once.
pub fn toast(title: &str, body: &str, thread: Option<i64>) {
    // AGENTDESK_TOAST_LOG: append each toast to this file instead of showing one (the parity suite reads it)
    if let Some(path) = std::env::var_os("AGENTDESK_TOAST_LOG") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{title}\t{body}");
        }
        return;
    }
    if std::env::var_os("AGENTDESK_NO_TOAST").is_some() || std::env::var_os("AGENTDESK_DATA").is_some() {
        return; // temp and test boards stay quiet
    }
    let body: String = body.chars().take(240).collect();
    let mut n = notify_rust::Notification::new();
    n.appname("AgentDesk").summary(title).body(&body);
    #[cfg(all(unix, not(target_os = "macos")))]
    if thread.is_some() {
        n.action("default", "Open"); // the body click on most desktops, and a button on the rest
    }
    let Ok(handle) = n.show() else { return };
    #[cfg(any(windows, all(unix, not(target_os = "macos"))))]
    if let Some(id) = thread {
        // blocks until the toast is clicked or goes away, so never on the caller's thread (the core loop)
        let _ = std::thread::Builder::new().name("toast-click".into()).spawn(move || wait_for_click(handle, id));
    }
    #[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
    let _ = (handle, thread);
}

#[cfg(windows)]
fn wait_for_click(handle: notify_rust::NotificationHandle, id: i64) {
    let _ = handle.wait_for_response(|r: &notify_rust::NotificationResponse| {
        if matches!(r, notify_rust::NotificationResponse::Default | notify_rust::NotificationResponse::Action(_)) {
            let _ = crate::tray::open_terminal(&open_args(id));
        }
    });
}

#[cfg(all(unix, not(target_os = "macos")))]
fn wait_for_click(handle: notify_rust::NotificationHandle, id: i64) {
    handle.wait_for_action(|action| {
        if action != "__closed" {
            let _ = crate::tray::open_terminal(&open_args(id));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_ids_parse_with_or_without_a_hash() {
        assert_eq!(parse_thread_id("12"), Ok(12));
        assert_eq!(parse_thread_id("#12"), Ok(12));
        assert_eq!(parse_thread_id(" #7 "), Ok(7));
        for bad in ["", "#", "0", "-3", "abc", "1.5", "12x", "##4"] {
            assert!(parse_thread_id(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_clicked_toast_runs_open_with_the_thread() {
        assert_eq!(open_args(42), ["open", "42"]);
    }
}
