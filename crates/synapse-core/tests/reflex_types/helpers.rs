use std::fmt::Debug;

use proptest::{
    prelude::*,
    test_runner::{Config, TestRng, TestRunner},
};
use serde::{Serialize, de::DeserializeOwned};

#[allow(clippy::needless_pass_by_value)]
pub fn round_trip<T>(type_name: &str, edge: &str, value: T) -> Result<T, Box<dyn std::error::Error>>
where
    T: Clone + Debug + PartialEq + Serialize + DeserializeOwned + 'static,
{
    let before = serde_json::to_value(value.clone())?;
    println!("readback=serde_round_trip type={type_name} edge={edge} before={before}");
    let parsed = serde_json::from_value::<T>(before)?;
    let after = serde_json::to_value(&parsed)?;
    println!(
        "readback=serde_round_trip type={type_name} edge={edge} after={after} result_value={after}"
    );
    assert_eq!(parsed, value);
    Ok(parsed)
}

pub fn assert_strategy_round_trips<T, S>(
    type_name: &str,
    strategy: S,
) -> Result<(), Box<dyn std::error::Error>>
where
    T: Clone + Debug + PartialEq + Serialize + DeserializeOwned + 'static,
    S: Strategy<Value = T>,
{
    let config = Config {
        cases: 1_000,
        failure_persistence: None,
        ..Config::default()
    };
    let algorithm = config.rng_algorithm;
    let mut runner = TestRunner::new_with_rng(config, TestRng::deterministic_rng(algorithm));

    println!("readback=serde_round_trip_proptest type={type_name} before=cases:1000");
    runner.run(&strategy, |value| {
        let json = serde_json::to_value(value.clone())?;
        let parsed = serde_json::from_value::<T>(json)?;
        prop_assert_eq!(parsed, value);
        Ok(())
    })?;
    println!(
        "readback=serde_round_trip_proptest type={type_name} after=cases:1000 result_value=all_round_tripped"
    );
    Ok(())
}
