use calyx_core::{Input, Lens, Modality, SlotVector};
use calyx_registry::AlgorithmicLens;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let lens =
        AlgorithmicLens::syn_one_hot_index("fsv.declared_category.v1", Modality::Structured, 3);
    for (input, expected) in [
        ("0", vec![1.0, 0.0, 0.0]),
        ("1", vec![0.0, 1.0, 0.0]),
        ("2", vec![0.0, 0.0, 1.0]),
    ] {
        let measured = lens.measure(&Input::new(Modality::Structured, input.as_bytes()))?;
        println!("input={input} measured={measured:?} expected={expected:?}");
        match measured {
            SlotVector::Dense { dim: 3, data } if data == expected => {}
            other => return Err(format!("unexpected vector for {input}: {other:?}").into()),
        }
    }
    for invalid in ["", "-1", "3", "1.5", "not-a-number"] {
        let result = lens.measure(&Input::new(Modality::Structured, invalid.as_bytes()));
        println!("invalid={invalid:?} result={result:?}");
        if result.is_ok() {
            return Err(format!("invalid category index {invalid:?} was accepted").into());
        }
    }
    println!("verdict=PASS declared categories are collision-free and invalid indices fail closed");
    Ok(())
}
