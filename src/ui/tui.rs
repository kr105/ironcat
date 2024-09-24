// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use log::Level;
use ratatui::{
	crossterm::event::{self, Event, KeyCode},
	layout::{Constraint, Direction, Layout, Rect},
	style::{Color, Modifier, Style},
	text::{Line, Span, Text},
	widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap},
	DefaultTerminal, Frame,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

use crate::{logger_channel::LogChannelEntry, nodes::NodeManager};

/// Initializes and runs the TUI application
pub async fn tui_start(node_manager: Arc<NodeManager>, log_receiver: mpsc::Receiver<LogChannelEntry>) {
	let terminal = ratatui::init();
	_ = run(terminal, node_manager, log_receiver).context("app loop failed");
	ratatui::restore();
}

/// Main TUI application loop
fn run(
	mut terminal: DefaultTerminal,
	node_manager: Arc<NodeManager>,
	mut log_receiver: mpsc::Receiver<LogChannelEntry>,
) -> Result<()> {
	let mut log_buffer = Vec::new();

	loop {
		// Process new log messages
		while let Ok(log_message) = log_receiver.try_recv() {
			log_buffer.push(log_message);
			if log_buffer.len() > 100 {
				log_buffer.remove(0);
			}
		}

		let node_manager_clone = node_manager.clone();
		terminal.draw(|f| draw(f, node_manager_clone, &log_buffer))?;

		if should_quit()? {
			break;
		}
	}
	Ok(())
}

/// Renders the TUI layout and content
fn draw(frame: &mut Frame, node_manager: Arc<NodeManager>, log_buffer: &[LogChannelEntry]) {
	let layout = Layout::default()
		.direction(Direction::Horizontal)
		.constraints(vec![Constraint::Percentage(30), Constraint::Percentage(70)])
		.split(frame.area());

	// Render left panel
	draw_left_panel(frame, layout[0], &node_manager);

	// Render log panel
	let log_block = Block::default().borders(Borders::ALL).title("Logs");
	let inner_area = log_block.inner(layout[1]);
	frame.render_widget(log_block, layout[1]);

	// Calculate visible lines to create the scrolling effect
	let visible_lines = inner_area.height as usize;

	let log_text = create_log_text(log_buffer, visible_lines);

	let log_paragraph = Paragraph::new(log_text).wrap(Wrap { trim: true });
	frame.render_widget(log_paragraph, inner_area);
}

/// Creates formatted log text for display
fn create_log_text(log_buffer: &[LogChannelEntry], visible_lines: usize) -> Text<'static> {
	let mut text = Text::default();
	let start_index = log_buffer.len().saturating_sub(visible_lines);

	for entry in log_buffer.iter().skip(start_index) {
		let (log_name, log_color) = match entry.log_type {
			Level::Trace => ("TRACE", Color::Magenta),
			Level::Debug => ("DEBUG", Color::Cyan),
			Level::Info => (" INFO", Color::Green),
			Level::Warn => (" WARN", Color::Yellow),
			Level::Error => ("ERROR", Color::Red),
		};

		let log_line = Line::from(vec![
			Span::styled(
				format!("[{}]", log_name),
				Style::default().fg(log_color).add_modifier(Modifier::BOLD),
			),
			Span::raw(" "),
			Span::styled(entry.text.clone(), Style::default().fg(Color::White)),
		]);

		text.extend(Text::from(log_line));
	}

	text
}

fn draw_left_panel(frame: &mut Frame, area: Rect, node_manager: &NodeManager) {
	// Split the panel horizontally
	let chunks = Layout::default()
		.direction(Direction::Vertical)
		.constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
		.split(area);

	// Top half: Stats display
	let stats = node_manager.get_stats();
	let stats_text = format!(
		"Total Nodes: {}\nConnected Nodes: {}\nDisconnected Nodes: {}",
		stats.total_nodes, stats.connected_nodes, stats.disconnected_nodes
	);
	let stats_paragraph = Paragraph::new(stats_text).block(Block::default().borders(Borders::ALL).title("Node Stats"));
	frame.render_widget(stats_paragraph, chunks[0]);

	// Bottom half: Nodes table sorted by height
	let mut nodes: Vec<_> = node_manager.nodes.iter().collect();
	nodes.sort_by(|a, b| b.value().height.cmp(&a.value().height));

	let header = Row::new(vec![
		Cell::from("Endpoint"),
		Cell::from("Height"),
		Cell::from("Connected"),
	]);

	let rows: Vec<Row> = nodes
		.iter()
		.map(|node| {
			let node = node.value();
			Row::new(vec![
				Cell::from(node.endpoint.to_string()),  // Convert to Cell
				Cell::from(node.height.to_string()),    // Convert to Cell
				Cell::from(node.connected.to_string()), // Convert to Cell
			])
		})
		.collect();

	let table = Table::new(
		rows,
		vec![
			Constraint::Percentage(50), // "Endpoint" column width
			Constraint::Percentage(25), // "Height" column width
			Constraint::Percentage(25), // "Connected" column width
		],
	)
	.header(header) // Use a Row for the header
	.block(Block::default().borders(Borders::ALL).title("Connected Nodes"));

	frame.render_widget(table, chunks[1]);
}

/// Checks if the user has requested to quit the application
fn should_quit() -> Result<bool> {
	if event::poll(Duration::from_millis(250)).context("event poll failed")? {
		if let Event::Key(key) = event::read().context("event read failed")? {
			return Ok(KeyCode::Char('q') == key.code);
		}
	}
	Ok(false)
}
