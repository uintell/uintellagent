// Tool Confirmation Hook — intercepts tool calls before execution
//
// Modes:
//   TUI interactive — shows confirmation dialog (oneshot channel → TUI)
//   CLI interactive — asks [y/N] on stdin (200ms TUI timeout, then stdin fallback)
//   auto_approve    — allows only calls that do not require confirmation

use rig_core::agent::hook::{AgentHook, Flow, HookContext, StepEvent};
use rig_core::completion::CompletionModel;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::oneshot;

pub struct ConfirmRequest {
    pub tool_name: String,
    pub args: String,
    pub reason: Option<String>,
    pub response_tx: oneshot::Sender<bool>,
}

pub struct ConfirmState {
    pub pending: std::sync::Mutex<Option<ConfirmRequest>>,
    cli_mode: bool,
    approve_all: AtomicBool,
}

impl ConfirmState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pending: std::sync::Mutex::new(None),
            cli_mode: false,
            approve_all: AtomicBool::new(false),
        })
    }

    pub(crate) fn cli() -> Self {
        Self {
            pending: std::sync::Mutex::new(None),
            cli_mode: true,
            approve_all: AtomicBool::new(false),
        }
    }

    /// Ask for confirmation. TUI mode: waits for oneshot. CLI mode: stdin fallback.
    pub async fn request(&self, tool_name: &str, args: &str, reason: Option<&str>) -> bool {
        if self.approve_all.load(Ordering::Relaxed) {
            return true;
        }
        if self.cli_mode {
            return self.ask_stdin(tool_name, args, reason);
        }

        let (tx, rx) = oneshot::channel();
        {
            let mut p = self.pending.lock().unwrap();
            *p = Some(ConfirmRequest {
                tool_name: tool_name.to_string(),
                args: args.to_string(),
                reason: reason.map(|s| s.to_string()),
                response_tx: tx,
            });
        }
        matches!(rx.await, Ok(true))
    }

    fn ask_stdin(&self, tool_name: &str, args: &str, reason: Option<&str>) -> bool {
        eprintln!();
        eprintln!("⚠  Tool confirmation: {tool_name}");
        if let Some(r) = reason {
            eprintln!("   {r}");
        }
        let mut chars = args.chars();
        let prefix: String = chars.by_ref().take(80).collect();
        let preview = if chars.next().is_some() {
            format!("{prefix}...")
        } else {
            prefix
        };
        eprintln!("   Args: {preview}");
        eprint!("   Approve? [y/N]: ");
        use std::io::Write;
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        match std::io::stdin().read_line(&mut input) {
            Ok(_) => matches!(input.trim().to_lowercase().as_str(), "y" | "yes"),
            Err(_) => false,
        }
    }

    pub fn take_pending(&self) -> Option<ConfirmRequest> {
        self.pending.lock().unwrap().take()
    }

    pub fn cancel_pending(&self) {
        if let Some(request) = self.pending.lock().unwrap().take() {
            let _ = request.response_tx.send(false);
        }
    }

    pub fn respond(&self, approved: bool, tx: oneshot::Sender<bool>) {
        let _ = tx.send(approved);
    }

    pub fn approve_all(&self) {
        self.approve_all.store(true, Ordering::Relaxed);
    }
}

pub struct ConfirmHook {
    state: Option<Arc<ConfirmState>>,
}

impl ConfirmHook {
    pub fn interactive(state: Arc<ConfirmState>) -> Self {
        Self::with_state(Some(state))
    }

    pub fn auto_approve() -> Self {
        Self::with_state(None)
    }

    pub fn cli_interactive() -> Self {
        Self::with_state(Some(Arc::new(ConfirmState::cli())))
    }

    fn with_state(state: Option<Arc<ConfirmState>>) -> Self {
        Self { state }
    }
}

impl<M: CompletionModel> AgentHook<M> for ConfirmHook {
    fn on_event(
        &self,
        _ctx: &HookContext,
        event: StepEvent<'_, M>,
    ) -> impl std::future::Future<Output = Flow> + rig_core::wasm_compat::WasmCompatSend {
        let state = self.state.clone();

        async move {
            match event {
                StepEvent::ToolCall {
                    tool_name, args, ..
                } => match crate::permissions::permission_for_tool(tool_name, args) {
                    crate::permissions::PermissionResult::Allowed => Flow::Continue,
                    crate::permissions::PermissionResult::Denied(reason) => Flow::Skip { reason },
                    crate::permissions::PermissionResult::Confirm(reason) => match &state {
                        Some(s) => {
                            if s.request(tool_name, args, Some(&reason)).await {
                                crate::permissions::record_approval(tool_name, args);
                                Flow::Continue
                            } else {
                                Flow::Skip {
                                    reason: "User denied".into(),
                                }
                            }
                        }
                        None => Flow::Skip { reason },
                    },
                },
                _ => Flow::Continue,
            }
        }
    }

    fn observes(&self, kind: rig_core::agent::hook::StepEventKind) -> bool {
        matches!(kind, rig_core::agent::hook::StepEventKind::ToolCall)
    }
}

#[cfg(test)]
mod tests {
    use super::ConfirmState;

    #[tokio::test]
    async fn cancelling_a_pending_confirmation_releases_the_agent_run() {
        let state = ConfirmState::new();
        let requester = state.clone();
        let task = tokio::spawn(async move {
            requester
                .request("terminal", r#"{"command":"rm test"}"#, Some("test"))
                .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if state.pending.lock().unwrap().is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("confirmation should become pending");
        state.cancel_pending();

        assert!(!task.await.unwrap());
        assert!(state.pending.lock().unwrap().is_none());
    }
}
