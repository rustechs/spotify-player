use std::{collections::VecDeque, sync::Arc, time::Duration};

use parking_lot::Mutex;
use tracing::Subscriber;
use tracing_subscriber::Layer;

/// How long an event waits for the buffer before its line is dropped.
const BUFFER_LOCK_TIMEOUT: Duration = Duration::from_millis(100);

pub struct BufferLayer {
    buffer: Arc<Mutex<VecDeque<String>>>,
    max_lines: usize,
}

impl BufferLayer {
    pub fn new(buffer: Arc<Mutex<VecDeque<String>>>, max_lines: usize) -> Self {
        Self { buffer, max_lines }
    }
}

impl<S: Subscriber> Layer<S> for BufferLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);

        let level = event.metadata().level();
        let target = event.metadata().target();
        let line = format!(
            "{} {:>5} {}: {}",
            chrono::Local::now().format("%H:%M:%S"),
            level,
            target,
            visitor.message
        );

        // Never block indefinitely: the panic hook logs through this layer, so a
        // panic on a thread that already holds the buffer (the Logs page render)
        // would otherwise park forever with the terminal still in raw mode. A
        // dropped line is still in the log file.
        let Some(mut buf) = self.buffer.try_lock_for(BUFFER_LOCK_TIMEOUT) else {
            return;
        };
        buf.push_back(line);
        while buf.len() > self.max_lines {
            buf.pop_front();
        }
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn core::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn event_logged_while_the_buffer_is_held_is_dropped_instead_of_deadlocking() {
        let buffer = Arc::new(Mutex::new(VecDeque::new()));
        let subscriber = tracing_subscriber::registry().with(BufferLayer::new(buffer.clone(), 2));

        tracing::subscriber::with_default(subscriber, || {
            let held = buffer.lock();
            // The same thread re-enters the layer, as the panic hook would.
            tracing::error!("lost");
            drop(held);

            tracing::error!("first");
            tracing::warn!("second");
            tracing::info!("third");
        });

        let lines = buffer.lock();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].ends_with("second") && lines[0].contains("WARN"),
            "{lines:?}"
        );
        assert!(
            lines[1].ends_with("third") && lines[1].contains("INFO"),
            "{lines:?}"
        );
    }
}
