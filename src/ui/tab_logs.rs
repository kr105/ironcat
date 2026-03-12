// SPDX-License-Identifier: Apache-2.0

//! Logs tab -- full-screen log viewer

use std::collections::VecDeque;

use ratatui::{
	Frame,
	layout::Rect,
	style::{Color, Modifier, Style},
	text::{Line, Span, Text},
	widgets::{Block, Borders, Paragraph, Wrap},
};
use tracing::Level;

use crate::tui_layer::TuiLogEntry;

/// Renders the log viewer with border and title
pub fn render(frame: &mut Frame, area: Rect, log_buffer: &VecDeque<TuiLogEntry>) {
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
pub fn create_log_text(log_buffer: &VecDeque<TuiLogEntry>, visible_lines: usize) -> Text<'static> {
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
