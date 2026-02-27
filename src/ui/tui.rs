// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use ratatui::{
	crossterm::event::{self, Event, KeyCode},
	layout::{Constraint, Direction, Layout, Rect},
	style::{Color, Modifier, Style},
	text::{Line, Span, Text},
	widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap},
	DefaultTerminal, Frame,
};
use std::{collections::VecDeque, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tracing::Level;

use crate::{nodes::NodeManager, tui_layer::TuiLogEntry};

/// Initializes and runs the TUI application
///
/// Takes ownership of both arguments because this runs via `spawn_blocking`
/// which requires `'static + Send`
// spawn_blocking requires 'static + Send, so ownership is needed
#[allow(clippy::needless_pass_by_value)]
pub fn tui_start(node_manager: Arc<NodeManager>, log_receiver: mpsc::Receiver<TuiLogEntry>) {
	let terminal = ratatui::init();
	if let Err(e) = run(terminal, &node_manager, log_receiver) {
		// TUI owns the terminal and the tracing channel may be dead, stderr is the only output option
		#[allow(clippy::print_stderr)]
		{
			eprintln!("TUI error: {e:?}");
		}
	}
	ratatui::restore();
}

/// Main TUI application loop
fn run(
	mut terminal: DefaultTerminal,
	node_manager: &Arc<NodeManager>,
	mut log_receiver: mpsc::Receiver<TuiLogEntry>,
) -> Result<()> {
	let mut log_buffer = VecDeque::new();

	loop {
		// Process new log messages
		while let Ok(log_message) = log_receiver.try_recv() {
			log_buffer.push_back(log_message);
			if log_buffer.len() > 100 {
				log_buffer.pop_front();
			}
		}

		let node_manager_clone = Arc::clone(node_manager);
		terminal.draw(|f| draw(f, &node_manager_clone, &log_buffer))?;

		if should_quit()? {
			break;
		}
	}
	Ok(())
}

/// Renders the TUI layout and content
fn draw(frame: &mut Frame, node_manager: &Arc<NodeManager>, log_buffer: &VecDeque<TuiLogEntry>) {
	// Layout returns exactly the number of constraints provided (2)
	let layout = Layout::default()
		.direction(Direction::Horizontal)
		.constraints(vec![Constraint::Percentage(40), Constraint::Percentage(60)])
		.split(frame.area());

	// Render left panel
	#[allow(clippy::indexing_slicing)]
	draw_left_panel(frame, layout[0], node_manager);

	// Render log panel
	let log_block = Block::default().borders(Borders::ALL).title("Logs");
	// Layout has 2 elements, index [1] is safe
	#[allow(clippy::indexing_slicing)]
	let inner_area = log_block.inner(layout[1]);
	#[allow(clippy::indexing_slicing)]
	frame.render_widget(log_block, layout[1]);

	// Calculate visible lines to create the scrolling effect
	let visible_lines = inner_area.height as usize;

	let log_text = create_log_text(log_buffer, visible_lines);

	let log_paragraph = Paragraph::new(log_text).wrap(Wrap { trim: true });
	frame.render_widget(log_paragraph, inner_area);
}

/// Creates formatted log text for display
fn create_log_text(log_buffer: &VecDeque<TuiLogEntry>, visible_lines: usize) -> Text<'static> {
	let mut text = Text::default();
	let start_index = log_buffer.len().saturating_sub(visible_lines);

	for entry in log_buffer.iter().skip(start_index) {
		let (log_name, log_color) = match entry.level {
			Level::TRACE => ("TRACE", Color::Magenta),
			Level::DEBUG => ("DEBUG", Color::Cyan),
			Level::INFO => (" INFO", Color::Green),
			Level::WARN => (" WARN", Color::Yellow),
			Level::ERROR => ("ERROR", Color::Red),
		};

		let log_line = Line::from(vec![
			Span::styled(
				format!("[{log_name}]"),
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
	// Layout returns exactly the number of constraints provided (2)
	let chunks = Layout::default()
		.direction(Direction::Vertical)
		.constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
		.split(area);

	// Single iteration for both stats and node list
	let (stats, mut nodes) = node_manager.get_snapshot();

	// Top half: Stats display
	let stats_text = format!(
		"Total Nodes: {}\nConnected Nodes: {}\nDisconnected Nodes: {}",
		stats.total, stats.connected, stats.disconnected
	);
	let stats_paragraph = Paragraph::new(stats_text).block(Block::default().borders(Borders::ALL).title("Node Stats"));
	// chunks has 2 elements from the 2 constraints above
	#[allow(clippy::indexing_slicing)]
	frame.render_widget(stats_paragraph, chunks[0]);

	// Bottom half: Nodes table sorted by height
	nodes.sort_by(|a, b| b.height.cmp(&a.height));

	let header = Row::new(vec![
		Cell::from("Endpoint"),
		Cell::from("Height"),
		Cell::from("State"),
		Cell::from("Type"),
	]);

	let rows: Vec<Row> = nodes
		.iter()
		.map(|node| {
			Row::new(vec![
				Cell::from(format!("{}:{}", node.address, node.port)),
				Cell::from(node.height.to_string()),
				Cell::from(node.state_label.clone()),
				Cell::from(node.connection_type.to_string()),
			])
		})
		.collect();

	let table = Table::new(
		rows,
		vec![
			Constraint::Percentage(50), // "Endpoint" column width
			Constraint::Percentage(15), // "Height" column width
			Constraint::Percentage(15), // "State" column width
			Constraint::Percentage(15), // "Type" column width
		],
	)
	.header(header)
	.block(Block::default().borders(Borders::ALL).title("Connected Nodes"));

	// chunks has 2 elements from the 2 constraints above
	#[allow(clippy::indexing_slicing)]
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
