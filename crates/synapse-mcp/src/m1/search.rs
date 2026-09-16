use synapse_core::{AccessibleNode, DetectedEntity};

use crate::m1::{FindParams, FindResult, FindResultKind};

pub fn element_match(node: &AccessibleNode, params: &FindParams) -> Option<FindResult> {
    if params.in_window.is_some() && params.in_window.as_ref() != node.parent.as_ref() {
        return None;
    }
    if let Some(role) = &params.role
        && !node.role.eq_ignore_ascii_case(role)
    {
        return None;
    }
    if let Some(name_substring) = &params.name_substring
        && !contains_ascii_case(&node.name, name_substring)
    {
        return None;
    }
    if let Some(automation_id) = &params.automation_id
        && node.automation_id.as_deref() != Some(automation_id.as_str())
    {
        return None;
    }
    let mut score = 0.25;
    if let Some(query) = &params.query {
        score += element_query_score(node, query)?;
    }
    if node.focused {
        score += 0.1;
    }
    if node.bbox.w > 0 && node.bbox.h > 0 {
        score += 0.15;
    } else {
        score -= 0.25;
    }
    if synapse_a11y::cdp_backend_from_element_id(&node.element_id).is_some() {
        score += 0.05;
    }
    Some(FindResult {
        kind: FindResultKind::Element,
        element_id: Some(node.element_id.clone()),
        entity_id: None,
        name: Some(node.name.clone()),
        role: Some(node.role.clone()),
        automation_id: node.automation_id.clone(),
        class_label: None,
        bbox: node.bbox,
        score,
    })
}

pub fn entity_match(entity: &DetectedEntity, params: &FindParams) -> Option<FindResult> {
    let query = params.query.as_ref()?;
    contains_ascii_case(&entity.class_label, query).then_some(FindResult {
        kind: FindResultKind::Entity,
        element_id: None,
        entity_id: Some(entity.entity_id.clone()),
        name: None,
        role: None,
        automation_id: None,
        class_label: Some(entity.class_label.clone()),
        bbox: entity.bbox,
        score: entity.confidence,
    })
}

fn contains_ascii_case(value: &str, needle: &str) -> bool {
    value
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

fn element_query_score(node: &AccessibleNode, query: &str) -> Option<f32> {
    let query = query.trim();
    if query.is_empty() {
        return Some(0.0);
    }

    if contains_ascii_case(&node.name, query)
        || contains_ascii_case(&node.role, query)
        || node
            .automation_id
            .as_deref()
            .is_some_and(|value| contains_ascii_case(value, query))
    {
        return Some(0.65);
    }

    let terms = query_terms(query);
    if terms.is_empty() {
        return None;
    }
    let haystack = format!(
        "{} {} {}",
        node.name,
        node.role,
        node.automation_id.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase();
    let matched = terms
        .iter()
        .filter(|term| haystack.contains(term.as_str()))
        .count();
    if matched == 0 {
        return None;
    }

    let coverage = matched as f32 / terms.len() as f32;
    let density = matched.min(3) as f32 / 3.0;
    Some(0.35f32.mul_add(coverage, 0.15 * density))
}

fn query_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for raw in query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|term| term.len() >= 2)
    {
        let term = raw.to_ascii_lowercase();
        if !terms.contains(&term) {
            terms.push(term);
        }
    }
    terms
}

#[cfg(test)]
mod tests {
    use synapse_core::{AccessibleNode, Rect, element_id};

    use super::*;

    fn node(element_id: synapse_core::ElementId, automation_id: &str) -> AccessibleNode {
        node_with(
            element_id,
            "Apply",
            "button",
            Some(automation_id),
            Rect {
                x: 0,
                y: 0,
                w: 10,
                h: 10,
            },
        )
    }

    fn node_with(
        element_id: synapse_core::ElementId,
        name: &str,
        role: &str,
        automation_id: Option<&str>,
        bbox: Rect,
    ) -> AccessibleNode {
        AccessibleNode {
            element_id,
            parent: None,
            name: name.to_owned(),
            role: role.to_owned(),
            automation_id: automation_id.map(str::to_owned),
            value: None,
            bbox,
            enabled: true,
            focused: false,
            patterns: Vec::new(),
            children_count: 0,
            depth: 0,
        }
    }

    #[test]
    fn cdp_duplicate_scores_above_uia_duplicate_for_actionable_find_result() {
        let params = FindParams {
            role: Some("button".to_owned()),
            name_substring: Some("Apply".to_owned()),
            ..FindParams::default()
        };
        let uia = node(element_id(0x100, "0000002a00000008"), "apply");
        let cdp = node(
            synapse_a11y::cdp_element_id(0x100, 8),
            "cdp:backendNodeId=8",
        );

        let uia_score = element_match(&uia, &params).expect("uia result").score;
        let cdp_score = element_match(&cdp, &params).expect("cdp result").score;

        println!(
            "readback=find_score edge=cdp_duplicate before=uia:{uia_score} after=cdp:{cdp_score}"
        );
        assert!(
            cdp_score > uia_score,
            "find should prefer actionable CDP web nodes over UIA duplicates"
        );
    }

    #[test]
    fn query_matches_named_uia_terms_without_exact_phrase() {
        let params = FindParams {
            query: Some("accept agree terms creator continue founding I agree".to_owned()),
            scope: Some(crate::m1::FindScope::Elements),
            ..FindParams::default()
        };
        let checkbox = node_with(
            element_id(0x100, "0000002a00000009"),
            "I agree to the current Leapable Creator Agreement and understand publishing",
            "check box",
            Some("creator-agreement-checkbox"),
            Rect {
                x: 10,
                y: 20,
                w: 20,
                h: 20,
            },
        );

        let result = element_match(&checkbox, &params).expect("query terms should match name");

        println!(
            "readback=find_query edge=tokenized_name before=query:{:?} after=name:{:?} score:{}",
            params.query, result.name, result.score
        );
        assert!(result.score > 0.45);
    }

    #[test]
    fn query_filters_role_matches_without_matching_terms() {
        let params = FindParams {
            query: Some("Accept Creator Agreement activate continue submit".to_owned()),
            role: Some("button".to_owned()),
            scope: Some(crate::m1::FindScope::Elements),
            ..FindParams::default()
        };
        let browser_back = node_with(
            element_id(0x100, "0000002a0000000a"),
            "Back",
            "button",
            Some("browser-back"),
            Rect {
                x: 1,
                y: 1,
                w: 24,
                h: 24,
            },
        );

        let result = element_match(&browser_back, &params);

        println!(
            "readback=find_query edge=role_without_terms before=query:{:?} role:{:?} after={:?}",
            params.query, params.role, result
        );
        assert!(result.is_none());
    }

    #[test]
    fn visible_named_element_scores_above_zero_size_match() {
        let params = FindParams {
            query: Some("Creator Agreement".to_owned()),
            scope: Some(crate::m1::FindScope::Elements),
            ..FindParams::default()
        };
        let visible = node_with(
            element_id(0x100, "0000002a0000000b"),
            "Accept the Creator Agreement",
            "heading",
            None,
            Rect {
                x: 10,
                y: 20,
                w: 300,
                h: 40,
            },
        );
        let zero_size = node_with(
            element_id(0x100, "0000002a0000000c"),
            "Creator Agreement hidden button",
            "button",
            None,
            Rect {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            },
        );

        let visible_score = element_match(&visible, &params)
            .expect("visible match")
            .score;
        let zero_score = element_match(&zero_size, &params)
            .expect("zero-size match remains visible in results")
            .score;

        println!(
            "readback=find_rank edge=zero_size before=visible:{visible_score} zero:{zero_score} after_visible_above={}",
            visible_score > zero_score
        );
        assert!(visible_score > zero_score);
    }
}
