// SPDX-License-Identifier: Apache-2.0

// Tests use unwrap for brevity since panics are the intended failure mode
#![allow(clippy::unwrap_used)]

use ironcat::tui_layer::{TuiLayer, TuiLogEntry};
use tokio::sync::mpsc;
use tracing::Level;
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
