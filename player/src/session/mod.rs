//! The loop's connection to the event system. The loop receives inputs from a session,
//! publishes every event to it, and on start loads past events from it to rebuild the model
//! history. The loop itself stores none of the published events.

pub mod chlive;

use async_trait::async_trait;

use dirigent_protocol::Event;

#[async_trait]
pub trait Session: Send {
    /// Events published earlier for this conversation, oldest first (resume).
    async fn load(&mut self) -> anyhow::Result<Vec<Event>>;

    /// Next input: `UserMessage` or `UserInterrupt`. `None` means no more inputs will come.
    /// Must be cancel-safe: the loop polls it inside `select!`.
    async fn recv(&mut self) -> Option<Event>;

    /// Hand over an event the loop handled or produced. Best effort.
    async fn publish(&mut self, event: &Event);
}
