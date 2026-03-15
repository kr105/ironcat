// SPDX-License-Identifier: Apache-2.0

//! Peers tab -- full-screen peer table

use ratatui::{
	Frame,
	layout::{Alignment, Constraint, Rect},
	style::{Color, Modifier, Style},
	text::Line,
	widgets::{Block, Borders, Cell, Row, Table},
};

use crate::nodes::{MAX_TIMEOUT_STRIKES, NodeSnapshot, NodeStateLabel};

/// Renders the peers table (full-screen version for the Peers tab)
pub fn render(frame: &mut Frame, area: Rect, nodes: &[NodeSnapshot]) {
	let header = Row::new(vec![
		Cell::from(Line::from("Endpoint").alignment(Alignment::Center)),
		Cell::from(Line::from("Height").alignment(Alignment::Center)),
		Cell::from(Line::from("State").alignment(Alignment::Center)),
		Cell::from(Line::from("Strikes").alignment(Alignment::Center)),
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

	// Sort: connected/handshaking/connecting first, then dead/disconnected/banned at the bottom
	let mut sorted_nodes: Vec<&NodeSnapshot> = nodes.iter().collect();
	sorted_nodes.sort_by_key(|n| match n.state_label {
		NodeStateLabel::Connected => 0,
		NodeStateLabel::Handshaking => 1,
		NodeStateLabel::Connecting => 2,
		NodeStateLabel::Disconnected(_) => 3,
		NodeStateLabel::Dead => 4,
		NodeStateLabel::Banned => 5,
	});

	let rows: Vec<Row> = sorted_nodes
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
				Cell::from({
					if !matches!(node.state_label, NodeStateLabel::Connected) || node.timeout_strikes == 0 {
						String::new()
					} else if node.strike_excluded {
						"EXCLUDED".to_string()
					} else {
						format!("{}/{MAX_TIMEOUT_STRIKES}", node.timeout_strikes)
					}
				})
				.style(Style::default().fg(
					if !matches!(node.state_label, NodeStateLabel::Connected) || node.timeout_strikes == 0 {
						Color::Reset
					} else if node.strike_excluded {
						Color::Red
					} else {
						Color::Yellow
					},
				)),
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
			Constraint::Length(10),     // Strikes
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
