macro_rules! assert_cf_literal {
    ($name:ident) => {
        assert_eq!(synapse_storage::cf::$name, stringify!($name));
    };
}

#[test]
fn cf_constants_match_literal_names() {
    assert_cf_literal!(CF_EVENTS);
    assert_cf_literal!(CF_OBSERVATIONS);
    assert_cf_literal!(CF_PROFILES);
    assert_cf_literal!(CF_MODEL_CACHE);
    assert_cf_literal!(CF_SESSIONS);
    assert_cf_literal!(CF_REFLEX_AUDIT);
    assert_cf_literal!(CF_OCR_CACHE);
    assert_cf_literal!(CF_TELEMETRY);
    assert_cf_literal!(CF_ACTION_LOG);
    assert_cf_literal!(CF_PROCESS_HISTORY);
    assert_cf_literal!(CF_KV);
}

#[test]
fn cf_names_sorted_snapshot() {
    let mut actual = vec![
        synapse_storage::cf::CF_EVENTS,
        synapse_storage::cf::CF_OBSERVATIONS,
        synapse_storage::cf::CF_PROFILES,
        synapse_storage::cf::CF_MODEL_CACHE,
        synapse_storage::cf::CF_SESSIONS,
        synapse_storage::cf::CF_REFLEX_AUDIT,
        synapse_storage::cf::CF_OCR_CACHE,
        synapse_storage::cf::CF_TELEMETRY,
        synapse_storage::cf::CF_ACTION_LOG,
        synapse_storage::cf::CF_PROCESS_HISTORY,
        synapse_storage::cf::CF_KV,
    ];
    println!("regression_state=storage_cf before=declared_order:{actual:?}");
    actual.sort_unstable();
    actual.dedup();
    println!(
        "regression_state=storage_cf after=sorted_unique:{actual:?} final_count:{}",
        actual.len()
    );
    assert_eq!(actual.len(), 11);
    insta::assert_json_snapshot!("column_family_names", actual);
}
