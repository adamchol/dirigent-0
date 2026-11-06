//! When the player stops: told to by control (the session was stopped, through the lease's
//! `Detach`), giving the session back; or from outside (its machine is going: SIGTERM from a
//! node drained, its VM deleted, or Ctrl-C), handing it over to the next player. Either way the
//! turn running may finish; stopped from outside, it's cut once the grace period is up (the
//! machine's own, less the time left to checkpoint and hand over), or at a second signal.

use std::pin::Pin;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::Sleep;
use tracing::info;

pub struct Stop {
    /// `Some(now)` once the player is to stop (as the agent's `Detach` takes it).
    pub when: watch::Receiver<Option<bool>>,
    /// Whether it's stopped from outside, so the session is handed over.
    pub outside: watch::Receiver<bool>,
}

/// Control's word (`control`, the lease's `detach`), with `signals` from outside, cutting the
/// turn `grace` after the first (or after the start, `stopped` by one already).
pub fn watch(
    mut control: watch::Receiver<Option<bool>>,
    mut signals: mpsc::Receiver<()>,
    grace: Duration,
    stopped: bool,
) -> Stop {
    let (when_tx, when) = watch::channel(*control.borrow_and_update());
    let (outside_tx, outside) = watch::channel(stopped);
    if stopped {
        stop(&when_tx, Some(false));
    }
    tokio::spawn(async move {
        let mut cut: Option<Pin<Box<Sleep>>> = stopped.then(|| Box::pin(tokio::time::sleep(grace)));
        let (mut from_control, mut from_outside) = (true, true);
        loop {
            tokio::select! {
                changed = control.changed(), if from_control => match changed {
                    Ok(()) => stop(&when_tx, *control.borrow_and_update()),
                    Err(_) => from_control = false,
                },
                signal = signals.recv(), if from_outside => match signal {
                    Some(()) if !*outside_tx.borrow() => {
                        info!(grace = grace.as_secs(), "stopped from outside: the turn may finish within the grace period");
                        outside_tx.send_replace(true);
                        stop(&when_tx, Some(false));
                        cut = Some(Box::pin(tokio::time::sleep(grace)));
                    }
                    Some(()) => {
                        info!("stopped from outside again: cutting the turn");
                        stop(&when_tx, Some(true));
                    }
                    None => from_outside = false,
                },
                () = async { cut.as_mut().expect("set").await }, if cut.is_some() => {
                    info!("the grace period is up: cutting the turn");
                    cut = None;
                    stop(&when_tx, Some(true));
                }
                () = when_tx.closed() => return,
            }
        }
    });
    Stop { when, outside }
}

/// Stopping, `now` or not; a stop that's now stays so.
fn stop(when: &watch::Sender<Option<bool>>, word: Option<bool>) {
    let Some(now) = word else { return };
    when.send_if_modified(|w| {
        let next = Some(now || w.unwrap_or(false));
        let changed = *w != next;
        *w = next;
        changed
    });
}

/// SIGTERM (how process managers, Kubernetes and a VM's shutdown stop it), Ctrl-C and SIGHUP
/// (its terminal closing), each time they come.
pub fn signals() -> mpsc::Receiver<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let (tx, rx) = mpsc::channel(4);
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM can be handled");
    let mut hup = signal(SignalKind::hangup()).expect("SIGHUP can be handled");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT can be handled");
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = term.recv() => {}
                _ = hup.recv() => {}
                _ = int.recv() => {}
            }
            if tx.send(()).await.is_err() {
                return;
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn from_outside_the_turn_may_finish_until_the_grace_period_is_up() {
        let (_control, detach) = watch::channel(None);
        let (signal, signals) = mpsc::channel(4);
        let stop = watch(detach, signals, Duration::from_secs(60), false);
        settle().await;
        assert_eq!((*stop.when.borrow(), *stop.outside.borrow()), (None, false));
        signal.send(()).await.unwrap();
        settle().await;
        assert_eq!(
            (*stop.when.borrow(), *stop.outside.borrow()),
            (Some(false), true)
        );
        tokio::time::sleep(Duration::from_secs(59)).await;
        assert_eq!(*stop.when.borrow(), Some(false));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(*stop.when.borrow(), Some(true));
    }

    #[tokio::test(start_paused = true)]
    async fn stopped_while_acquiring_it_starts_stopped_from_outside() {
        let (_control, detach) = watch::channel(None);
        let (_signal, signals) = mpsc::channel(4);
        let stop = watch(detach, signals, Duration::from_secs(60), true);
        assert_eq!(
            (*stop.when.borrow(), *stop.outside.borrow()),
            (Some(false), true)
        );
        tokio::time::sleep(Duration::from_secs(61)).await;
        assert_eq!(*stop.when.borrow(), Some(true));
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_signal_cuts_the_turn_at_once() {
        let (_control, detach) = watch::channel(None);
        let (signal, signals) = mpsc::channel(4);
        let stop = watch(detach, signals, Duration::from_secs(60), false);
        signal.send(()).await.unwrap();
        signal.send(()).await.unwrap();
        settle().await;
        assert_eq!(*stop.when.borrow(), Some(true));
    }

    #[tokio::test(start_paused = true)]
    async fn controls_word_stops_it_without_handing_over() {
        let (control, detach) = watch::channel(None);
        let (_signal, signals) = mpsc::channel(4);
        let stop = watch(detach, signals, Duration::from_secs(60), false);
        control.send_replace(Some(false));
        settle().await;
        assert_eq!(
            (*stop.when.borrow(), *stop.outside.borrow()),
            (Some(false), false)
        );
        control.send_replace(Some(true));
        settle().await;
        assert_eq!(*stop.when.borrow(), Some(true));
        // and a graceful word after a stop that's now leaves it now
        control.send_replace(Some(false));
        settle().await;
        assert_eq!(*stop.when.borrow(), Some(true));
    }
}
