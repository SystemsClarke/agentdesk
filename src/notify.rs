//! Desktop toasts, one per new question (and per follow-up/"Done" from the agent that asked). Never fails the caller.

pub fn toast(title: &str, body: &str) {
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
    let _ = notify_rust::Notification::new().appname("AgentDesk").summary(title).body(&body).show();
}
