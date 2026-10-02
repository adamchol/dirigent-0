//! The loop's connection to the outside world. The loop receives inputs from
//! a session, publishes every event to it, and on start loads past events
//! from it to rebuild the model history. The loop itself stores none of the
//! published events.

pub mod grpc;

use async_trait::async_trait;

use crate::event::Event;

#[async_trait]
pub trait Session: Send {
    /// Events published earlier for this conversation, oldest first (resume).
    async fn load(&mut self) -> anyhow::Result<Vec<Event>>;

    /// Next input: `UserMessage` or `UserInterrupt`. `None` means the client
    /// is gone. Must be cancel-safe: the loop polls it inside `select!`.
    async fn recv(&mut self) -> Option<Event>;

    /// Hand over an event the loop handled or produced. Best effort once the
    /// client is gone.
    async fn publish(&mut self, event: &Event);
}
