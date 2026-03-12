// SPDX-License-Identifier: Apache-2.0

use std::fmt::Write as _;

use tokio::sync::mpsc;
use tracing::{Level, Subscriber, field::Visit};
use tracing_subscriber::{Layer, layer::Context};

/// A log entry sent to the TUI for display
pub struct TuiLogEntry {
	/// Severity level of the log entry
	pub level: Level,
	/// The formatted log message text
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

/// Visitor that formats a tracing event as "message field1=val1 field2=val2"
struct FieldVisitor {
	message: String,
	fields: String,
}

impl FieldVisitor {
	const fn new() -> Self {
		Self {
			message: String::new(),
			fields: String::new(),
		}
	}

	/// Builds the final formatted string: "message  key=val key=val"
	fn finish(self) -> String {
		if self.fields.is_empty() {
			self.message
		} else {
			format!("{}  {}", self.message, self.fields.trim_start())
		}
	}

	/// Appends a key=value pair to the fields buffer
	fn append_field(&mut self, name: &str, value: &dyn std::fmt::Display) {
		let _ = write!(self.fields, " {name}={value}");
	}
}

impl Visit for FieldVisitor {
	fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
		if field.name() == "message" {
			self.message = format!("{value:?}");
		} else {
			let _ = write!(self.fields, " {}={:?}", field.name(), value);
		}
	}

	fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
		if field.name() == "message" {
			self.message = value.to_string();
		} else {
			self.append_field(field.name(), &value);
		}
	}

	fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
		self.append_field(field.name(), &value);
	}

	fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
		self.append_field(field.name(), &value);
	}

	fn record_u128(&mut self, field: &tracing::field::Field, value: u128) {
		self.append_field(field.name(), &value);
	}

	fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
		self.append_field(field.name(), &value);
	}
}

impl<S: Subscriber> Layer<S> for TuiLayer {
	fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
		let mut visitor = FieldVisitor::new();
		event.record(&mut visitor);

		// Drop messages if channel is full (same behavior as the old LogChannel)
		_ = self.sender.try_send(TuiLogEntry {
			level: *event.metadata().level(),
			text: visitor.finish(),
		});
	}
}
