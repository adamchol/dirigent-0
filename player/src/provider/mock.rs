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
    /// Title calls are answered apart, so the turns' scripts and requests are only theirs:
    /// by these scripts, then by naming the session after its text's first words.
    title_scripts: Mutex<VecDeque<Script>>,
    titles: Mutex<Vec<Request>>,
    /// Text replies go on after their first line with paragraphs of random
    /// length (`--mock`: something to watch stream); not in tests.
    long: bool,
}

impl MockProvider {
    /// The player's `--mock`: long replies.
    pub fn long() -> Self {
        MockProvider {
            long: true,
            ..Default::default()
        }
    }
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

    /// Replace the remaining title calls' scripts.
    pub fn reset_titles(&self, scripts: impl IntoIterator<Item = Script>) {
        *self.title_scripts.lock().unwrap() = scripts.into_iter().collect();
    }

    pub fn titles(&self) -> Vec<Request> {
        self.titles.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn stream(&self, req: &Request) -> Result<DeltaStream, ProviderError> {
        if req.system.as_deref() == Some(crate::title::INSTRUCTION) {
            self.titles.lock().unwrap().push(req.clone());
            let script = self.title_scripts.lock().unwrap().pop_front();
            return play(script.unwrap_or_else(|| name(req))).await;
        }
        self.requests.lock().unwrap().push(req.clone());
        let n = {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            *c
        };
        let script = self.scripts.lock().unwrap().pop_front();
        play(script.unwrap_or_else(|| echo(req, n, self.long))).await
    }
}

/// A script played out: its deltas, or its failure, after its delay.
async fn play(script: Script) -> Result<DeltaStream, ProviderError> {
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

/// A title call's answer: the session's first six words, and its text clipped.
fn name(req: &Request) -> Script {
    let text = req.messages.last().map(|m| m.text()).unwrap_or_default();
    let session = text
        .split_once("<session>")
        .map_or(text.as_str(), |(_, s)| s)
        .trim_end_matches("</session>")
        .trim();
    let words: Vec<&str> = session.split_whitespace().collect();
    let clipped: String = words.join(" ").chars().take(60).collect();
    let answer = serde_json::json!({
        "title": words.iter().take(6).copied().collect::<Vec<_>>().join(" "),
        "description": format!("mock: {clipped}"),
    });
    Script::text(&answer.to_string(), Duration::ZERO)
}

/// Fallback when no script is queued (`--mock` demos): echo the latest user
/// text after a short delay, run `Bash` for text starting with `!`, call any
/// tool for `!!<tool> <json input>`, or stream a longer reply for `?demo`.
fn echo(req: &Request, n: u64, long: bool) -> Script {
    let delay = Duration::from_millis(300);
    let Some(last) = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == super::Role::User)
    else {
        return streamed(&format!("mock reply #{n}"), delay, long);
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
    if text.trim() == "?demo" {
        return demo(delay);
    }
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
        return streamed(&reply, delay, long);
    }
    streamed(&format!("mock reply #{n} to: {text}"), delay, long)
}

/// A text reply streamed word by word.
fn streamed(text: &str, delay: Duration, long: bool) -> Script {
    let mut rng = Rng::seeded();
    let text = if long {
        format!("{text}\n\n{}", lorem(&mut rng))
    } else {
        text.to_string()
    };
    let mut deltas = vec![Delta::BlockStart {
        index: 0,
        block: ContentBlock::Text {
            text: String::new(),
        },
    }];
    // long: a few words at a time, as a model's tokens come
    let words: Vec<&str> = text.split_inclusive([' ', '\n']).collect();
    let mut i = 0;
    while i < words.len() {
        let take = if long { 1 + rng.below(3) } else { 1 };
        let piece = words[i..(i + take).min(words.len())].concat();
        deltas.push(Delta::Text {
            index: 0,
            text: piece,
        });
        i += take;
    }
    deltas.push(Delta::BlockStop { index: 0 });
    deltas.push(Delta::Usage(Usage {
        input: 10,
        // long: about as many tokens as words
        output: if long { words.len() as u64 } else { 5 },
        cache_read: 0,
        cache_write: 0,
    }));
    deltas.push(Delta::Stop(StopReason::EndTurn));
    Script::Paced {
        delay,
        gap: Duration::from_millis(if long { 35 } else { 50 }),
        deltas,
    }
}

/// `?demo`: a reply worth watching stream, thinking and then Markdown with
/// code, a few characters at a time.
fn demo(delay: Duration) -> Script {
    const THINKING: &str = "The user wants to see a reply stream. I'll think for a moment, then \
        answer with a list and some code, so every part of the timeline shows.";
    const TEXT: &str = "Here's what a streamed reply looks like:\n\n\
        - **Thinking** shows first, its latest lines fading in above\n\
        - then the text, as *Markdown*, piece by piece\n\
        - and code, highlighted as it comes:\n\n\
        ```rust\nfn main() {\n    for n in 1..=3 {\n        println!(\"turn {n}\");\n    }\n}\n```\n\n\
        Once the reply is whole, it takes the streamed one's place.";
    let pieces = |text: &'static str| {
        text.split_inclusive([' ', '\n'])
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let mut deltas = vec![Delta::BlockStart {
        index: 0,
        block: ContentBlock::Thinking {
            text: String::new(),
            provider_data: serde_json::Value::Null,
        },
    }];
    deltas.extend(
        pieces(THINKING)
            .into_iter()
            .map(|text| Delta::Thinking { index: 0, text }),
    );
    deltas.push(Delta::BlockStop { index: 0 });
    deltas.push(Delta::BlockStart {
        index: 1,
        block: ContentBlock::Text {
            text: String::new(),
        },
    });
    deltas.extend(
        pieces(TEXT)
            .into_iter()
            .map(|text| Delta::Text { index: 1, text }),
    );
    deltas.push(Delta::BlockStop { index: 1 });
    deltas.push(Delta::Usage(Usage {
        input: 40,
        output: 120,
        cache_read: 0,
        cache_write: 0,
    }));
    deltas.push(Delta::Stop(StopReason::EndTurn));
    Script::Paced {
        delay,
        gap: Duration::from_millis(60),
        deltas,
    }
}

/// A few paragraphs of Lorem ipsum, of random number and length, now and
/// then with a list: a long reply's body.
fn lorem(rng: &mut Rng) -> String {
    const WORDS: &[&str] = &[
        "lorem",
        "ipsum",
        "dolor",
        "sit",
        "amet",
        "consectetur",
        "adipiscing",
        "elit",
        "sed",
        "do",
        "eiusmod",
        "tempor",
        "incididunt",
        "ut",
        "labore",
        "et",
        "dolore",
        "magna",
        "aliqua",
        "enim",
        "ad",
        "minim",
        "veniam",
        "quis",
        "nostrud",
        "exercitation",
        "ullamco",
        "laboris",
        "nisi",
        "aliquip",
        "ex",
        "ea",
        "commodo",
        "consequat",
        "duis",
        "aute",
        "irure",
        "in",
        "reprehenderit",
        "voluptate",
        "velit",
        "esse",
        "cillum",
        "fugiat",
        "nulla",
        "pariatur",
        "excepteur",
        "sint",
        "occaecat",
        "cupidatat",
        "non",
        "proident",
        "sunt",
        "culpa",
        "qui",
        "officia",
        "deserunt",
        "mollit",
        "anim",
        "id",
        "est",
        "laborum",
    ];
    let sentence = |rng: &mut Rng, min: usize, max: usize| {
        let n = min + rng.below(max - min + 1);
        let words: Vec<&str> = (0..n).map(|_| WORDS[rng.below(WORDS.len())]).collect();
        let s = words.join(" ");
        let mut c = s.chars();
        c.next()
            .map(|f| f.to_uppercase().chain(c).collect::<String>())
            .unwrap_or_default()
    };
    let mut parts = Vec::new();
    for _ in 0..1 + rng.below(4) {
        let sentences: Vec<String> = (0..2 + rng.below(4))
            .map(|_| format!("{}.", sentence(rng, 6, 16)))
            .collect();
        parts.push(sentences.join(" "));
        if rng.below(4) == 0 {
            let items: Vec<String> = (0..2 + rng.below(3))
                .map(|_| format!("- {}", sentence(rng, 3, 8)))
                .collect();
            parts.push(items.join("\n"));
        }
    }
    parts.join("\n\n")
}

/// A small xorshift generator: the mock's randomness needs no quality.
struct Rng(u64);

impl Rng {
    fn seeded() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.as_nanos() as u64);
        Rng(nanos | 1)
    }

    /// A number in `0..n`.
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}
