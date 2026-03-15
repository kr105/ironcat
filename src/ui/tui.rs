// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use ratatui::{
	DefaultTerminal, Frame,
	crossterm::event::{self, Event, KeyCode},
	layout::{Constraint, Direction, Layout, Rect},
	style::{Color, Modifier, Style},
	text::{Line, Span},
	widgets::Paragraph,
};
use std::{collections::VecDeque, sync::Arc, time::Duration};
use tokio::sync::mpsc;

use crate::{
	mempool::{MempoolStats, RecentTxEntry},
	nodes::NodeManager,
	tui_layer::TuiLogEntry,
};

use super::{tab_logs, tab_mempool, tab_overview, tab_peers};

/// Number of tabs in the TUI
const TAB_COUNT: usize = 4;

/// Tab label names in display order
const TAB_NAMES: [&str; TAB_COUNT] = ["Overview", "Mempool", "Peers", "Logs"];

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
	/// Currently active tab index (0-3)
	active_tab: usize,
	/// Mempool statistics (updated when wired in)
	mempool_stats: MempoolStats,
	/// Recent mempool transactions (updated when wired in)
	mempool_recent: Vec<RecentTxEntry>,
}

/// Initializes and runs the TUI application
///
/// Takes ownership of all arguments because this runs via `spawn_blocking`
/// which requires `'static + Send`
// spawn_blocking requires 'static + Send, so ownership is needed
#[allow(clippy::needless_pass_by_value)]
pub fn tui_start(
	node_manager: Arc<NodeManager>,
	log_receiver: mpsc::Receiver<TuiLogEntry>,
	mempool: Arc<tokio::sync::RwLock<crate::mempool::Mempool>>,
) {
	let terminal = ratatui::init();
	if let Err(e) = run(terminal, &node_manager, log_receiver, &mempool) {
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
	mempool: &Arc<tokio::sync::RwLock<crate::mempool::Mempool>>,
) -> Result<()> {
	let mut log_buffer = VecDeque::new();
	let mut last_draw = std::time::Instant::now();
	let mut tui_state = TuiState {
		prev_height: node_manager.chain_height(),
		prev_time: std::time::Instant::now(),
		headers_per_sec: 0.0,
		active_tab: 0,
		mempool_stats: MempoolStats::default(),
		mempool_recent: Vec::new(),
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
			// Update mempool stats from the shared mempool
			if let Ok(pool) = mempool.try_read() {
				tui_state.mempool_stats = pool.get_stats();
				tui_state.mempool_recent = pool.recent_entries(50);
			}

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

		if event::poll(poll_timeout).context("event poll failed")?
			&& let Event::Key(key) = event::read().context("event read failed")?
		{
			match key.code {
				KeyCode::Char('q') => break,
				KeyCode::Tab => {
					#[allow(clippy::arithmetic_side_effects)] // tab index wraps within small constant range
					{
						tui_state.active_tab = (tui_state.active_tab + 1) % TAB_COUNT;
					}
					terminal.draw(|f| draw(f, node_manager, &log_buffer, &tui_state))?;
					last_draw = std::time::Instant::now();
				}
				_ => {}
			}
		}
	}
	Ok(())
}

/// Renders the tab bar, active tab content, and help bar
fn draw(frame: &mut Frame, node_manager: &NodeManager, log_buffer: &VecDeque<TuiLogEntry>, tui_state: &TuiState) {
	let main_chunks = Layout::default()
		.direction(Direction::Vertical)
		.constraints([
			Constraint::Length(1), // Tab bar
			Constraint::Min(4),    // Tab content
			Constraint::Length(1), // Help bar
		])
		.split(frame.area());

	// Tab bar
	#[allow(clippy::indexing_slicing)] // layout produces exactly 3 elements
	draw_tab_bar(frame, main_chunks[0], tui_state.active_tab);

	let (stats, mut nodes) = node_manager.get_snapshot();
	let chain_height = node_manager.chain_height();
	let tip_bits = node_manager.chain_tip_bits();
	let block_height = node_manager.block_height();

	// Tab content
	#[allow(clippy::indexing_slicing)] // layout produces exactly 3 elements
	match tui_state.active_tab {
		0 => {
			let last_block_secs = node_manager.last_block_elapsed().map(|d| d.as_secs());
			let last_block_tx_count = node_manager.last_block_tx_count();
			let data = tab_overview::OverviewData {
				stats: &stats,
				nodes: &nodes,
				listen_port: node_manager.listen_port,
				port_reachable: node_manager.is_port_reachable(),
				chain_height,
				tip_bits,
				block_height,
				mempool_stats: &tui_state.mempool_stats,
				last_block_secs,
				last_block_tx_count: if last_block_tx_count > 0 {
					Some(last_block_tx_count as usize)
				} else {
					None
				},
			};
			tab_overview::render(frame, main_chunks[1], &data);
		}
		1 => {
			tab_mempool::render(
				frame,
				main_chunks[1],
				&tui_state.mempool_stats,
				&tui_state.mempool_recent,
			);
		}
		2 => {
			nodes.sort_by(|a, b| b.height.cmp(&a.height));
			tab_peers::render(frame, main_chunks[1], &nodes);
		}
		3 => {
			tab_logs::render(frame, main_chunks[1], log_buffer);
		}
		// active_tab is always 0..TAB_COUNT due to modulo in Tab handler
		_ => {}
	}

	// Help bar
	#[allow(clippy::indexing_slicing)] // layout produces exactly 3 elements
	draw_help_bar(frame, main_chunks[2]);
}

/// Renders the tab bar with the active tab highlighted
fn draw_tab_bar(frame: &mut Frame, area: Rect, active_tab: usize) {
	let mut spans = Vec::new();
	spans.push(Span::raw(" "));

	for (i, name) in TAB_NAMES.iter().enumerate() {
		if i == active_tab {
			spans.push(Span::styled(
				format!("[{name}]"),
				Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
			));
		} else {
			spans.push(Span::styled(format!("[{name}]"), Style::default().fg(Color::DarkGray)));
		}
		spans.push(Span::raw(" "));
	}

	frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Formats a number with comma separators (e.g. 1,234,567)
pub(crate) fn format_number(n: u32) -> String {
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
pub(crate) fn format_progress_bar(ratio: f64, width: usize) -> String {
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

/// Renders the help bar at the bottom
fn draw_help_bar(frame: &mut Frame, area: Rect) {
	let help = Line::from(vec![
		Span::styled(" Tab", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
		Span::styled(":switch", Style::default().fg(Color::DarkGray)),
		Span::styled("  ", Style::default()),
		Span::styled("q", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
		Span::styled(":quit", Style::default().fg(Color::DarkGray)),
	]);
	frame.render_widget(Paragraph::new(help), area);
}
