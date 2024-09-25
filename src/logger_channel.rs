// SPDX-License-Identifier: Apache-2.0

use log::{Level, LevelFilter, Log, Metadata, Record};
use simplelog::{Config, SharedLogger};
use tokio::sync::mpsc;

pub struct LogChannelEntry {
	pub log_type: Level,
	pub text: String,
}

/// A custom logger that sends log messages through a channel
pub struct LogChannel {
	level: LevelFilter,
	config: Config,
	sender: mpsc::Sender<LogChannelEntry>,
}

impl LogChannel {
	/// Creates a new `LogChannel` with the specified log level, configuration, and sender
	pub fn new(log_level: LevelFilter, config: Config, sender: mpsc::Sender<LogChannelEntry>) -> Box<LogChannel> {
		Box::new(LogChannel {
			level: log_level,
			config,
			sender,
		})
	}
}

impl Log for LogChannel {
	fn enabled(&self, metadata: &Metadata<'_>) -> bool {
		metadata.level() <= self.level
	}

	fn log(&self, record: &Record<'_>) {
		if self.enabled(record.metadata()) {
			_ = self.sender.try_send(LogChannelEntry {
				log_type: record.level(),
				text: record.args().to_string(),
			});

			// Intentionally ignore send errors, dropping the message if the channel is full
		}
	}

	fn flush(&self) {}
}

impl SharedLogger for LogChannel {
	fn level(&self) -> LevelFilter {
		self.level
	}

	fn config(&self) -> Option<&Config> {
		Some(&self.config)
	}

	fn as_log(self: Box<Self>) -> Box<dyn Log> {
		Box::new(*self)
	}
}
