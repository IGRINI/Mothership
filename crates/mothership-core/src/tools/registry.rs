use std::collections::HashMap;
use std::sync::Mutex;

use super::ToolCancellationToken;

#[derive(Debug, Default)]
pub struct ToolExecutionRegistry {
    active: Mutex<HashMap<String, ToolCancellationToken>>,
}

impl ToolExecutionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, tool_call_id: &str, token: ToolCancellationToken) -> bool {
        let mut active = self.active.lock().unwrap();
        if active.contains_key(tool_call_id) {
            return false;
        }
        active.insert(tool_call_id.to_string(), token);
        true
    }

    pub fn cancel(&self, tool_call_id: &str) -> bool {
        let active = self.active.lock().unwrap();
        let Some(token) = active.get(tool_call_id) else {
            return false;
        };
        token.cancel();
        true
    }

    pub fn finish(&self, tool_call_id: &str) {
        self.active.lock().unwrap().remove(tool_call_id);
    }
}
