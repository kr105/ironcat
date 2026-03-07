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

use crate::{
	difficulty::{self, ConsensusParams},
	nodes::{NodeManager, NodeSnapshot, NodeStateLabel, NodeStats},
	tui_layer::TuiLogEntry,
};

/// Interval between stats-driven redraws when no log events arrive
const STATS_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Mutable state tracked across TUI redraws for rate calculations
struct TuiState {
	/// Chain height at the previous redraw
	prev_height: u32,
	/// Timestamp of the previous redraw
	prev_time: std::time::Instant,
	/// Computed headers-per-second rate
	headers_per_sec: f64,
}

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
///
/// Only redraws when new log messages arrive, a key is pressed,
/// or the stats refresh interval elapses
fn run(
	mut terminal: DefaultTerminal,
	node_manager: &Arc<NodeManager>,
	mut log_receiver: mpsc::Receiver<TuiLogEntry>,
) -> Result<()> {
	let mut log_buffer = VecDeque::new();
	let mut last_draw = std::time::Instant::now();
	let mut tui_state = TuiState {
		prev_height: node_manager.chain_height(),
		prev_time: std::time::Instant::now(),
		headers_per_sec: 0.0,
	};

	// Initial draw so the screen isn't blank
	terminal.draw(|f| draw(f, node_manager, &log_buffer, &tui_state))?;

	loop {
		let mut needs_redraw = false;

		// Drain all pending log messages
		while let Ok(log_message) = log_receiver.try_recv() {
			log_buffer.push_back(log_message);
			if log_buffer.len() > 100 {
				log_buffer.pop_front();
			}
			needs_redraw = true;
		}

		// Periodic redraw for node stats changes
		if last_draw.elapsed() >= STATS_REFRESH_INTERVAL {
			needs_redraw = true;
		}

		if needs_redraw {
			// Update headers/s rate
			let current_height = node_manager.chain_height();
			let elapsed = tui_state.prev_time.elapsed().as_secs_f64();
			if elapsed > 0.5 {
				#[allow(clippy::cast_precision_loss)] // height delta fits comfortably in f64
				let delta = f64::from(current_height.saturating_sub(tui_state.prev_height));
				#[allow(clippy::float_arithmetic)] // rate calculation requires division
				{
					tui_state.headers_per_sec = delta / elapsed;
				}
				tui_state.prev_height = current_height;
				tui_state.prev_time = std::time::Instant::now();
			}

			terminal.draw(|f| draw(f, node_manager, &log_buffer, &tui_state))?;
			last_draw = std::time::Instant::now();
		}

		// Block on keyboard events; use shorter timeout if we expect more logs soon
		let time_to_refresh = STATS_REFRESH_INTERVAL.saturating_sub(last_draw.elapsed());
		let poll_timeout = time_to_refresh.min(Duration::from_millis(500));

		if event::poll(poll_timeout).context("event poll failed")? {
			if let Event::Key(key) = event::read().context("event read failed")? {
				if key.code == KeyCode::Char('q') {
					break;
				}
			}
		}
	}
	Ok(())
}

/// Renders the dashboard layout: stats row, peers table, logs panel, help bar
fn draw(frame: &mut Frame, node_manager: &NodeManager, log_buffer: &VecDeque<TuiLogEntry>, tui_state: &TuiState) {
	// Vertical stack: stats | peers | logs | help
	let main_chunks = Layout::default()
		.direction(Direction::Vertical)
		.constraints([
			Constraint::Length(9),      // Stats row
			Constraint::Percentage(50), // Peers table
			Constraint::Percentage(50), // Logs panel
			Constraint::Length(1),      // Help bar
		])
		.split(frame.area());

	let (stats, mut nodes) = node_manager.get_snapshot();
	let chain_height = node_manager.chain_height();
	let tip_bits = node_manager.chain_tip_bits();

	// Top row: Network Stats | Chain Sync (side by side)
	#[allow(clippy::indexing_slicing)] // layout produces exactly 4 elements
	let top_chunks = Layout::default()
		.direction(Direction::Horizontal)
		.constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
		.split(main_chunks[0]);

	#[allow(clippy::indexing_slicing)] // layout produces exactly 2 elements
	draw_network_stats(frame, top_chunks[0], &stats);
	let block_height = node_manager.block_height();
	#[allow(clippy::indexing_slicing)] // layout produces exactly 2 elements
	draw_chain_sync(
		frame,
		top_chunks[1],
		chain_height,
		tip_bits,
		block_height,
		&nodes,
		tui_state,
	);

	nodes.sort_by(|a, b| b.height.cmp(&a.height));

	// Peers table (full width)
	#[allow(clippy::indexing_slicing)] // layout produces exactly 4 elements
	draw_peers_table(frame, main_chunks[1], &nodes);

	// Logs panel (full width)
	#[allow(clippy::indexing_slicing)] // layout produces exactly 4 elements
	draw_logs(frame, main_chunks[2], log_buffer);

	// Help bar
	#[allow(clippy::indexing_slicing)] // layout produces exactly 4 elements
	draw_help_bar(frame, main_chunks[3]);
}

/// Renders the Network Stats panel (top-left quadrant)
fn draw_network_stats(frame: &mut Frame, area: Rect, stats: &NodeStats) {
	let text = vec![
		Line::from(vec![
			Span::styled("Peers: ", Style::default().fg(Color::Gray)),
			Span::styled(
				format!("{}", stats.connected),
				Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
			),
			Span::styled(
				format!("  ({} in / {} out)", stats.incoming, stats.outgoing),
				Style::default().fg(Color::DarkGray),
			),
		]),
		Line::from(vec![
			Span::styled("Handshaking: ", Style::default().fg(Color::Gray)),
			Span::styled(format!("{}", stats.handshaking), Style::default().fg(Color::Yellow)),
			Span::styled("  Connecting: ", Style::default().fg(Color::Gray)),
			Span::styled(format!("{}", stats.connecting), Style::default().fg(Color::Yellow)),
		]),
		Line::from(vec![
			Span::styled("Disconnected: ", Style::default().fg(Color::Gray)),
			Span::styled(format!("{}", stats.disconnected), Style::default().fg(Color::DarkGray)),
			Span::styled("  Dead: ", Style::default().fg(Color::Gray)),
			Span::styled(format!("{}", stats.dead), Style::default().fg(Color::DarkGray)),
		]),
		Line::from(vec![
			Span::styled("Banned: ", Style::default().fg(Color::Gray)),
			Span::styled(format!("{}", stats.banned), Style::default().fg(Color::Red)),
			Span::styled("  Total: ", Style::default().fg(Color::Gray)),
			Span::styled(format!("{}", stats.total), Style::default().fg(Color::White)),
		]),
	];

	let paragraph = Paragraph::new(text).block(
		Block::default()
			.borders(Borders::ALL)
			.title("Network")
			.border_style(Style::default().fg(Color::Cyan)),
	);
	frame.render_widget(paragraph, area);
}

/// Renders the Chain Sync panel (top-right quadrant)
fn draw_chain_sync(
	frame: &mut Frame,
	area: Rect,
	chain_height: u32,
	tip_bits: u32,
	block_height: u32,
	nodes: &[NodeSnapshot],
	tui_state: &TuiState,
) {
	// Find best peer height among connected peers with reported height
	let best_peer = nodes.iter().filter(|n| n.height > 0).max_by_key(|n| n.height);

	let best_height = best_peer.map_or(0, |n| n.height);
	let best_addr = best_peer.map(|n| format!("{}", n.address));

	// Calculate sync progress ratio
	#[allow(clippy::cast_precision_loss, clippy::float_arithmetic)] // heights fit in f64, division needed for ratio
	let progress = if best_height > 0 {
		f64::from(chain_height) / f64::from(best_height)
	} else {
		0.0
	}
	.min(1.0);

	let best_label = best_addr
		.as_ref()
		.map_or_else(|| "unknown".to_string(), |addr| format!("{best_height} ({addr})"));

	let text = vec![
		Line::from(vec![
			Span::styled("Local:  ", Style::default().fg(Color::Gray)),
			Span::styled(
				format_number(chain_height),
				Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
			),
		]),
		Line::from(vec![
			Span::styled("Blocks: ", Style::default().fg(Color::Gray)),
			Span::styled(
				format!("{} / {}", format_number(block_height), format_number(chain_height)),
				Style::default().fg(if block_height >= chain_height {
					Color::Green
				} else {
					Color::Yellow
				}),
			),
		]),
		Line::from(vec![
			Span::styled("Best:   ", Style::default().fg(Color::Gray)),
			Span::styled(best_label, Style::default().fg(Color::White)),
		]),
		Line::from(vec![
			Span::styled("Sync:   ", Style::default().fg(Color::Gray)),
			Span::styled(
				format_progress_bar(progress, 20),
				Style::default().fg(if progress >= 1.0 { Color::Green } else { Color::Yellow }),
			),
			#[allow(clippy::float_arithmetic)] // percentage display requires multiplication
			Span::styled(format!(" {:.1}%", progress * 100.0), Style::default().fg(Color::White)),
		]),
		Line::from(vec![
			Span::styled("Rate:   ", Style::default().fg(Color::Gray)),
			Span::styled(
				format!("{:.1} headers/s", tui_state.headers_per_sec),
				Style::default().fg(Color::White),
			),
		]),
		Line::from(vec![
			Span::styled("Diff:   ", Style::default().fg(Color::Gray)),
			Span::styled(
				difficulty::format_difficulty(difficulty::compact_to_difficulty(tip_bits)),
				Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
			),
			Span::styled("  ", Style::default()),
			Span::styled(
				difficulty::active_algorithm(chain_height, &ConsensusParams::mainnet()),
				Style::default().fg(Color::Cyan),
			),
		]),
	];

	let paragraph = Paragraph::new(text).block(
		Block::default()
			.borders(Borders::ALL)
			.title("Chain Sync")
			.border_style(Style::default().fg(Color::Cyan)),
	);
	frame.render_widget(paragraph, area);
}

/// Formats a number with comma separators (e.g. 1,234,567)
fn format_number(n: u32) -> String {
	let s = n.to_string();
	let mut result = String::with_capacity(s.len().saturating_add(s.len() / 3));
	for (i, c) in s.chars().rev().enumerate() {
		if i > 0 && i % 3 == 0 {
			result.push(',');
		}
		result.push(c);
	}
	result.chars().rev().collect()
}

/// Builds an ASCII progress bar like "████████░░░░"
fn format_progress_bar(ratio: f64, width: usize) -> String {
	#[allow(
		clippy::cast_sign_loss,
		clippy::cast_possible_truncation,
		clippy::cast_precision_loss,
		clippy::float_arithmetic
	)] // ratio is 0.0..=1.0, width is small, multiplication needed for bar sizing
	let filled = (ratio * width as f64) as usize;
	let empty = width.saturating_sub(filled);
	let mut bar = String::with_capacity(width);
	for _ in 0..filled {
		bar.push('\u{2588}'); // full block
	}
	for _ in 0..empty {
		bar.push('\u{2591}'); // light shade
	}
	bar
}

/// Renders the Peers table (bottom-left quadrant)
fn draw_peers_table(frame: &mut Frame, area: Rect, nodes: &[NodeSnapshot]) {
	let header = Row::new(vec![
		Cell::from("Endpoint"),
		Cell::from("Height"),
		Cell::from("St"),
		Cell::from("Ty"),
		Cell::from("Ver"),
		Cell::from("User Agent"),
	])
	.style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));

	let rows: Vec<Row> = nodes
		.iter()
		.map(|node| {
			let state_color = match node.state_label {
				NodeStateLabel::Connected => Color::Green,
				NodeStateLabel::Handshaking | NodeStateLabel::Connecting => Color::Yellow,
				NodeStateLabel::Disconnected(_) | NodeStateLabel::Dead => Color::DarkGray,
				NodeStateLabel::Banned => Color::Red,
			};

			Row::new(vec![
				Cell::from(format!("{}:{}", node.address, node.port)),
				Cell::from(node.height.to_string()),
				Cell::from(node.state_label.short().to_string()).style(Style::default().fg(state_color)),
				Cell::from(node.connection_type.to_string()),
				Cell::from(node.version.to_string()),
				Cell::from(node.user_agent.clone()),
			])
		})
		.collect();

	let table = Table::new(
		rows,
		vec![
			Constraint::Min(22),        // Endpoint (IP:port)
			Constraint::Length(7),      // Height
			Constraint::Length(2),      // State (2-char)
			Constraint::Length(3),      // Type (In/Out)
			Constraint::Length(5),      // Version
			Constraint::Percentage(30), // User Agent (fill remaining)
		],
	)
	.header(header)
	.block(
		Block::default()
			.borders(Borders::ALL)
			.title("Peers")
			.border_style(Style::default().fg(Color::Cyan)),
	);

	frame.render_widget(table, area);
}

/// Renders the Logs panel (bottom-right quadrant)
fn draw_logs(frame: &mut Frame, area: Rect, log_buffer: &VecDeque<TuiLogEntry>) {
	let log_block = Block::default()
		.borders(Borders::ALL)
		.title("Logs")
		.border_style(Style::default().fg(Color::Cyan));

	let inner_area = log_block.inner(area);
	frame.render_widget(log_block, area);

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

/// Renders the help bar at the bottom
fn draw_help_bar(frame: &mut Frame, area: Rect) {
	let help = Line::from(vec![
		Span::styled(" q", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
		Span::styled(":quit", Style::default().fg(Color::DarkGray)),
	]);
	frame.render_widget(Paragraph::new(help), area);
}
