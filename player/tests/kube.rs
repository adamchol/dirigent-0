//! The chart on the test cluster (`kube-test e2e`, which forwards control's and chlive's ports):
//! players in KubeVirt VMs, started by control. Skipped unless `DIRIGENT_KUBE_E2E` is set. Not
//! in the Nix builds' source (nix/packages/dirigent.nix), so editing them rebuilds no image.
//! Each test says its sessions (`session <id>`), which `kube-test e2e` collects the logs of.

use std::time::{Duration, Instant};

use chlive_client::{ClientOptions, LiveClient};
use dirigent_protocol::rows::{EventRow, EVENTS, LOG_QUERY};
use dirigent_protocol::{ContentBlock, Event};
use futures::StreamExt;

const USER: &str = "user:e2e";

struct Stack {
    chlive: String,
    /// Control's HTTP API.
    control: String,
}

fn stack() -> Option<Stack> {
    std::env::var_os("DIRIGENT_KUBE_E2E")?;
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name}"));
    Some(Stack {
        chlive: var("CHLIVE_URL"),
        control: var("DIRIGENT_CONTROL_HTTP"),
    })
}

/// A new session, [`USER`]'s, made with `body` (its spec); said, for the logs.
async fn put_session(stack: &Stack, body: serde_json::Value) -> String {
    let session = uuid::Uuid::new_v4().to_string();
    let status = api(stack, reqwest::Method::PUT, &format!("/sessions/{session}"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, reqwest::StatusCode::CREATED);
    eprintln!("session {session}");
    session
}

/// A request to control's API, as [`USER`].
fn api(stack: &Stack, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .request(method, format!("{}{path}", stack.control))
        .header("dirigent-user", USER)
}

/// A JSON answer of control's.
async fn get(stack: &Stack, path: &str) -> serde_json::Value {
    let res = api(stack, reqwest::Method::GET, path).send().await.unwrap();
    assert!(res.status().is_success(), "{path}: {}", res.status());
    res.json().await.unwrap()
}

/// chlive, as [`USER`], with the credentials control signs for them (`declared` auth).
fn client(stack: &Stack) -> LiveClient {
    let control = stack.control.clone();
    let token = move || {
        let control = control.clone();
        async move {
            let got: serde_json::Value = reqwest::Client::new()
                .post(format!("{control}/token"))
                .header("dirigent-user", USER)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            Ok::<_, reqwest::Error>(got["token"].as_str().unwrap_or_default().to_string())
        }
    };
    LiveClient::new(ClientOptions::new(&stack.chlive).credentials(token))
}

/// Says `text` in `session`, as [`USER`].
async fn say(stack: &Stack, session: &str, text: &str) {
    let event = Event::UserMessage {
        text: text.into(),
        via: None,
    };
    let row = EventRow::new(session, uuid::Uuid::new_v4().to_string(), &event);
    client(stack).insert(EVENTS, [row]).await.unwrap();
}

/// The session's log, read through chlive as it grows, once `done` holds for it, within `within`.
async fn log_within(
    stack: &Stack,
    session: &str,
    within: Duration,
    done: impl Fn(&[EventRow]) -> bool,
) -> Vec<EventRow> {
    let client = client(stack);
    let mut rows = client
        .query(LOG_QUERY)
        .param("session", session)
        .rows::<EventRow>();
    let deadline = Instant::now() + within;
    let mut last = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline.into(), rows.next()).await {
            Ok(Some(Ok(r))) => {
                last = r.iter().map(|r| r.as_ref().clone()).collect::<Vec<_>>();
                if done(&last) {
                    return last;
                }
            }
            Ok(Some(Err(e))) => panic!("log subscription failed: {e}"),
            Ok(None) => panic!("log subscription ended"),
            Err(_) => panic!("timed out; the log has {:#?}", kinds(&last)),
        }
    }
}

/// The epochs of the players that attached, in order.
fn attached(rows: &[EventRow]) -> Vec<i64> {
    rows.iter()
        .filter_map(|r| match r.event().unwrap() {
            Event::PlayerAttached { epoch, .. } => Some(epoch),
            _ => None,
        })
        .collect()
}

fn turns_ended(rows: &[EventRow]) -> usize {
    rows.iter().filter(|r| r.kind == "turn_ended").count()
}

/// The last tool's output.
fn tool_output(rows: &[EventRow]) -> String {
    let row = rows.iter().rev().find(|r| r.kind == "tool_result");
    row.expect("a tool's result").data.clone()
}

/// What happened, leaving out the players attaching, their workspaces, and the session's titles
/// (which come when their call does).
fn kinds(rows: &[EventRow]) -> Vec<String> {
    rows.iter()
        .filter(|r| {
            !["player_attached", "workspace_ready", "checkpoint", "titled"]
                .contains(&r.kind.as_str())
        })
        .map(|r| match r.event().unwrap() {
            Event::UserMessage { text, .. } => format!("user:{text}"),
            Event::AssistantMessage { content, .. } => {
                let calls = content
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::ToolCall { .. }))
                    .count();
                let text: String = content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                // its first line: `--mock`'s replies go on with random paragraphs
                format!(
                    "assistant:{}{}",
                    text.lines().next().unwrap_or_default(),
                    "+tool".repeat(calls)
                )
            }
            Event::ToolResult { is_error, .. } => {
                format!("tool:{}", if is_error { "err" } else { "ok" })
            }
            Event::TurnEnded { reason, .. } => format!("end:{reason:?}"),
            _ => r.kind.clone(),
        })
        .collect()
}

/// kubectl on the test cluster, in its namespace; its output.
fn kubectl(args: &[&str]) -> String {
    let out = std::process::Command::new("kubectl")
        .args(["-n", "dirigent"])
        .args(args)
        .output()
        .expect("kubectl");
    assert!(
        out.status.success(),
        "kubectl {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The session's VMs, by name.
fn vms(session: &str) -> Vec<String> {
    let label = format!("dirigent.dev/session={session}");
    kubectl(&["get", "vmi", "-l", &label, "-o", "name"])
        .lines()
        .map(str::to_string)
        .collect()
}

/// Stops the session; its VMs power off and control clears them away.
async fn stop_and_clear(stack: &Stack, session: &str) {
    let stop = api(
        stack,
        reqwest::Method::POST,
        &format!("/sessions/{session}/stop"),
    );
    assert!(stop.send().await.unwrap().status().is_success());
    let deadline = Instant::now() + Duration::from_secs(180);
    while !vms(session).is_empty() {
        assert!(
            Instant::now() < deadline,
            "the VMs stay: {:?}",
            vms(session)
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// A log line without its terminal colours (`ESC[…m`), for a control that logs them.
fn uncolored(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            chars.by_ref().find(|&c| c == 'm');
        } else {
            out.push(c);
        }
    }
    out
}

/// Control's log, its lines without colours.
fn control_log() -> Vec<String> {
    kubectl(&["logs", "deploy/dirigent-control", "-c", "control"])
        .lines()
        .map(uncolored)
        .collect()
}

/// A warm VM's player that waits for a session, within `within`.
async fn waiting_warm_player(within: Duration) -> String {
    let deadline = Instant::now() + within;
    loop {
        let players = kubectl(&[
            "get",
            "vmi",
            "-l",
            "dirigent.dev/warm",
            "-o",
            "jsonpath={.items[*].metadata.labels.dirigent\\.dev/player}",
        ]);
        let log = control_log();
        let waits = |p: &&str| {
            log.iter()
                .any(|l| l.contains("a warm player waits") && l.contains(*p))
        };
        if let Some(p) = players.split_whitespace().find(waits) {
            return p.to_string();
        }
        assert!(Instant::now() < deadline, "no warm player waits: {players}");
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// The cluster's own system, once it's built, within `within`; none if it hasn't one.
async fn system_within(stack: &Stack, within: Duration) -> Option<String> {
    let deadline = Instant::now() + within;
    loop {
        let status = get(stack, "/system").await;
        if status["configured"] != true {
            return None;
        }
        if status["state"] == "ready" {
            return Some(status["store_path"].as_str().unwrap().to_string());
        }
        assert!(
            Instant::now() < deadline,
            "the system isn't built: {status}"
        );
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// The last generation control's log tells of on lines with `what`.
fn last_generation(what: &str) -> u64 {
    control_log()
        .iter()
        .filter(|l| l.contains(what))
        .filter_map(|l| {
            let (_, n) = l.rsplit_once("generation=")?;
            n.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .ok()
        })
        .max()
        .unwrap_or(0)
}

/// The shared store's generation VMs get, once it's past `after`, within `within`.
async fn generation_within(after: u64, within: Duration) -> u64 {
    let deadline = Instant::now() + within;
    loop {
        let last = last_generation("VMs get the shared Nix store's generation");
        if last > after {
            return last;
        }
        assert!(Instant::now() < deadline, "no generation past {after}");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[tokio::test]
async fn a_player_in_a_vm_finishes_its_turn_when_the_vm_goes_and_hands_over() {
    let Some(stack) = stack() else { return };
    let session = put_session(&stack, serde_json::json!({ "start": true })).await;
    // a turn that outlasts the VM: it's deleted while the command runs
    say(&stack, &session, "!sleep 20 && echo slept > note.txt").await;
    let boot = Duration::from_secs(600);
    log_within(&stack, &session, boot, |r| {
        r.iter().any(|r| r.kind == "assistant_message")
    })
    .await;
    let [vm] = &vms(&session)[..] else {
        panic!("{:?}", vms(&session))
    };
    kubectl(&["delete", vm, "--wait=false"]);

    // the turn finished, was checkpointed, and the next player came, in a new VM
    let log = log_within(&stack, &session, boot, |r| attached(r) == [1, 2]).await;
    assert_eq!(
        kinds(&log),
        [
            "user:!sleep 20 && echo slept > note.txt",
            "turn_started",
            "assistant:+tool",
            "tool:ok",
            "assistant:mock reply #2: ran it",
            "end:Completed",
            "player_detached",
        ]
    );

    // its workspace is the first's, restored from the checkpoint
    say(&stack, &session, "!cat note.txt").await;
    let log = log_within(&stack, &session, boot, |r| turns_ended(r) == 2).await;
    let read = tool_output(&log);
    assert!(read.contains("slept"), "{read}");

    // stopped, its VM powers off and control clears it away
    stop_and_clear(&stack, &session).await;
}

#[tokio::test]
async fn a_session_on_a_machine_of_its_own_gets_a_vm_that_size() {
    let Some(stack) = stack() else { return };
    let machine = serde_json::json!({ "cpu": 2, "memory": 2, "disk": 3 });
    let session = put_session(
        &stack,
        serde_json::json!({ "start": true, "resources": machine }),
    )
    .await;
    say(
        &stack,
        &session,
        "!echo cores:$(nproc) mib:$(free -m | awk '/Mem/ {print $2}') \
         gib:$(df -BG --output=size /var/lib/dirigent | tail -1 | tr -d ' G')",
    )
    .await;
    let log = log_within(&stack, &session, Duration::from_secs(600), |r| {
        turns_ended(r) == 1
    })
    .await;
    let out = tool_output(&log);
    let got = |what: &str| -> u32 {
        out.split(&format!("{what}:"))
            .find_map(|s| {
                let n: String = s.chars().take_while(char::is_ascii_digit).collect();
                n.parse().ok()
            })
            .unwrap_or_else(|| panic!("no {what} in {out}"))
    };
    assert_eq!(got("cores"), 2, "{out}");
    // the guest's own take of its 2 GiB, and of its 3 GiB disk once formatted
    assert!((1500..=2048).contains(&got("mib")), "{out}");
    assert!((2..=3).contains(&got("gib")), "{out}");
    let [vm] = &vms(&session)[..] else {
        panic!("{:?}", vms(&session))
    };
    assert!(!vm.contains("/warm-"), "a warm VM on another machine: {vm}");
    stop_and_clear(&stack, &session).await;
}

/// With warm VMs (the chart's `player.warm.count`): a session on the default machine takes the
/// warm VM whose player waits, and another is made.
#[tokio::test]
async fn a_session_takes_a_warm_vm_and_another_is_made() {
    let Some(stack) = stack() else { return };
    if get(&stack, "/machines").await["warm"] == 0 {
        return;
    }
    let boot = Duration::from_secs(600);
    let waiting = waiting_warm_player(boot).await;

    let asked = Instant::now();
    let session = put_session(&stack, serde_json::json!({ "start": true })).await;
    log_within(&stack, &session, boot, |r| attached(r) == [1]).await;
    // no boot: the player was up already
    let took = asked.elapsed();
    assert!(took < Duration::from_secs(30), "attached in {took:?}");
    assert_eq!(
        vms(&session),
        [format!("virtualmachineinstance.kubevirt.io/warm-{waiting}")]
    );
    say(&stack, &session, "!echo warm").await;
    log_within(&stack, &session, boot, |r| turns_ended(r) == 1).await;

    let next = waiting_warm_player(boot).await;
    assert_ne!(next, waiting);
    stop_and_clear(&stack, &session).await;
}

/// With the cluster's own system and the shared store (nix/kube-test/system.yaml): a VM boots
/// into the system (kexec) over a generation of the store, and what it added is in a later
/// generation, which the next VM has without fetching it.
#[tokio::test]
async fn a_vm_boots_into_the_clusters_system_and_shares_what_it_added() {
    let Some(stack) = stack() else { return };
    let Some(system) = system_within(&stack, Duration::from_secs(2400)).await else {
        return;
    };
    let first = generation_within(0, Duration::from_secs(300)).await;
    let boot = Duration::from_secs(600);
    // a path no generation can have yet, besides one from upstream
    let mark = format!("dirigent-e2e-{}", uuid::Uuid::new_v4());

    let session = put_session(&stack, serde_json::json!({ "start": true })).await;
    say(
        &stack,
        &session,
        &format!(
            "!readlink /run/current-system && cowsay moo && nix shell nixpkgs#hello -c hello \
             && echo {mark} > /tmp/{mark} && nix-store --add /tmp/{mark}"
        ),
    )
    .await;
    let log = log_within(&stack, &session, boot, |r| turns_ended(r) == 1).await;
    let out = tool_output(&log);
    assert!(out.contains(&system), "not on the cluster's system: {out}");
    assert!(
        out.contains("moo") && out.contains("Hello, world!"),
        "{out}"
    );
    assert!(out.contains(&format!("-{mark}")), "not added: {out}");
    // stopped, its player tells what it added; control has it in the generation it writes next
    // (a minute apart at most, its rounds every 30 s)
    stop_and_clear(&stack, &session).await;
    tokio::time::sleep(Duration::from_secs(120)).await;
    let writing = last_generation("writing the shared Nix store's generation");
    assert!(writing > first, "no generation of what it added");
    let next = generation_within(writing - 1, Duration::from_secs(600)).await;

    let session = put_session(&stack, serde_json::json!({ "start": true })).await;
    say(
        &stack,
        &session,
        &format!(
            "!ls /nix/store | grep -- '-{mark}$' && nix shell --offline nixpkgs#hello -c hello"
        ),
    )
    .await;
    let log = log_within(&stack, &session, boot, |r| turns_ended(r) == 1).await;
    let out = tool_output(&log);
    assert!(
        out.contains(&mark) && out.contains("Hello, world!"),
        "generation {next}: {out}"
    );
    stop_and_clear(&stack, &session).await;
}
