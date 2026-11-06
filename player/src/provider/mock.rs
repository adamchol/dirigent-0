use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;

use super::{
    ContentBlock, Delta, DeltaStream, Provider, ProviderError, Request, StopReason, Usage,
};

#[derive(Debug)]
#[cfg_attr(not(test), allow(dead_code))]
pub enum Script {
    Deltas {
        delay: Duration,
        deltas: Vec<Delta>,
    },
    Fail {
        delay: Duration,
        error: ProviderError,
    },
    /// Like `Deltas`, with `gap` between deltas (for watching a reply stream).
    Paced {
        delay: Duration,
        gap: Duration,
        deltas: Vec<Delta>,
    },
}

#[cfg_attr(not(test), allow(dead_code))] // scripted replies are test-only
impl Script {
    pub fn text(text: &str, delay: Duration) -> Self {
        Script::Deltas {
            delay,
            deltas: vec![
                Delta::BlockStart {
                    index: 0,
                    block: ContentBlock::Text {
                        text: String::new(),
                    },
                },
                Delta::Text {
                    index: 0,
                    text: text.to_string(),
                },
                Delta::BlockStop { index: 0 },
                Delta::Usage(Usage {
                    input: 10,
                    output: 5,
                    cache_read: 0,
                    cache_write: 0,
                }),
                Delta::Stop(StopReason::EndTurn),
            ],
        }
    }
}

#[derive(Default)]
pub struct MockProvider {
    scripts: Mutex<VecDeque<Script>>,
    requests: Mutex<Vec<Request>>,
    calls: Mutex<u64>,
}

#[cfg_attr(not(test), allow(dead_code))] // scripting/inspection are test-only
impl MockProvider {
    /// Replace the remaining scripts.
    pub fn reset(&self, scripts: impl IntoIterator<Item = Script>) {
        *self.scripts.lock().unwrap() = scripts.into_iter().collect();
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn stream(&self, req: &Request) -> Result<DeltaStream, ProviderError> {
        self.requests.lock().unwrap().push(req.clone());
        let n = {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            *c
        };
        let script = self.scripts.lock().unwrap().pop_front();
        let script = script.unwrap_or_else(|| echo(req, n));
        match script {
            Script::Deltas { delay, deltas } => {
                tokio::time::sleep(delay).await;
                Ok(futures::stream::iter(deltas.into_iter().map(Ok)).boxed())
            }
            Script::Paced { delay, gap, deltas } => {
                tokio::time::sleep(delay).await;
                Ok(futures::stream::iter(deltas)
                    .then(move |d| async move {
                        tokio::time::sleep(gap).await;
                        Ok(d)
                    })
                    .boxed())
            }
            Script::Fail { delay, error } => {
                tokio::time::sleep(delay).await;
                Err(error)
            }
        }
    }
}

/// Fallback when no script is queued (`--mock` demos): echo the latest user
/// text after a short delay, run `Bash` for text starting with `!`, or call
/// any tool for `!!<tool> <json input>`.
fn echo(req: &Request, n: u64) -> Script {
    let delay = Duration::from_millis(300);
    let Some(last) = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == super::Role::User)
    else {
        return streamed(&format!("mock reply #{n}"), delay);
    };
    // The newest text block is the latest prompt (an aborted turn can leave
    // tool results and a marker in the same user message).
    let text = last
        .content
        .iter()
        .rev()
        .find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or_default();
    let call = if let Some(call) = text.trim().strip_prefix("!!") {
        let (name, input) = call.split_once(char::is_whitespace).unwrap_or((call, "{}"));
        let input = serde_json::from_str(input.trim())
            .unwrap_or_else(|e| serde_json::json!({ "__error": e.to_string() }));
        Some((name.to_string(), input))
    } else {
        text.trim().strip_prefix('!').map(|command| {
            (
                "Bash".to_string(),
                serde_json::json!({ "command": command.trim() }),
            )
        })
    };
    if let Some((name, input)) = call {
        return Script::Deltas {
            delay,
            deltas: vec![
                Delta::BlockStart {
                    index: 0,
                    block: ContentBlock::ToolCall {
                        id: format!("mock-{n}"),
                        name,
                        input,
                    },
                },
                Delta::BlockStop { index: 0 },
                Delta::Usage(Usage {
                    input: 10,
                    output: 5,
                    cache_read: 0,
                    cache_write: 0,
                }),
                Delta::Stop(StopReason::ToolUse),
            ],
        };
    }
    let after_tools = last
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolResult { .. }));
    if after_tools {
        let reply = if text.is_empty() {
            format!("mock reply #{n}: ran it")
        } else {
            format!("mock reply #{n}: ran it, and you said: {text}")
        };
        return streamed(&reply, delay);
    }
    streamed(&format!("mock reply #{n} to: {text}"), delay)
}

/// A text reply streamed word by word.
fn streamed(text: &str, delay: Duration) -> Script {
    let mut deltas = vec![Delta::BlockStart {
        index: 0,
        block: ContentBlock::Text {
            text: String::new(),
        },
    }];
    deltas.extend(text.split_inclusive(' ').map(|word| Delta::Text {
        index: 0,
        text: word.to_string(),
    }));
    deltas.push(Delta::BlockStop { index: 0 });
    deltas.push(Delta::Usage(Usage {
        input: 10,
        output: 5,
        cache_read: 0,
        cache_write: 0,
    }));
    deltas.push(Delta::Stop(StopReason::EndTurn));
    Script::Paced {
        delay,
        gap: Duration::from_millis(50),
        deltas,
    }
}
