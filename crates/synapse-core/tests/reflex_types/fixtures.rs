use chrono::{DateTime, Duration, Utc};
use synapse_core::{
    Action, Backend, ButtonAction, DataPredicate, EventFilter, EventSource, Key, KeyCode,
    MouseButton, ReflexKind, ReflexLifetime, ReflexRegistration, ReflexState, ReflexStatus,
    ReflexThen,
};

pub fn empty_registration() -> ReflexRegistration {
    ReflexRegistration {
        id: String::new(),
        kind: empty_kind(),
        priority: 0,
        lifetime: ReflexLifetime::UntilCancelled,
        exclusive: false,
    }
}

pub fn required_registration() -> ReflexRegistration {
    ReflexRegistration {
        id: "reflex-required".to_owned(),
        kind: hold_move_kind(),
        priority: 100,
        lifetime: ReflexLifetime::UntilCancelled,
        exclusive: false,
    }
}

pub fn full_registration() -> ReflexRegistration {
    ReflexRegistration {
        id: "reflex-fully-populated".to_owned(),
        kind: full_on_event_kind(),
        priority: 900,
        lifetime: full_lifetime(),
        exclusive: true,
    }
}

pub fn empty_kind() -> ReflexKind {
    ReflexKind::Combo {
        steps: Vec::new(),
        backend: Backend::Auto,
    }
}

pub fn hold_move_kind() -> ReflexKind {
    ReflexKind::HoldMove {
        keys: vec![key_named("w")],
        backend: Backend::Software,
        re_assert: false,
    }
}

pub fn full_on_event_kind() -> ReflexKind {
    ReflexKind::OnEvent {
        when: full_filter(),
        then: full_then(),
        debounce_ms: 250,
    }
}

pub fn full_lifetime() -> ReflexLifetime {
    ReflexLifetime::UntilEvent {
        filter: EventFilter::Kind {
            kind: "entity_disappeared".to_owned(),
        },
    }
}

pub fn empty_status() -> ReflexStatus {
    ReflexStatus {
        id: String::new(),
        kind_summary: String::new(),
        state: ReflexState::Active,
        registered_at: fixed_time(0),
        last_fired_at: None,
        fire_count: 0,
        priority: 0,
        lifetime: ReflexLifetime::UntilCancelled,
        exclusive: false,
        last_error_code: None,
    }
}

pub fn required_status() -> ReflexStatus {
    ReflexStatus {
        id: "reflex-required".to_owned(),
        kind_summary: "hold_move".to_owned(),
        state: ReflexState::Active,
        registered_at: fixed_time(1),
        last_fired_at: None,
        fire_count: 0,
        priority: 100,
        lifetime: ReflexLifetime::UntilCancelled,
        exclusive: false,
        last_error_code: None,
    }
}

pub fn full_status() -> ReflexStatus {
    ReflexStatus {
        id: "reflex-starved".to_owned(),
        kind_summary: "aim_track(track:42)".to_owned(),
        state: ReflexState::Starved,
        registered_at: fixed_time(10),
        last_fired_at: Some(fixed_time(12)),
        fire_count: 64,
        priority: 250,
        lifetime: ReflexLifetime::Duration { ms: 5_000 },
        exclusive: true,
        last_error_code: Some("REFLEX_STARVED".to_owned()),
    }
}

pub fn empty_then() -> ReflexThen {
    ReflexThen::Actions {
        actions: Vec::new(),
    }
}

pub fn action_then(key: &str) -> ReflexThen {
    ReflexThen::Action {
        action: key_press_action(key),
    }
}

pub fn full_then() -> ReflexThen {
    ReflexThen::Actions {
        actions: vec![
            key_press_action("e"),
            Action::MouseButton {
                button: MouseButton::Left,
                action: ButtonAction::Press,
                hold_ms: 16,
                backend: Backend::Software,
            },
        ],
    }
}

pub fn full_filter() -> EventFilter {
    EventFilter::And {
        args: vec![
            EventFilter::Kind {
                kind: "hud_value_changed".to_owned(),
            },
            EventFilter::Source {
                source: EventSource::PerceptionHud,
            },
            EventFilter::Data {
                path: "/field".to_owned(),
                predicate: DataPredicate::Eq {
                    value: serde_json::json!("hp"),
                },
            },
            EventFilter::Data {
                path: "/new".to_owned(),
                predicate: DataPredicate::Lt {
                    value: serde_json::json!(20),
                },
            },
        ],
    }
}

pub fn key_press_action(key: &str) -> Action {
    Action::KeyPress {
        key: key_named(key),
        hold_ms: 30,
        backend: Backend::Software,
    }
}

pub fn key_named(value: &str) -> Key {
    Key {
        code: KeyCode::Named {
            value: value.to_owned(),
        },
        use_scancode: false,
    }
}

pub fn fixed_time(offset_seconds: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from(std::time::SystemTime::UNIX_EPOCH) + Duration::seconds(offset_seconds)
}
