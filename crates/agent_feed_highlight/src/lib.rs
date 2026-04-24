use agent_feed_core::{
    AgentEvent, Bulletin, BulletinChip, BulletinId, BulletinMode, EventKind, PrivacyClass,
    TickerItem, VisualKind,
};
use time::OffsetDateTime;

const DEFAULT_DWELL_MS: u64 = 14_000;
const URGENT_DWELL_MS: u64 = 20_000;

#[must_use]
pub fn score_event(event: &AgentEvent) -> u8 {
    if let Some(score_hint) = event.score_hint {
        return score_hint.min(100);
    }

    let base: u8 = match event.kind {
        EventKind::PermissionDenied => 95,
        EventKind::PermissionRequest => 88,
        EventKind::TestFail => 90,
        EventKind::TurnFail => 88,
        EventKind::ToolFail => 82,
        EventKind::McpFail => 72,
        EventKind::TurnComplete => 72,
        EventKind::PlanUpdate => 68,
        EventKind::FileChanged | EventKind::DiffCreated => 62,
        EventKind::TestPass => 60,
        EventKind::ToolComplete | EventKind::CommandExec => 42,
        EventKind::AdapterHealth => 36,
        EventKind::SessionStart | EventKind::SessionEnd => 34,
        EventKind::AgentMessage | EventKind::SummaryCreated => 30,
        EventKind::ToolStart | EventKind::TurnStart | EventKind::McpCall | EventKind::WebSearch => {
            20
        }
        EventKind::Error => 70,
    };

    let file_bonus = (event.files.len() as u8).saturating_mul(3).min(12);
    base.saturating_add(event.severity.score_bonus())
        .saturating_add(file_bonus)
        .min(100)
}

#[must_use]
pub fn bulletin_from_event(event: &AgentEvent) -> Bulletin {
    let priority = score_event(event);
    let mode = mode_for(event.kind);
    let dwell_ms = if event.kind.is_urgent() {
        URGENT_DWELL_MS
    } else {
        DEFAULT_DWELL_MS
    };

    Bulletin {
        id: BulletinId::new(),
        created_at: OffsetDateTime::now_utc(),
        mode,
        priority,
        dwell_ms,
        eyebrow: eyebrow(event),
        headline: clamp_words(&event.title, 14),
        deck: clamp_words(
            event
                .summary
                .as_deref()
                .unwrap_or_else(|| deck_fallback(event)),
            28,
        ),
        lower_third: lower_third(event, priority),
        chips: chips(event, priority),
        ticker: ticker(event),
        image: None,
        visual: VisualKind::Stage,
        privacy: PrivacyClass::Redacted,
    }
}

fn mode_for(kind: EventKind) -> BulletinMode {
    match kind {
        EventKind::PermissionDenied
        | EventKind::PermissionRequest
        | EventKind::TestFail
        | EventKind::ToolFail
        | EventKind::TurnFail
        | EventKind::McpFail
        | EventKind::Error => BulletinMode::Breaking,
        EventKind::FileChanged | EventKind::DiffCreated => BulletinMode::DiffAtlas,
        EventKind::CommandExec | EventKind::ToolStart | EventKind::ToolComplete => {
            BulletinMode::CommandDesk
        }
        EventKind::McpCall => BulletinMode::McpWire,
        EventKind::SummaryCreated => BulletinMode::Recap,
        _ => BulletinMode::Dispatch,
    }
}

fn eyebrow(event: &AgentEvent) -> String {
    let project = event.project.as_deref().unwrap_or("local");
    format!("{} / {} / {}", event.agent, project, event.kind.as_str())
}

fn deck_fallback(event: &AgentEvent) -> &'static str {
    match event.kind {
        EventKind::TurnComplete => "turn completed with display-safe activity.",
        EventKind::TurnFail => "turn failed. details were reduced before display.",
        EventKind::PermissionRequest => "permission requested. policy boundary is visible.",
        EventKind::PermissionDenied => "permission denied. risky action did not proceed.",
        EventKind::TestFail => "test signal failed after agent activity.",
        EventKind::TestPass => "test signal passed after agent activity.",
        EventKind::FileChanged => "changed files were detected without showing raw diffs.",
        _ => "agent activity was reduced to a safe bulletin.",
    }
}

fn lower_third(event: &AgentEvent, priority: u8) -> String {
    let mut parts = vec![
        event.agent.clone(),
        event.project.clone().unwrap_or_else(|| "local".to_string()),
        format!("score {priority}"),
        "redacted".to_string(),
    ];
    if !event.files.is_empty() {
        parts.insert(2, format!("{} files", event.files.len()));
    }
    parts.join(" · ")
}

fn chips(event: &AgentEvent, priority: u8) -> Vec<BulletinChip> {
    let mut labels = vec![
        event.agent.clone(),
        event.kind.as_str().to_string(),
        format!("score {priority}"),
        "redacted".to_string(),
    ];
    if let Some(project) = &event.project {
        labels.insert(1, project.clone());
    }
    labels.into_iter().take(5).map(BulletinChip::new).collect()
}

fn ticker(event: &AgentEvent) -> Vec<TickerItem> {
    let mut items = Vec::new();
    if let Some(tool) = &event.tool {
        items.push(TickerItem::new(format!("{} used {}", event.agent, tool)));
    }
    if !event.files.is_empty() {
        items.push(TickerItem::new(format!(
            "{} changed {} files",
            event.agent,
            event.files.len()
        )));
    }
    if event.kind.is_urgent() {
        items.push(TickerItem::new(format!("{} needs attention", event.kind)));
    }
    items
}

fn clamp_words(input: &str, max_words: usize) -> String {
    let mut words = input.split_whitespace();
    let mut output = Vec::new();
    for _ in 0..max_words {
        if let Some(word) = words.next() {
            output.push(word);
        }
    }
    if words.next().is_some() {
        format!("{}...", output.join(" "))
    } else {
        output.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_feed_core::{AgentEvent, EventKind, SourceKind};

    #[test]
    fn failing_tests_score_as_breaking() {
        let mut event = AgentEvent::new(SourceKind::Generic, EventKind::TestFail, "tests failed");
        event.agent = "codex".to_string();
        event.project = Some("agent_feed".to_string());

        let bulletin = bulletin_from_event(&event);

        assert_eq!(bulletin.mode, BulletinMode::Breaking);
        assert!(bulletin.priority >= 90);
        assert!(bulletin.eyebrow.contains("codex / agent_feed"));
    }
}
