# 07. Perception Subsystem

**Source files covered:**
- `crates/synapse-perception/Cargo.toml`
- `crates/synapse-perception/src/lib.rs`
- `crates/synapse-perception/src/observe.rs`
- `crates/synapse-perception/src/ocr.rs`
- `crates/synapse-perception/src/template_match.rs`
- `crates/synapse-perception/src/hud/mod.rs`
- `crates/synapse-perception/src/hud/anchor.rs`
- `crates/synapse-perception/src/hud/extractor.rs`
- `crates/synapse-perception/src/event_extensions.rs`
- `crates/synapse-perception/src/error.rs`

---

## 1. Overview

The `synapse-perception` crate fuses multiple sensor producers into a single `Observation` value and provides the pixel-level perception primitives used when the accessibility tree is insufficient. It contains four functional areas:

1. **Observation assembly** (`observe.rs`) — fuses foreground context, accessibility (a11y) elements, detected entities, HUD readings, audio, recent events, clipboard, filesystem events, and diagnostics into one `Observation`. Selects a `PerceptionMode` (a11y-only / pixel-only / hybrid / auto).
2. **OCR** (`ocr.rs`) — reads text from screen regions or caller-supplied bitmaps using the Windows `Windows.Media.Ocr` engine (WinRT). On WSL/Linux it shells out to a host Windows PowerShell script that uses the same WinRT engine.
3. **Template matching** (`template_match.rs`) — slotted HUD counter extraction via grayscale normalized cross-correlation (NCC) against a template set.
4. **HUD detection** (`hud/`) — resolving HUD crop regions against window geometry (`anchor.rs`) and extracting/parsing typed HUD field values via template-match-with-OCR-fallback (`extractor.rs`).

It also provides **event extensions** (`event_extensions.rs`) — profile-defined derived-event rules.

Dependencies (`Cargo.toml`): `image`, `regex`, `serde`, `serde_json`, `chrono`, `thiserror`, `tokio`, `tracing`, plus workspace crates `synapse-a11y`, `synapse-capture` (used for `screen_region_to_bgra_bitmap` — see [05_capture_subsystem.md](05_capture_subsystem.md)), and `synapse-core`. The `windows` crate is a Windows-only target dependency.

### Public surface (`lib.rs`)

| Re-export group | Items |
|---|---|
| error | `PerceptionError`, `PerceptionResult` |
| event_extensions | `evaluate_event_extensions`, `validate_event_extension`, `validate_event_extensions` |
| hud | `ExtractionSource`, `FieldExtraction`, `FieldExtractionRequest`, `HudAnchor`, `HudAnchorRegion`, `ResolvedHudRegion`, `extract_field`, `parse_hud_text`, `resolve_anchor_region`, `resolve_hud_region`, `resolve_hud_region_rect` |
| observe | `A11yTreeSummary`, `ObservationAssembler`, `ObservationInput`, `ObserveInclude`, `assemble`, `assemble_from_input`, `auto_mode`, `auto_mode_with_a11y`, `bounded_sensor_latency`, `is_interactable_node`, `is_known_game_process`, `parse_perception_mode` |
| ocr | `OcrProvider`, `SystemOcrProvider`, `TextRegion`, `is_empty_region`, `read_text`, `read_text_with_provider` |
| ocr (Windows only) | `read_text_from_bgra_bitmap`, `read_text_from_software_bitmap` |
| template_match | `HudTemplate`, `TemplateCounterConfig`, `TemplateCounterReading`, `TemplateSlotReading`, `extract_template_counter_from_frame`, `extract_template_counter_from_region` |

---

## 2. Observation pipeline (`observe.rs`)

### 2.1 The observe pipeline

The pipeline is driven by `ObservationAssembler::assemble(include, input)`. Steps, in order:

1. Record start `Instant`.
2. `ensure_any_sensor_available(&input)` — error if no sensor is usable.
3. Build `A11yTreeSummary::from_nodes(&input.elements)` (node count + max depth).
4. Choose `mode`: use `input.mode_override` if present, else `auto_mode_with_a11y(&foreground, &summary)`.
5. `filter_elements` (depth/interactable filter + pagination) and `filter_entities` (truncation).
6. Drop heavy diagnostic payloads when `include.diagnostics` is false (`capture_config`, `capture_runtime`, `input_backends`, `cdp` are nulled; `web_path` always retained).
7. Construct the `Observation` with a monotonic `seq` (`AtomicU64`, starts at 1, `fetch_add` with `Ordering::Relaxed`) and `at = Utc::now()`.
8. `update_size_fields(&mut observation)` is called **twice** (serializes to JSON, sets `size_bytes`, and `size_estimate_tokens = size_bytes.div_ceil(4)`).

### 2.2 Public function / type signatures

| Signature | Notes |
|---|---|
| `ObservationAssembler::new() -> Self` | `next_seq = AtomicU64::new(1)` |
| `ObservationAssembler::assemble(&self, include: ObserveInclude, input: ObservationInput) -> PerceptionResult<Observation>` | core fusion |
| `assemble(include: ObserveInclude, input: ObservationInput) -> PerceptionResult<Observation>` | fresh seq counter per call |
| `assemble_from_input(input: ObservationInput) -> PerceptionResult<Observation>` | uses `ObserveInclude::default()` |
| `auto_mode(foreground: &ForegroundContext) -> PerceptionMode` | `Hybrid` for known game process, else `A11yOnly` |
| `auto_mode_with_a11y(foreground: &ForegroundContext, summary: &A11yTreeSummary) -> PerceptionMode` | `Hybrid` if known game OR `summary.is_sparse()`, else `A11yOnly` |
| `parse_perception_mode(value: &str) -> PerceptionResult<PerceptionMode>` | see table below |
| `bounded_sensor_latency(input: BTreeMap<String, f32>) -> BTreeMap<String, f32>` | keeps only finite values for keys in `SENSOR_KEYS` |
| `is_interactable_node(node: &AccessibleNode) -> bool` | role/pattern based interactability test |
| `is_known_game_process(process_name: &str) -> bool` | hardcoded process allowlist |
| `A11yTreeSummary::from_nodes(nodes: &[AccessibleNode]) -> Self` | |
| `A11yTreeSummary::is_sparse(&self) -> bool` | `node_count < 2 || max_depth < 1` |
| `ObservationInput::new(foreground: ForegroundContext) -> Self` | all sensors default unavailable/disabled |
| `ObserveInclude::default() -> Self` / `ObserveInclude::focused_only() -> Self` | include presets |

### 2.3 Constants (`observe.rs`)

| Constant | Value |
|---|---|
| `DEFAULT_MAX_ELEMENTS` | `60` (usize) — default `max_subtree_nodes` |
| `DEFAULT_MAX_DEPTH` | `2` (u32) — default `max_subtree_depth` |
| `DEFAULT_MAX_ENTITIES` | `60` (usize) |
| `SPARSE_A11Y_NODE_THRESHOLD` | `2` |
| `SPARSE_A11Y_DEPTH_THRESHOLD` | `1` |
| `SENSOR_KEYS` | `["a11y", "capture", "detection", "ocr", "audio"]` |
| `INTERACTABLE_ROLES` | 28 normalized role strings (see below) |

`parse_perception_mode` accepted strings (input trimmed and lowercased):

| Input | Result |
|---|---|
| `a11y_only` | `PerceptionMode::A11yOnly` |
| `pixel_only` | `PerceptionMode::PixelOnly` |
| `hybrid` | `PerceptionMode::Hybrid` |
| `auto` | `PerceptionMode::Auto` |
| other | `Err(PerceptionModeInvalid { value })` |

`is_known_game_process` allowlist (lowercased match): `eldenring.exe`, `fortniteclient-win64-shipping.exe`, `game.exe`, `minecraft.exe`, `overwatch.exe`, `starfield.exe`, `valorant.exe`.

### 2.4 `is_interactable_node` logic

- Disabled node (`!node.enabled`) → `false`.
- Role normalized (whitespace, `_`, `-` stripped, lowercased) and tested against `INTERACTABLE_ROLES`: `button, checkbox, combobox, edit, gridcell, hyperlink, link, listbox, listitem, menuitem, menuitemcheckbox, menuitemradio, option, radio, radiobutton, searchbox, slider, spinbutton, spinner, splitbutton, switch, tab, tabitem, textarea, textbox, textfield, togglebutton, treeitem`.
- Role `document` → interactable only if it exposes `UiaPattern::Value` or `UiaPattern::Text`.
- Structural roles (`scrollbar, progressbar, titlebar, window, pane, group, generic`) → `false`.
- Otherwise interactable if any of patterns `Invoke, Toggle, Value, SelectionItem, ExpandCollapse, RangeValue` is present.

### 2.5 What an "observation" contains

The assembler produces a `synapse_core::Observation` (fields populated here):

| Field | Source / behavior |
|---|---|
| `seq` | monotonic counter |
| `at` | `Utc::now()` |
| `mode` | selected `PerceptionMode` |
| `foreground` | `input.foreground` |
| `perceived_text_notice` | `None` |
| `suspected_injection` | empty |
| `focused` | `input.focused` when `include.focused` |
| `elements` | filtered `Vec<AccessibleNode>` |
| `entities` | filtered `Vec<DetectedEntity>` |
| `hud` | `input.hud` (`HudReadings`) when `include.hud`, else default |
| `audio` | `input.audio` when `include.audio`, else default |
| `recent_events` | when `include.events` |
| `clipboard_summary` | when `include.clipboard` |
| `fs_recent` | when `include.fs` |
| `diagnostics` | `ObservationDiagnostics` (see below) |

`ObservationDiagnostics` fields set here: `assembled_in_ms`, `sensor_latency_ms` (bounded), `a11y_enabled` (= focused\|elements\|events), `pixel_enabled` (= entities\|hud), `audio_enabled`, the four `*_status` sensor statuses, `is_minimized`, `capture_config`, `capture_runtime`, `input_backends`, `cdp`, `web_path`, `elements_truncated`, `elements_page`, `entities_truncated`, `size_bytes`, `size_estimate_tokens`.

### 2.6 `ObserveInclude` fields

| Field | Type | `default()` | `focused_only()` |
|---|---|---|---|
| `focused` | bool | `true` | `true` |
| `elements` | bool | `true` | `false` |
| `entities` | bool | `true` | `false` |
| `hud` | bool | `true` | `false` |
| `audio` | bool | `false` | `false` |
| `events` | bool | `true` | `false` |
| `clipboard` | bool | `false` | `false` |
| `fs` | bool | `false` | `false` |
| `diagnostics` | bool | `true` | `true` |
| `interactable_only` | bool | `false` | `false` |
| `max_subtree_depth` | u32 | `2` | `2` |
| `max_subtree_nodes` | usize | `60` | `60` |
| `element_offset` | usize | `0` | `0` |
| `max_entities` | usize | `60` | `60` |

### 2.7 Element filtering / pagination (`filter_elements`)

- If `!include.elements`: returns empty vec; `truncated = !elements.is_empty()`; emits an `ObservationElementsPage { total, offset: 0, limit: 0, next_offset: Some(0) }` when truncated.
- If `interactable_only`: `elements.retain(is_interactable_node)`, structural depth cut **skipped** (`depth_truncated = false`) — per #882, to surface deep web form fields.
- Else: retain nodes with `depth <= max_subtree_depth`; `depth_truncated` true if any removed.
- Pagination: `offset = min(element_offset, total)`, `limit = max_subtree_nodes`, `next_offset = offset+limit if < total`. Truncated flag = `depth_truncated || paged`.

`filter_entities`: truncates to `max_entities`; truncated flag set if length exceeded.

`ensure_any_sensor_available` returns `Ok` if any of `a11y/capture/detection/audio` status is `Healthy | DegradedLatency{..} | DegradedSensorFailed{..}`; otherwise `ObserveNoPerceptionAvailable`.

### 2.8 `ObservationInput` fields

`foreground: ForegroundContext`, `is_minimized: bool`, `focused: Option<FocusedElement>`, `elements: Vec<AccessibleNode>`, `entities: Vec<DetectedEntity>`, `hud: HudReadings`, `audio: AudioContext`, `recent_events: Vec<EventSummary>`, `clipboard_summary: Option<ClipboardSummary>`, `fs_recent: Vec<FsEvent>`, `sensor_latency_ms: BTreeMap<String, f32>`, `a11y_status / capture_status / detection_status / audio_status: SensorStatus`, `mode_override: Option<PerceptionMode>`, `capture_config: Option<ObservationCaptureConfig>`, `capture_runtime: Option<CaptureRuntimeReadback>`, `input_backends: Option<InputBackendDiagnostics>`, `cdp: Option<CdpDiagnostics>`, `web_path: Option<WebPerceptionPath>`.

`ObservationInput::new` defaults: `a11y_status` and `capture_status` = `Unavailable`; `detection_status` and `audio_status` = `Disabled`.

---

## 3. OCR (`ocr.rs`)

### 3.1 OCR engine

**Windows:** `Windows.Media.Ocr::OcrEngine` (WinRT), created via `OcrEngine::TryCreateFromUserProfileLanguages()`. The engine is cached in a `OnceLock<Result<OcrEngine, String>>`. Recognition runs synchronously by `.join()`-ing the `IAsyncOperation` returned by `RecognizeAsync`.

**WSL/Linux** (`cfg(all(unix, not(target_os = "macos")))`): shells out to a host Windows PowerShell process (candidates: `/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe`, `/mnt/c/Program Files/PowerShell/7/pwsh.exe`, `powershell.exe`, `pwsh.exe`). The embedded script captures the screen region with `System.Drawing` (`Graphics.CopyFromScreen`), saves a PNG to `%TEMP%\synapse-ocr`, then runs the **same** `Windows.Media.Ocr.OcrEngine` via WinRT reflection and emits per-word JSON (`confidence` is hardcoded to `1.0`).

**Other platforms:** `read_text` returns `OcrBackendUnavailable` ("OCR backend is implemented on Windows and WSL/Linux with Windows host OCR").

No tesseract is used.

### 3.2 Public types & functions

| Item | Signature |
|---|---|
| `TextRegion` | struct `{ text: String, bbox: Rect, confidence: f32 }` (serde, `deny_unknown_fields`) |
| `OcrProvider` (trait) | `fn read_text(&self, region: Rect) -> PerceptionResult<Vec<TextRegion>>` |
| `SystemOcrProvider` | unit struct; `OcrProvider::read_text` delegates to free `read_text` |
| `read_text` | `fn read_text(region: Rect) -> PerceptionResult<Vec<TextRegion>>` |
| `read_text_with_provider` | `fn read_text_with_provider(provider: &dyn OcrProvider, region: Rect) -> PerceptionResult<Vec<TextRegion>>` |
| `is_empty_region` | `const fn is_empty_region(region: Rect) -> bool` → `region.w <= 0 || region.h <= 0` |
| `read_text_from_software_bitmap` (Windows) | `fn read_text_from_software_bitmap(region: Rect, bitmap: &windows::Graphics::Imaging::SoftwareBitmap) -> PerceptionResult<Vec<TextRegion>>` |
| `read_text_from_bgra_bitmap` (Windows) | `fn read_text_from_bgra_bitmap(region: Rect, width: u32, height: u32, bytes: &[u8]) -> PerceptionResult<Vec<TextRegion>>` |

- `read_text(region)`: empty region → `OcrNoText`; else delegates to platform `read_text`. On Windows it calls `synapse_capture::screen_region_to_bgra_bitmap(region)` (see [05_capture_subsystem.md](05_capture_subsystem.md)) then `read_text_from_bgra_bitmap`.
- `read_text_with_provider`: empty region → `OcrNoText`; empty provider output → `OcrNoText`.

### 3.3 Output structure & coordinate mapping

Each recognized word becomes a `TextRegion`. `confidence` is always `1.0` (WinRT OCR does not expose per-word confidence here). The word `BoundingRect` (engine-local, in the recognized bitmap's pixel space) is mapped to screen coordinates:

```
bbox.x = region.x + round(rect.X / scale)   // saturating_add
bbox.y = region.y + round(rect.Y / scale)   // saturating_add
bbox.w = max(round(rect.Width  / scale), 1)
bbox.h = max(round(rect.Height / scale), 1)
```

where `scale` is the upscale factor applied before recognition (1 for `read_text_from_software_bitmap`).

### 3.4 Windows BGRA recognition pipeline & constants

`read_text_from_bgra_bitmap` runs a **primary** recognition over the whole region and may run a **sparse fallback**; `select_ocr_candidate` picks the higher-scoring result (`ocr_candidate_score = words.len()*1000 + total_char_count`), or the fallback if primary errored.

| Constant | Value | Purpose |
|---|---|---|
| `OCR_MIN_RECOGNITION_HEIGHT_PX` | `64` | minimum row height before upscaling kicks in |
| `OCR_MAX_UPSCALE` | `6` | upscale clamp |
| `OCR_SPARSE_TILE_MIN_WIDTH_PX` | `640` | width above which sparse-strip handling is considered |
| `OCR_SPARSE_TILE_TARGET_WIDTH_PX` | `480` | target tile width |
| `OCR_SPARSE_TILE_OVERLAP_PX` | `96` | tile overlap |
| `OCR_SPARSE_ASPECT_RATIO_NUMERATOR` | `6` | width ≥ height×6 ⇒ "sparse horizontal strip" |
| `OCR_BACKGROUND_DIFF_THRESHOLD` | `90` (u16) | summed B+G+R abs-diff above which a pixel is "content" |
| `OCR_CONTENT_PADDING_PX` | `6` | padding around detected content bounds |

- **Upscaling** (`recognition_upscale`): if height `>= 64` or `0`, scale = 1. Else `height_scale = ceil(64/height).clamp(1,6)`, bounded by WinRT `MaxImageDimension` (`dimension_scale`); final scale = `min`. Upscale uses `image::imageops::resize(..., FilterType::Nearest)`.
- **Bitmap creation**: `SoftwareBitmap::CreateCopyWithAlphaFromBuffer(..., BitmapPixelFormat::Bgra8, w, h, BitmapAlphaMode::Ignore)` via a `DataWriter`. `validate_bgra_len` requires `bytes.len() == width*height*4` and non-zero dimensions.
- **Content-bounds trim** (`content_bounds_for_bgra`): computes background color as the average of the 4 corner BGR pixels, scans all pixels, and crops to the bounding box of differing pixels plus padding — but only if the trimmed area is `< 90%` of the original area.
- **Tiling** (`sparse_ocr_tiles`): splits wide strips into overlapping tiles (target width 480, overlap 96, full height). Used when too wide for the engine or when width ≥ 640 and width ≥ height×6.
- **Dedupe** (`dedupe_text_regions`): sorts by (y, x, text); merges regions whose bbox overlap ratio (`overlap_area / min_area`) `>= 0.45`, keeping the higher-scoring word.

---

## 4. Template matching (`template_match.rs`)

### 4.1 Algorithm

**Normalized cross-correlation (NCC)** over grayscale (`GrayImage`/luma8) pixels. For each slot the template is slid over every valid `(x, y)` offset; the score at each offset is:

```
NCC = Σ (T - mean_T)(S - mean_S)  /  ( sqrt(Σ (T-mean_T)²) · sqrt(Σ (S-mean_S)²) )
```

clamped to `[-1.0, 1.0]`. `mean_T`/`mean_S` are the template and the underlying slot-window means. If the denominator `<= f64::EPSILON`, the offset scores `None`. The best (highest) score per template, and the best template per slot, win (`best_slot_match` / `best_template_location`).

### 4.2 Slotted counter extraction

`extract_template_counter_from_region`:
1. `validate_config` (see below).
2. For each slot `index` in `0..config.slots`: compute slot x/width via integer-scaled edges `scaled_slot_edge = floor(region_w * index / slots)` so widths distribute evenly across `region_w`; crop the full-height slot column; find best template match.
3. If `best.confidence < config.min_confidence` → `HudExtractionFailed`.
4. Accumulate `value += best.value` (checked add; error on overflow); error if running total `> config.max_value`.
5. Aggregate `confidence = confidence_sum / slots`. Records per-slot `TemplateSlotReading`.

Slot reading `x` is `slot_x + best.x` (screen/region-relative within the cropped region), `y` is `best.y`.

### 4.3 Public functions & types

| Item | Signature |
|---|---|
| `HudTemplate` | struct `{ label: String, value: u32, image: GrayImage }` |
| `HudTemplate::from_gray` | `fn from_gray(label: impl Into<String>, value: u32, image: GrayImage) -> PerceptionResult<Self>` (errors on zero dims) |
| `HudTemplate::load` | `fn load(label: impl Into<String>, value: u32, path: impl AsRef<Path>) -> PerceptionResult<Self>` (opens via `image::open`, converts to `to_luma8()`) |
| `TemplateCounterConfig` | struct `{ slots: u32, min_confidence: f64, max_value: u32 }` |
| `TemplateCounterReading` | struct `{ value: u32, confidence: f64, elapsed_ms: f64, slots: Vec<TemplateSlotReading> }` |
| `TemplateSlotReading` | struct `{ index: u32, label: String, value: u32, confidence: f64, x: u32, y: u32 }` |
| `extract_template_counter_from_frame` | `fn (frame: &DynamicImage, region: Rect, templates: &[HudTemplate], config: TemplateCounterConfig) -> PerceptionResult<TemplateCounterReading>` |
| `extract_template_counter_from_region` | `fn (region: &GrayImage, templates: &[HudTemplate], config: TemplateCounterConfig) -> PerceptionResult<TemplateCounterReading>` |

### 4.4 Constants & defaults

| Constant / default | Value |
|---|---|
| `MINECRAFT_STATUS_SLOTS` | `10` |
| `MINECRAFT_STATUS_MAX_VALUE` | `20` |
| `DEFAULT_MIN_TEMPLATE_CONFIDENCE` | `0.85` |
| `TemplateCounterConfig::default()` | `{ slots: 10, min_confidence: 0.85, max_value: 20 }` |

### 4.5 Validation (`validate_config`) — all return `HudExtractionFailed`

- region has zero width/height; empty template set; `slots == 0`.
- `min_confidence` not in `0.0..=1.0`.
- `region_w < slots` (cannot split into non-empty slots).
- any template with zero dims, or `template_w > first_slot_w` or `template_h > region_h`.

`extract_template_counter_from_frame` additionally requires `region` in-frame and positive (`crop_frame_region`): rejects negative origin / non-positive size, and `x+w` / `y+h` exceeding frame dimensions or overflowing.

---

## 5. HUD detection (`hud/`)

### 5.1 Region anchoring (`hud/anchor.rs`)

`HudAnchor` enum (serde `snake_case`): `None`, `TopLeft`, `TopRight`, `BottomLeft`, `BottomCenter`, `BottomRight`, `Center`. `From<WindowEdge>` maps the six edge variants (no `None`).

| Type | Fields |
|---|---|
| `HudAnchorRegion` | `anchor: HudAnchor`, `x_offset: i32`, `y_offset: i32`, `w: i32`, `h: i32` (serde, `deny_unknown_fields`) |
| `ResolvedHudRegion` | `left: i32`, `top: i32`, `right: i32`, `bottom: i32` (serde, `deny_unknown_fields`) |

`ResolvedHudRegion` methods: `width() = right-left`, `height() = bottom-top`, `rect() -> Rect { x:left, y:top, w:width, h:height }`, `as_ltrb() -> (i32,i32,i32,i32)`.

| Function | Signature |
|---|---|
| `resolve_hud_region` | `fn (region: &HudRegion, window_client: Rect) -> PerceptionResult<ResolvedHudRegion>` |
| `resolve_hud_region_rect` | `fn (region: &HudRegion, window_client: Rect) -> PerceptionResult<Rect>` |
| `resolve_anchor_region` | `fn (region: HudAnchorRegion, window_client: Rect) -> PerceptionResult<ResolvedHudRegion>` |

`resolve_hud_region` dispatches on `synapse_core::HudRegion`:
- `Absolute { x, y, w, h }` → `resolve_absolute` (validate positive dims, build region).
- `FractionOfWindow { x, y, w, h }` (f32) → validate window positive + fractions finite/in unit square (`x>=0,y>=0,w>0,h>0,x+w<=1,y+h<=1`); each fraction rounded against window w/h (`round(size*fraction)`); offsets added to window origin.
- `AnchoredToEdge { edge, x_offset, y_offset, w, h }` → delegates to `resolve_anchor_region` (edge→`HudAnchor`).

`resolve_anchor_region`: validates dims positive; `HudAnchor::None` treats offsets as **absolute** screen coords (ignores window). Otherwise validates window positive, computes anchor point, adds offsets (checked), builds region. Anchor points: `TopLeft=(x,y)`, `TopRight=(x+w, y)`, `BottomLeft=(x, y+h)`, `BottomCenter=(x+w/2, y+h)`, `BottomRight=(x+w, y+h)`, `Center=(x+w/2, y+h/2)`. All additions use `checked_add` → `HudExtractionFailed` on overflow.

### 5.2 Field extraction (`hud/extractor.rs`)

| Type | Definition |
|---|---|
| `ExtractionSource` (enum) | `TemplateMatch`, `Ocr`, `OcrFallback` |
| `FieldExtraction` | `{ field_name: String, reading: HudReading, source: ExtractionSource, template_reading: Option<TemplateCounterReading>, ocr_text: Option<String>, elapsed_ms: f64 }` |
| `FieldExtractionRequest<'a>` | `{ field: &'a HudFieldSpec, screen_region: Rect, region_image: &'a GrayImage, templates: &'a [HudTemplate], ocr_provider: &'a dyn OcrProvider, stale_ms: u32 }` |

| Function | Signature |
|---|---|
| `extract_field` | `fn extract_field(request: &FieldExtractionRequest<'_>) -> PerceptionResult<FieldExtraction>` |
| `parse_hud_text` | `fn parse_hud_text(parser: &HudParser, raw_text: &str) -> PerceptionResult<HudValue>` |

**`extract_field`** validates `field.confidence_threshold` (finite, `0.0..=1.0`), then dispatches on `field.extractor` (`synapse_core::HudExtractor`):
- `TemplateMatch { .. }` → `extract_template_field`: runs the slotted counter with a **permissive** config (`min_confidence = 0.0`, otherwise default). If aggregate `confidence >= field.confidence_threshold`, returns `TemplateMatch` source with `HudValue::Number(value)`. Otherwise (or on template error) falls back to OCR over `screen_region` (`OcrFallback`). If template errored *and* OCR fallback errored, returns a combined `HudExtractionFailed`.
- `WinrtOcr` → `extract_ocr_field` with source `Ocr`.
- `Crnn { model_id }` → `HudExtractionFailed` ("not wired").
- `ColorRatio { .. }` → `HudExtractionFailed` ("not handled by the OCR fallback extractor").

**OCR field path** (`ocr_reading`): calls `read_text_with_provider`; on `OcrNoText` uses parser no-text default (only `BoundedInteger { default_on_no_text: Some(..) }`), else error. Joins region texts (space-separated, trimmed). Confidence = `min_word_confidence` (min over finite, `[0,1]`-clamped word confidences). If `confidence < field.confidence_threshold` → error. Parses via `parse_hud_text`.

**`parse_hud_text`** dispatch on `synapse_core::HudParser` (text trimmed; empty → error):

| Parser | Behavior |
|---|---|
| `Number` | first `NUMBER_PATTERN` match parsed as f64 → `HudValue::Number` |
| `BoundedInteger { min, max, .. }` | parse number, require integer within `min..=max` (and u32 range) → `Number` |
| `FractionNumerator` | `num` group of `FRACTION_PATTERN` → `Number` |
| `FractionDenominator` | `den` group of `FRACTION_PATTERN` → `Number` |
| `Regex { pattern, group }` | compile profile regex; take capture `group` trimmed; parse f64 → `Number`, else `HudValue::Text` |
| `Enum { mapping }` | exact map lookup → `HudValue::Enum`, else error |

Regex constants:
```
NUMBER_PATTERN   = r"[-+]?(?:\d+(?:\.\d*)?|\.\d+)"
FRACTION_PATTERN = r"(?P<num>[-+]?(?:\d+(?:\.\d*)?|\.\d+))\s*/\s*(?P<den>[-+]?(?:\d+(?:\.\d*)?|\.\d+))"
```

Template-match confidence is mapped to `f32` via `confidence_to_f32` (clamp `[0,1]`).

---

## 6. Event extensions (`event_extensions.rs`)

Profile-defined rules (`synapse_core::EventExtension`) that emit derived events when a real `Event` matches a filter.

| Function | Signature |
|---|---|
| `validate_event_extension` | `fn (extension: &EventExtension) -> PerceptionResult<()>` |
| `validate_event_extensions` | `fn (extensions: &[EventExtension]) -> PerceptionResult<()>` |
| `evaluate_event_extensions` | `fn (extensions: &[EventExtension], trigger: &Event, first_seq: u64) -> PerceptionResult<Vec<Event>>` |

**`validate_event_extension`** returns `EventExtensionInvalid` when: `name` empty; `emits_kind` empty; `from_filter.validate()` fails; or `from_filter.is_trivially_always_true()`.

**`evaluate_event_extensions`** validates all extensions first, then for each extension whose `from_filter.matches(trigger)` is true, emits a new `Event`:
- `seq = first_seq + (index among matched events)` (checked; overflow → `EventExtensionInvalid`).
- `at = Utc::now()`, `source = EventSource::Perception`, `kind = extension.emits_kind`.
- `data` JSON: `{ extension_name, trigger_seq, trigger_source, trigger_kind, trigger_data }`.
- `correlations = [ EventRef { seq: trigger.seq, relation: "event_extension_trigger" } ]`.

---

## 7. Error types (`error.rs`)

`pub type PerceptionResult<T> = Result<T, PerceptionError>;`

| Variant | Display | `code()` (`synapse_core::error_codes`) |
|---|---|---|
| `OcrNoText { region: Rect }` | `OCR produced no text for region {region:?}` | `OCR_NO_TEXT` |
| `OcrBackendUnavailable { detail: String }` | `OCR backend is unavailable: {detail}` | `OCR_BACKEND_UNAVAILABLE` |
| `ObserveNoPerceptionAvailable { detail: String }` | `no perception source is available: {detail}` | `OBSERVE_NO_PERCEPTION_AVAILABLE` |
| `ObserveInternal { detail: String }` | `observe failed internally: {detail}` | `OBSERVE_INTERNAL` |
| `HudExtractionFailed { detail: String }` | `HUD extraction failed: {detail}` | `HUD_EXTRACTION_FAILED` |
| `EventExtensionInvalid { name: String, detail: String }` | `event extension {name:?} is invalid: {detail}` | `PROFILE_PARSE_ERROR` |
| `PerceptionModeInvalid { value: String }` | `invalid perception mode: {value}` | `PERCEPTION_MODE_INVALID` |

`PerceptionError` derives `Debug` and `thiserror::Error`. `code(&self) -> &'static str` maps each variant to a stable code string. Note `EventExtensionInvalid` maps to `PROFILE_PARSE_ERROR` (not a dedicated code).

---

## Cross-references

- Screen capture (`screen_region_to_bgra_bitmap`, BGRA frames): See [05_capture_subsystem.md](05_capture_subsystem.md).
- `Observation`, `AccessibleNode`, `HudReadings`, `HudExtractor`, `HudParser`, `HudValue`, `HudRegion`, `WindowEdge`, `Event`/`EventExtension`, `PerceptionMode`, `error_codes` are defined in `synapse-core`.
