//! Where normalised events go.

use iris_core::event::Event;

/// Receives normalised events.
pub trait EventSink: Send + Sync {
    /// Delivers one event. Implementations must not block for long; the
    /// sources call this from their receive loops.
    fn publish(&self, event: Event);
}
