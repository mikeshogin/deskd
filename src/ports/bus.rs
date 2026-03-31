//! MessageBus port — abstraction over message transport.
//!
//! The bus handles routing messages between agents, adapters, and external
//! targets. Implementations may use Unix sockets, in-memory channels, or
//! network transports.

use anyhow::Result;

use crate::domain::message::Message;

/// Abstraction over message transport.
///
/// Workers and adapters interact with the bus through this trait.
/// The Unix socket implementation lives in `bus.rs`; tests can use
/// an in-memory implementation.
pub trait MessageBus: Send + Sync {
    /// Send a message to the bus for routing.
    fn send(&self, msg: &Message) -> impl std::future::Future<Output = Result<()>> + Send;

    /// Receive the next message addressed to this client.
    fn recv(&self) -> impl std::future::Future<Output = Result<Message>> + Send;

    /// Register this client on the bus with the given subscriptions.
    fn register(
        &self,
        name: &str,
        subscriptions: &[String],
    ) -> impl std::future::Future<Output = Result<()>> + Send;
}
