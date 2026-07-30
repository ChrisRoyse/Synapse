use std::collections::BTreeMap;

use super::runtime::load_runtime_lens;
use super::*;
use crate::lens::process_runtime_requires_golden;

pub(crate) fn rebuild_registry(snapshot: &VaultRegistrySnapshot) -> Result<Registry> {
    let mut registry = Registry::new();
    for lens in &snapshot.lenses {
        if lens.lens_id != lens.contract.lens_id() {
            return Err(CalyxError::lens_frozen_violation(format!(
                "registry lens {} does not match frozen contract {}",
                lens.lens_id,
                lens.contract.lens_id()
            )));
        }
        let runtime: Arc<dyn Lens> = if lens
            .spec
            .as_ref()
            .is_some_and(process_runtime_requires_golden)
        {
            load_runtime_lens(lens)?
        } else {
            Arc::new(LazyPersistedLens::new(lens.clone()))
        };
        registry.register_persisted_arc(
            runtime,
            lens.contract.clone(),
            lens.spec.clone(),
            lens.determinism,
            lens.runtime_golden.clone(),
        )?;
    }
    Ok(registry)
}

struct LazyPersistedLens {
    snapshot: RegistryLensSnapshot,
    runtime: Mutex<Option<LazyRuntimeCache>>,
}

enum LazyRuntimeCache {
    Loaded(Arc<dyn Lens>),
    Failed(String),
}

impl LazyPersistedLens {
    fn new(snapshot: RegistryLensSnapshot) -> Self {
        Self {
            snapshot,
            runtime: Mutex::new(None),
        }
    }

    fn runtime(&self) -> Result<Arc<dyn Lens>> {
        let mut guard = self.runtime.lock().map_err(|_| {
            CalyxError::lens_unreachable(format!(
                "lazy persisted lens {} runtime mutex was poisoned",
                self.snapshot.lens_id
            ))
        })?;
        match guard.as_ref() {
            Some(LazyRuntimeCache::Loaded(runtime)) => return Ok(runtime.clone()),
            Some(LazyRuntimeCache::Failed(load_error)) => {
                return Err(self.error(load_error.clone()));
            }
            None => {}
        }
        match load_runtime_lens(&self.snapshot) {
            Ok(runtime) => {
                *guard = Some(LazyRuntimeCache::Loaded(runtime.clone()));
                Ok(runtime)
            }
            Err(error) => {
                let load_error = format!(
                    "{}: {} (remediation: {})",
                    error.code, error.message, error.remediation
                );
                *guard = Some(LazyRuntimeCache::Failed(load_error.clone()));
                Err(self.error(load_error))
            }
        }
    }

    fn error(&self, load_error: String) -> CalyxError {
        CalyxError::lens_unreachable(format!(
            "lens {} is persisted but its runtime failed to load in this process: {}",
            self.snapshot.lens_id, load_error
        ))
    }
}

impl Lens for LazyPersistedLens {
    fn id(&self) -> LensId {
        self.snapshot.lens_id
    }

    fn shape(&self) -> SlotShape {
        self.snapshot.contract.shape()
    }

    fn modality(&self) -> Modality {
        self.snapshot.contract.modality()
    }

    fn text_queryable(&self) -> bool {
        // Answered from the persisted spec, never by loading the runtime: this
        // is called once per slot on every text query, and a lens whose runtime
        // cannot load in this process must still be classifiable (#1896).
        self.snapshot.spec.as_ref().map_or_else(
            || self.snapshot.contract.modality() == Modality::Text,
            crate::persistence_contracts::spec_text_queryable,
        )
    }

    fn exact_value_queryable(&self) -> bool {
        // Answered from the persisted spec for the same reason
        // `text_queryable` is: this runs per slot on a query path, and a lens
        // whose runtime cannot load must still be classifiable (#1899).
        self.snapshot
            .spec
            .as_ref()
            .is_some_and(crate::persistence_contracts::spec_exact_value_queryable)
    }

    fn measure(&self, input: &Input) -> Result<SlotVector> {
        self.runtime()?.measure(input)
    }

    fn measure_batch(&self, inputs: &[Input]) -> Result<Vec<SlotVector>> {
        self.runtime()?.measure_batch(inputs)
    }

    fn measurement_group_key(&self) -> Result<Option<calyx_core::MeasurementGroupKey>> {
        self.runtime()?.measurement_group_key()
    }

    fn measure_grouped_batch(
        &self,
        requests: &[calyx_core::GroupedLensRequest],
        inputs: &[Input],
    ) -> Result<Option<BTreeMap<LensId, Vec<SlotVector>>>> {
        self.runtime()?.measure_grouped_batch(requests, inputs)
    }
}
