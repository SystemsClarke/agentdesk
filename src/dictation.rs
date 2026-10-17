//! Dictation (Ctrl+D): local speech-to-text for the window's text boxes, run by the embedded Python plugin
//! (`python/agentdesk/dictate.py`: sounddevice and sherpa-onnx).

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};

/// Where a dictation is: the shape the window already reads (`state` is idle, downloading, loading, listening or done).
#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub state: String,
    pub text: String,
    pub error: Option<String>,
    pub progress: f64,
    /// The microphone in use, when known (start reports it).
    pub device: Option<String>,
}

impl Status {
    pub fn idle() -> Self {
        Status { state: "idle".into(), text: String::new(), error: None, progress: 0.0, device: None }
    }

    pub fn from_value(v: &Value) -> Self {
        Status {
            state: v["state"].as_str().unwrap_or("idle").to_string(),
            text: v["text"].as_str().unwrap_or("").to_string(),
            error: v["error"].as_str().map(String::from),
            progress: v["progress"].as_f64().unwrap_or(0.0),
            device: v["device"].as_str().map(String::from),
        }
    }

    pub fn to_value(&self) -> Value {
        let mut v = json!({"state": self.state, "text": self.text, "error": self.error, "progress": self.progress});
        if let Some(d) = &self.device {
            v["device"] = json!(d);
        }
        v
    }
}

/// Whether a dictation was ever started: until then poll and stop have nothing to talk to, so they answer idle without waking Python.
static STARTED: AtomicBool = AtomicBool::new(false);

/// The entry point for the window and the UI API: `start`, `poll`, `stop`, `arm` or `disarm`, answered in the plugin's JSON shape
/// (state, text, error, progress, and the device on start). An `Err` is a failure to even begin (no action, no plugin).
pub fn call(action: &str) -> Result<Value> {
    if !matches!(action, "start" | "poll" | "stop" | "arm" | "disarm") {
        return Err(anyhow!("action is start, poll, stop, arm or disarm"));
    }
    if matches!(action, "start" | "arm") {
        if crate::pyhost::interpreter().is_none() {
            return Err(anyhow!("Dictation needs Python 3 with the packages in python/requirements.txt (set AGENTDESK_PYTHON_BIN)."));
        }
        STARTED.store(true, Ordering::Relaxed);
    } else if !STARTED.load(Ordering::Relaxed) {
        return Ok(Status::idle().to_value()); // nothing was started: nothing to poll or stop
    }
    Ok(Status::from_value(&crate::pyhost::call(&format!("dictate.{action}"), json!({}))?).to_value())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trips_the_plugins_json() {
        let v = json!({"state": "listening", "text": "hello", "error": null, "progress": 0.5, "device": "USB Mic"});
        let s = Status::from_value(&v);
        assert_eq!((s.state.as_str(), s.text.as_str(), s.device.as_deref()), ("listening", "hello", Some("USB Mic")));
        assert_eq!(s.to_value(), v);
        assert!(Status::idle().to_value().get("device").is_none());
        assert_eq!(Status::from_value(&json!({})), Status::idle());
    }

    #[test]
    fn unknown_actions_are_refused_and_an_unstarted_poll_is_idle() {
        assert!(call("dance").is_err());
        assert_eq!(call("stop").unwrap()["state"], "idle");
    }
}
