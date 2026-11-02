//! The loop's connection to the event system: a session's log, which users and the player all
//! append to. The loop reads the log on start to rebuild its state, then receives what users
//! append, and appends every event it handles or produces. The loop stores none of the log.

pub mod chlive;
pub mod control;

use async_trait::async_trait;

use dirigent_protocol::Event;

#[async_trait]
pub trait Session: Send {
    /// The log so far, oldest first: each event with its id (resume).
    async fn load(&mut self) -> anyhow::Result<Vec<(String, Event)>>;

    /// The next event users append, with its id. `None` means no more will come.
    /// Must be cancel-safe: the loop polls it inside `select!`.
    async fn recv(&mut self) -> Option<(String, Event)>;

    /// Append an event; returns its id. Best effort.
    async fn publish(&mut self, event: &Event) -> String;
}
