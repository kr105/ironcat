// SPDX-License-Identifier: Apache-2.0

use tokio::sync::mpsc;
use tracing::{field::Visit, Level, Subscriber};
use tracing_subscriber::{layer::Context, Layer};

/// A log entry sent to the TUI for display
pub struct TuiLogEntry {
	pub level: Level,
	pub text: String,
}

/// A tracing layer that sends log events to the TUI via an mpsc channel
pub struct TuiLayer {
	sender: mpsc::Sender<TuiLogEntry>,
}

impl TuiLayer {
	/// Creates a new `TuiLayer` with the given sender
	pub const fn new(sender: mpsc::Sender<TuiLogEntry>) -> Self {
		Self { sender }
	}
}

/// Visitor that extracts the message field from a tracing event
struct MessageVisitor {
	message: String,
}

impl MessageVisitor {
	const fn new() -> Self {
		Self { message: String::new() }
	}
}

impl Visit for MessageVisitor {
	fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
		if field.name() == "message" {
			self.message = format!("{value:?}");
		}
	}

	fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
		if field.name() == "message" {
			self.message = value.to_string();
		}
	}
}

impl<S: Subscriber> Layer<S> for TuiLayer {
	fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
		let mut visitor = MessageVisitor::new();
		event.record(&mut visitor);

		// Drop messages if channel is full (same behavior as the old LogChannel)
		_ = self.sender.try_send(TuiLogEntry {
			level: *event.metadata().level(),
			text: visitor.message,
		});
	}
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
	use super::*;
	use tracing_subscriber::layer::SubscriberExt;

	#[test]
	fn layer_sends_log_entry() {
		let (tx, mut rx) = mpsc::channel::<TuiLogEntry>(10);
		let layer = TuiLayer::new(tx);
		let subscriber = tracing_subscriber::registry().with(layer);

		tracing::subscriber::with_default(subscriber, || {
			tracing::info!("hello from test");
		});

		let entry = rx.try_recv().unwrap();
		assert_eq!(entry.level, Level::INFO);
		assert!(entry.text.contains("hello from test"));
	}
}
