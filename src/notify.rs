//! Desktop toasts, one per new question (and per follow-up/"Done" from the agent that asked). Never fails the caller.

pub fn toast(title: &str, body: &str) {
    if std::env::var_os("AGENTDESK_NO_TOAST").is_some() || std::env::var_os("AGENTDESK_DATA").is_some() {
        return; // temp and test boards stay quiet
    }
    let body: String = body.chars().take(240).collect();
    let _ = notify_rust::Notification::new().appname("AgentDesk").summary(title).body(&body).show();
}
