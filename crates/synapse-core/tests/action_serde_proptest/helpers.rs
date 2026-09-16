use proptest::{
    prelude::*,
    test_runner::{Config, TestRng, TestRunner},
};
use synapse_core::Action;

pub fn run_action_round_trip<S>(
    variant: &'static str,
    strategy: S,
) -> Result<(), Box<dyn std::error::Error>>
where
    S: Strategy<Value = Action>,
{
    let config = Config {
        cases: 1_000,
        failure_persistence: None,
        ..Config::default()
    };
    let algorithm = config.rng_algorithm;
    let mut runner = TestRunner::new_with_rng(config, TestRng::deterministic_rng(algorithm));

    runner.run(&strategy, |action| {
        let json = serde_json::to_string(&action)?;
        let parsed = serde_json::from_str::<Action>(&json)?;
        prop_assert_eq!(parsed, action);
        Ok(())
    })?;

    println!("readback=action_serde edge={variant} result_value=ok cases=1000");
    Ok(())
}
