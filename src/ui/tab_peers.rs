// SPDX-License-Identifier: Apache-2.0

//! Peers tab -- full-screen peer table

use ratatui::{
	Frame,
	layout::{Alignment, Constraint, Rect},
	style::{Color, Modifier, Style},
	text::Line,
	widgets::{Block, Borders, Cell, Row, Table},
};

use crate::nodes::{NodeSnapshot, NodeStateLabel};

/// Renders the peers table (full-screen version for the Peers tab)
pub fn render(frame: &mut Frame, area: Rect, nodes: &[NodeSnapshot]) {
	let header = Row::new(vec![
		Cell::from(Line::from("Endpoint").alignment(Alignment::Center)),
		Cell::from(Line::from("Height").alignment(Alignment::Center)),
		Cell::from(Line::from("State").alignment(Alignment::Center)),
		Cell::from(Line::from("Type").alignment(Alignment::Center)),
		Cell::from(Line::from("Version").alignment(Alignment::Center)),
		Cell::from(Line::from("Last Seen").alignment(Alignment::Center)),
		Cell::from(Line::from("User Agent").alignment(Alignment::Center)),
	])
	.style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));

	let now = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0);

	let rows: Vec<Row> = nodes
		.iter()
		.map(|node| {
			let state_color = match node.state_label {
				NodeStateLabel::Connected => Color::Green,
				NodeStateLabel::Handshaking | NodeStateLabel::Connecting => Color::Yellow,
				NodeStateLabel::Disconnected(_) | NodeStateLabel::Dead => Color::DarkGray,
				NodeStateLabel::Banned => Color::Red,
			};

			let last_seen_str = if node.last_seen == 0 {
				"-".to_string()
			} else {
				format_elapsed(now.saturating_sub(node.last_seen))
			};

			Row::new(vec![
				Cell::from(format!("{}:{}", node.address, node.port)),
				Cell::from(node.height.to_string()),
				Cell::from(node.state_label.to_string()).style(Style::default().fg(state_color)),
				Cell::from(node.connection_type.to_string()),
				Cell::from(node.version.to_string()),
				Cell::from(last_seen_str),
				Cell::from(node.user_agent.clone()),
			])
		})
		.collect();

	let table = Table::new(
		rows,
		vec![
			Constraint::Length(22),     // Endpoint (IP:port)
			Constraint::Length(8),      // Height
			Constraint::Length(13),     // State (longest: "Disconnected")
			Constraint::Length(4),      // Type (In/Out)
			Constraint::Length(8),      // Version
			Constraint::Length(10),     // Last Seen
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

/// Formats seconds elapsed as a human-readable string
fn format_elapsed(secs: u64) -> String {
	if secs < 60 {
		format!("{secs}s ago")
	} else if secs < 3600 {
		format!("{}m ago", secs / 60)
	} else if secs < 86400 {
		format!("{}h ago", secs / 3600)
	} else {
		format!("{}d ago", secs / 86400)
	}
}
