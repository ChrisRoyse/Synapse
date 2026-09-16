# 11. Profiles Subsystem

**Source files covered:**

- `crates/synapse-profiles/src/lib.rs` — crate root and public re-exports
- `crates/synapse-profiles/src/toml_format.rs` — raw TOML deserialization structs (`RawProfile` and children)
- `crates/synapse-profiles/src/parser.rs` — parsing entry points, validation, defaults, `LoadedProfile`, `ScreenBounds`
- `crates/synapse-profiles/src/resolver.rs` — foreground-window matching (`ForegroundWindow`, `ProfileMatchResolution`, `resolve_active_profile`)
- `crates/synapse-profiles/src/watcher.rs` — hot-reload runtime (`ProfileRuntime`, `ProfileStatus`, `ForegroundProfileTransition`)
- `crates/synapse-profiles/src/package/mod.rs` — package manifest parse entry points
- `crates/synapse-profiles/src/package/types.rs` — package manifest data model
- `crates/synapse-profiles/src/package/validation.rs` — fail-closed manifest validation
- `crates/synapse-profiles/src/package/digest.rs` — manifest digest and signature payload
- `crates/synapse-profiles/src/error.rs` — `ProfileError`, `ProfileLoadError`
- `crates/synapse-profiles/Cargo.toml`
- Bundled profiles: `crates/synapse-profiles/profiles/*.toml`

See [03_configuration.md](03_configuration.md) for how the profiles directory and screen bounds are configured at the application layer.

---

## 1. Overview

A **profile** is a TOML document describing how Synapse perceives and acts on one foreground application or game. Each profile declares:

- which windows it applies to (`[[matches]]`),
- a `use_scope` (the safety/governance scope of automation),
- a perception `mode`, screen-capture settings, object detection settings, OCR backend, and HUD field specs,
- a keymap (named actions to key chords),
- input backend defaults, free-form metadata, and event extensions.

Profiles are stored as individual `.toml` files in a profiles directory. At runtime, `ProfileRuntime` loads every `.toml` in that directory, watches it for changes (hot reload), and resolves which profile is active for the current foreground window.

A separate concept, the **profile package** (`crates/synapse-profiles/src/package/`), is the distribution/bundling manifest format used by the profile registry. A package manifest is a different TOML document that points at a profile TOML file plus assets, with provenance, permissions, hashes, and signatures. Packages are validated fail-closed but are not themselves loaded by `ProfileRuntime`.

The crate's TOML deserialization is built on `serde` + `toml`. File watching uses the `notify` crate. Digests use `sha2` (SHA-256). Timestamps are validated as RFC3339 via `chrono`. (`crates/synapse-profiles/Cargo.toml`)

The on-disk profile is deserialized into `RawProfile` (`crates/synapse-profiles/src/toml_format.rs`) and then converted into the `synapse_core::Profile` domain type wrapped in `LoadedProfile`. Profile-specific enums (`ProfileUseScope`, `PerceptionMode`, `ProfileCaptureTarget`, `Backend`, `OcrBackend`, `HudRegion`, `HudExtractor`, `HudParser`, `ProfileMatch`, `EventExtension`) live in `synapse-core`.

All raw structs use `#[serde(deny_unknown_fields)]`, so unknown TOML keys are a parse error.

---

## 2. Profile Data Model

### 2.1 Top-level profile keys (`RawProfile`)

Source: `crates/synapse-profiles/src/toml_format.rs` lines 21–53.

| Key | TOML type | Required | Default | Notes |
|-----|-----------|----------|---------|-------|
| `id` | string | yes | — | `ProfileId`; used as the map key in the runtime. |
| `label` | string | yes | — | Human-readable name. |
| `schema_version` | integer (u32) | yes | — | Must equal `synapse_core::PROFILE_SCHEMA_VERSION`, else `VersionIncompatible`. Bundled profiles use `2`. Stored on `Profile.version` as a string. |
| `use_scope` | string | yes | — | One of the use-scope values in §2.2. |
| `mode` | string | no | `"a11y_only"` | Perception mode, §2.3. |
| `mouse_velocity_profile_default` | string | conditional | — | Must be `"natural"` (case-insensitive). Mutually exclusive with `mouse_curve_default`; exactly one of the two must be present. |
| `mouse_curve_default` | string | conditional | — | Deprecated alias for the above; must be `"natural"`. |
| `keyboard_dynamics_default` | string | yes | — | Must be `"natural"` (case-insensitive). |
| `matches` | array of tables `[[matches]]` | yes (non-empty) | — | At least one entry required, §2.7. |
| `capture` | table `[capture]` | no | defaults | §2.4. |
| `detection` | table `[detection]` | no | defaults | §2.5. |
| `ocr` | table `[ocr]` | no | defaults | §2.6. |
| `hud` | array of tables `[[hud]]` | no | `[]` | HUD field specs, §2.8. |
| `keymap` | table (string→string) | no | `{}` | Action name → key chord; validated §2.9. |
| `backends` | table `[backends]` | no | defaults | §2.10. |
| `metadata` | table (string→string) | no | `{}` | Free-form; preserved verbatim. |
| `event_extensions` | array of `EventExtension` | no | `[]` | Validated §2.11. |

The conversion (`RawProfile::into_loaded`) produces `Profile` plus a `ProfileDefaults` struct holding the two `*_default` strings.

`ProfileDefaults` (`crates/synapse-profiles/src/parser.rs` lines 24–28):

| Field | Type |
|-------|------|
| `mouse_velocity_profile_default` | `String` (always `"natural"`) |
| `keyboard_dynamics_default` | `String` (always `"natural"`) |

`LoadedProfile` (`crates/synapse-profiles/src/parser.rs` lines 30–37):

| Field | Type | Notes |
|-------|------|-------|
| `profile` | `synapse_core::Profile` | The converted domain profile. |
| `schema_version` | `u32` | Raw schema version. |
| `defaults` | `ProfileDefaults` | See above. |
| `source_path` | `PathBuf` | File the profile was loaded from. |
| `modified` | `SystemTime` | File mtime (falls back to `UNIX_EPOCH` if unavailable). |

### 2.2 `use_scope` values (`parse_use_scope`, parser.rs 146–158)

| TOML string | `ProfileUseScope` variant |
|-------------|---------------------------|
| `productivity` | `Productivity` |
| `single_player` | `SinglePlayer` |
| `operator_owned_test` | `OperatorOwnedTest` |
| `sanctioned_research` | `SanctionedResearch` |
| `unknown` | `Unknown` |

Any other value → `ProfileError::Parse`.

### 2.3 `mode` values (`parse_mode`, parser.rs 160–171)

| TOML string | `PerceptionMode` variant |
|-------------|--------------------------|
| `a11y_only` | `A11yOnly` (default) |
| `pixel_only` | `PixelOnly` |
| `hybrid` | `Hybrid` |
| `auto` | `Auto` |

### 2.4 `[capture]` (`RawCapture`, toml_format.rs 174–203)

| Key | Type | Default | Notes |
|-----|------|---------|-------|
| `target` | string | `"foreground_window"` | `foreground_window` → `ForegroundWindow`, `primary_monitor` → `PrimaryMonitor` (`parse_capture_target`). |
| `min_update_interval_ms` | u32 | `50` (`DEFAULT_CAPTURE_INTERVAL_MS`) | Minimum capture interval. |
| `cursor_visible` | bool | `true` | Whether the cursor is captured/visible. |

### 2.5 `[detection]` (`RawDetection`, toml_format.rs 205–238)

| Key | Type | Default | Notes |
|-----|------|---------|-------|
| `model_id` | optional string | `None` | Object-detection model id. |
| `classes_of_interest` | array of string | `[]` | Detection classes to keep. |
| `confidence_threshold` | f32 | `0.5` (`DEFAULT_CONFIDENCE_THRESHOLD`) | Not range-validated for detection. |
| `max_detections` | u32 | `32` (`DEFAULT_MAX_DETECTIONS`) | |

### 2.6 `[ocr]` (`RawOcr`, toml_format.rs 240–263)

| Key | Type | Default | Notes |
|-----|------|---------|-------|
| `default_backend` | string | `"auto"` | `winrt`→`Winrt`, `crnn`→`Crnn`, `auto`→`Auto` (`parse_ocr_backend`). |

`ProfileOcr.regions` and `ProfileOcr.parser_config` are always set empty by the parser (not configurable from this TOML — `into_ocr`, toml_format.rs 256–262). "Not determined from source" how those are populated elsewhere.

### 2.7 `[[matches]]` (`RawProfileMatch`, toml_format.rs 147–172)

| Key | Type | Default | Notes |
|-----|------|---------|-------|
| `exe` | optional string | `None` | Executable name (matched case-insensitively at resolve time). |
| `title_regex` | optional string | `None` | Regex against window title; compiled and validated. |
| `steam_appid` | optional u32 | `None` | Exact Steam AppID. |
| `window_class` | optional string | `None` | Win32 window class (matched case-insensitively). |
| `process_args` | array of string | `[]` | Carried into `ProfileMatch`; not used by the resolver. |

Validation (`validate_match`, parser.rs 289–310):
- Each match entry must define at least one of `exe`, `title_regex`, `steam_appid`, `window_class` (note: `process_args` does **not** count). Otherwise `ProfileError::Parse`.
- `title_regex`, if present, must compile as a `regex::Regex`.

### 2.8 `[[hud]]` (`RawHudField`, toml_format.rs 265–326)

| Key | Type | Default | Notes |
|-----|------|---------|-------|
| `name` | string | — (required) | HUD field name. |
| `region` | `HudRegion` (table) | `None` | If absent, reconstructed from flat `region_kind`/`x`/`y`/`w`/`h`. |
| `extractor` | `HudExtractor` (table) | `HudExtractor::WinrtOcr` | |
| `parser` | `HudParser` (table) | `HudParser::Number` | |
| `confidence_threshold` | f32 | `default_hud_confidence_threshold()` | Must be finite and in `0.0..=1.0`, else `Parse`. |
| `region_kind` | optional string | `"absolute"` | Only `"absolute"` supported in flat form; anything else → `Parse`. |
| `x`,`y`,`w`,`h` | optional i32 | `None` | Required when building a flat absolute region; missing → `Parse`. |

`HudRegion` variants and their validation (`validate_hud_region`, parser.rs 312–354), checked against `ScreenBounds`:

| Variant | TOML `kind` | Constraints |
|---------|-------------|-------------|
| `Absolute { x, y, w, h }` | `absolute` | `x>=0`, `y>=0`, `w>0`, `h>0`, `x+w <= bounds.width`, `y+h <= bounds.height`. |
| `FractionOfWindow { x, y, w, h }` | `fraction_of_window` | floats `>=0`, `w>0`,`h>0`, `x+w <= 1.0`, `y+h <= 1.0`. |
| `AnchoredToEdge { w, h, .. }` | (anchored) | `w>0`,`h>0`, `w <= bounds.width`, `h <= bounds.height`. |

`HudParser::BoundedInteger { min, max, default_on_no_text }` validation (`validate_hud_parser`, toml_format.rs 344–369): `min <= max`, and `default_on_no_text` (if set) must be within `min..=max`.

### 2.9 `[keymap]` validation (`validate_keymap` / `canonical_key_name`, parser.rs 212–287)

- Alias (left side) must be non-empty after trim, else `KeymapInvalid`.
- The binding is split on `+`. Each token is canonicalized to a known key name; an unknown token → `KeymapInvalid` (`unsupported key`). A duplicate key within one binding → `KeymapInvalid` (`duplicate key`).
- Recognized single tokens after canonicalization: single ASCII alphanumerics; `f1`..`f24`; and the named set `` ` `` `alt backspace ctrl delete down end enter esc home insert left lmb mmb pagedown pageup right rmb lshift rshift shift space super tab up x1 x2`.
- Aliases applied during canonicalization include: `control`→`ctrl`, `escape`→`esc`, `return`→`enter`, `backtick`/`grave`/`graveaccent`/`keyboardgraveaccent`→`` ` ``, `leftshift`→`lshift`, `rightshift`→`rshift`, mouse aliases (`leftmouse`/`lmb`/`mouse_left`→`lmb`, etc.), arrow aliases (`arrowup`→`up`, …), `win`/`windows`/`meta`→`super`, `pgup`→`pageup`, `pgdn`→`pagedown`.

### 2.10 `[backends]` (`RawBackends`, toml_format.rs 426–459)

| Key | Type | Default | Notes |
|-----|------|---------|-------|
| `default` | string | `"auto"` | Has serde `alias = "default_backend"`. |
| `keyboard_default` | string | `"auto"` | |
| `mouse_default` | string | `"auto"` | |
| `pad_default` | string | `"auto"` | |

Backend strings (`parse_backend`, parser.rs 187–198): `software`→`Software`, `vigem`→`Vigem`, `hardware`→`Hardware`, `auto`→`Auto`; others → `Parse`.

### 2.11 `[[event_extensions]]` validation (`validate_event_extensions`, toml_format.rs 383–424)

Each `EventExtension` (deserialized by `synapse_core`):
- `name` must be non-empty (after trim).
- `emits_kind` must be non-empty (after trim).
- `from_filter` must pass `EventExtension::from_filter.validate()`.
- `from_filter` must not be trivially always true (`is_trivially_always_true()` → reject).

See the Luanti profile (`crates/synapse-profiles/profiles/luanti.minetest.toml` lines 113–177) for filter syntax (`op`, `args`, `source`, `kind`, `data`/`path`/`predicate`).

### 2.12 `ScreenBounds` (parser.rs 39–52)

| Field | Type | Default |
|-------|------|---------|
| `width` | i32 | `3840` (`DEFAULT_SCREEN_WIDTH`) |
| `height` | i32 | `2160` (`DEFAULT_SCREEN_HEIGHT`) |

Used to validate absolute/anchored HUD regions. `parse_profile_file` uses the default; `parse_profile_file_with_bounds` / `ProfileRuntime::spawn_with_screen_bounds` accept caller-supplied bounds.

### 2.13 Parse entry points (parser.rs)

| Function | Signature | Behavior |
|----------|-----------|----------|
| `parse_profile_file(path)` | `-> Result<LoadedProfile, ProfileError>` | Reads file, uses default 3840×2160 bounds. |
| `parse_profile_file_with_bounds(path, bounds)` | same | Reads file + mtime, delegates to bytes parser. |
| `parse_profile_bytes(path, bytes, modified, bounds)` | same | `toml::from_slice` → `RawProfile` → `into_loaded`. |
| `bundled_profiles_dir()` | `-> PathBuf` | Prefers `profiles/` next to the running executable; falls back to `$CARGO_MANIFEST_DIR/profiles` for dev runs. |

---

## 3. Resolver (`crates/synapse-profiles/src/resolver.rs`)

### 3.1 `ForegroundWindow` (lines 7–13)

| Field | Type | Notes |
|-------|------|-------|
| `exe` | `Option<String>` | Foreground process exe name. |
| `title` | `Option<String>` | Window title. |
| `steam_appid` | `Option<u32>` | |
| `window_class` | `Option<String>` | |

### 3.2 `ProfileMatchResolution` (lines 23–27)

| Field | Type | Notes |
|-------|------|-------|
| `profile_id` | `ProfileId` | The winning profile's id. |
| `rank_name` | `&'static str` | The matcher that won: `"exe"`, `"title_regex"`, `"steam_appid"`, or `"window_class"`. |

### 3.3 Match strength ranking (`MatchRank`, lines 15–21)

Internal enum, ordered weakest→strongest by discriminant:

| Rank | Value |
|------|-------|
| `WindowClass` | 1 |
| `SteamAppId` | 2 |
| `TitleRegex` | 3 |
| `Exe` | 4 |

### 3.4 Algorithm — `resolve_active_profile(profiles, foreground)` (lines 33–57)

Documented as ADR-0006 precedence: `exe > title_regex > steam_appid > window_class`, then newest file mtime.

1. **Per-candidate rank** (`candidate_rank`, lines 66–111): for one `[[matches]]` entry, every field that the entry declares **must match** the foreground (an AND across declared fields), or the candidate is rejected (`None`):
   - `exe`: case-insensitive equality with `foreground.exe`. If foreground has no exe → no match.
   - `title_regex`: compiled and tested against `foreground.title`. A regex that fails to compile makes the candidate non-matching (returns `None`) rather than erroring.
   - `steam_appid`: exact equality.
   - `window_class`: case-insensitive equality.
   - The candidate's resulting rank is the **strongest** matching field present (`.max()` over the fields it set).
2. **Per-profile rank** (`best_rank`, lines 59–64): the maximum candidate rank across all of the profile's `[[matches]]`. Profiles with no matching candidate are filtered out.
3. **Winner selection** (`max_by`, lines 43–52) among matching profiles, comparing in order:
   1. higher `MatchRank` wins;
   2. then newer `modified` (file mtime) wins;
   3. then larger `source_path` wins (tie-break, reverse comparison);
   4. then larger `profile.id` wins (tie-break, reverse);
   5. then earlier slice index wins (tie-break, reverse on index).
4. Returns `Some(ProfileMatchResolution)` with the winner's id and the winning rank's name, or `None` if nothing matched.

Confirmed by tests in the same file (lines 124–419): exe beats a newer title_regex profile; equal-rank ties resolve to newer mtime; the strongest matching field within a profile is used; a match entry requires every declared field to match; an invalid `title_regex` candidate is ignored in favor of a valid window_class match.

---

## 4. Watcher / Runtime (`crates/synapse-profiles/src/watcher.rs`)

### 4.1 `ProfileRuntime` (lines 72–78)

Fields: `profile_dir: PathBuf`, `bounds: ScreenBounds`, `state: Arc<RwLock<ProfileState>>`, `_watcher: RecommendedWatcher` (kept alive for the lifetime of the runtime).

Internal `ProfileState` (lines 64–70): `profiles: BTreeMap<ProfileId, LoadedProfile>`, `active_profile_id: Option<ProfileId>`, `last_errors: Vec<ProfileLoadError>`, `last_reload_at: Option<SystemTime>`.

### 4.2 Construction

- `spawn(profile_dir)` → `spawn_with_screen_bounds(profile_dir, ScreenBounds::default())`.
- `spawn_with_screen_bounds(profile_dir, bounds)` (lines 87–149):
  1. `create_dir_all(profile_dir)`.
  2. Initial synchronous `refresh_state` (loads all profiles).
  3. Creates a `notify::recommended_watcher` whose callback forwards events over an `mpsc` channel.
  4. Watches `profile_dir` **non-recursively** (`RecursiveMode::NonRecursive`).
  5. Spawns a thread named `synapse-profile-watch` that drives reloads.

### 4.3 Hot-reload mechanism (watch thread, lines 122–146)

- On the first filesystem event, the thread sleeps `WATCH_DEBOUNCE = 200 ms` (line 24), then drains all queued events (`while rx.try_recv().is_ok() {}`) — a trailing-debounce that coalesces bursts into a single reload.
- Then calls `refresh_state`. Errors are logged via `warn!` with the error code; they do not crash the thread.
- A watcher-level error event is logged with code `PROFILE_PARSE_ERROR`.
- A closed channel (`rx.recv()` → `Err`) breaks the loop and stops the thread.

**What triggers reload:** any `notify` event on the profile directory (file create/modify/remove/rename), debounced as above.

### 4.4 `refresh_state` / `load_dir` (lines 352–427)

- Snapshots the previous profile map (under read lock).
- `load_dir` iterates `read_dir(profile_dir)`; only files whose extension equals `toml` (case-insensitive) are considered. Each is parsed with `parse_profile_file_with_bounds`.
  - Success → inserted into the new map keyed by `profile.id` (later ids overwrite earlier on collision).
  - Failure → logged (`warn!`), a `ProfileLoadError` is pushed to `errors`, and **the previously loaded profile from that same `source_path` is retained** (fail-open on individual files: a broken edit keeps the last-good version).
- Under write lock: replaces `profiles`, clears `active_profile_id` if the active id is no longer present, stores `last_errors`, sets `last_reload_at = SystemTime::now()`.

### 4.5 Query / control methods

| Method | Returns | Notes |
|--------|---------|-------|
| `profile_dir()` | `&Path` | |
| `refresh()` | `Result<()>` | Manual reload. |
| `activate(profile_id)` | `Result<()>` | Sets active id; `NotFound` if unknown. Logs `PROFILE_ACTIVATED`. |
| `list(include_inactive)` | `Vec<ProfileStatus>` | §4.6. |
| `profile(profile_id)` | `Option<Profile>` | |
| `loaded_profiles()` | `Vec<LoadedProfile>` | |
| `active_profile_id()` | `Option<ProfileId>` | |
| `last_errors()` | `Vec<ProfileLoadError>` | |
| `last_reload_at()` | `Option<String>` | Epoch milliseconds as string. |
| `resolve_foreground(fg)` | `Option<ProfileMatchResolution>` | Pure resolve; does not change active id. |
| `activate_for_foreground(fg)` | `Option<ProfileMatchResolution>` | Resolves and sets active id (via `reevaluate_foreground`). |
| `reevaluate_foreground(fg)` | `ForegroundProfileTransition` | §4.7. |

All read/write lock failures map to `ProfileError::StatePoisoned`.

### 4.6 `ProfileStatus` (lines 26–49)

Reporting view built by `profile_statuses` (lines 309–350). Fields: `id`, `label`, `use_scope`, `mode`, `detection_model_id`, `detection_classes`, `hud_fields` (names), `keymap_actions` (keys), `backends`, `event_extensions` (`Vec<ProfileEventExtensionStatus { name, emits_kind }>`), `active`, `schema_version`, `matches`, `metadata`, `source_path`. When `include_inactive` is false, only the active profile is returned.

### 4.7 `ForegroundProfileTransition` (lines 51–62)

Returned by `reevaluate_foreground`, which atomically resolves the active profile for a new foreground window and updates state. Fields: `previous_profile_id`, `active_profile_id`, `previous_scope`, `active_scope`, `effective_previous_scope`, `effective_active_scope` (unknown profiles map to `ProfileUseScope::Unknown` via `effective_scope`), `resolution`, `changed` (id changed), `scope_changed` (effective scope changed). Logs `PROFILE_FOREGROUND_ACTIVATED` or `PROFILE_FOREGROUND_CLEARED` when `changed`.

---

## 5. Package Format (`crates/synapse-profiles/src/package/`)

A package manifest bundles a profile for distribution via the profile registry. It is parsed and validated but is a distinct artifact from the profile TOML it references.

### 5.1 Parse entry points (`package/mod.rs`)

| Function | Behavior |
|----------|----------|
| `parse_package_manifest_file(path)` | Read file → `parse_package_manifest_bytes`. |
| `parse_package_manifest_bytes(path, bytes)` | `toml::from_slice` → `ProfilePackageManifest` → `validate(path)`. |
| `parse_package_manifest_bytes_with_digest(path, bytes, expected)` | Validates `expected` is `sha256:…`, recomputes `package_manifest_digest(bytes)`, rejects on mismatch, then parses. |

Constants: `PROFILE_PACKAGE_SCHEMA_VERSION = 1`, `PROFILE_PACKAGE_KIND = "profile_package"` (`package/validation.rs` 14–15).

### 5.2 `ProfilePackageManifest` (`package/types.rs` 6–33)

| Key | Type | Required | Validation summary |
|-----|------|----------|--------------------|
| `schema_version` | u32 | yes | Must be `<= 1` (`VersionIncompatible` if greater). |
| `kind` | string | yes | Must equal `"profile_package"`. |
| `package_id` | string | yes | Must contain a `.` separator; lowercase ascii + digits + `.`/`-`/`_`. |
| `package_version` | string | yes | Strict semver `major.minor.patch` (with optional `-prerelease`/`+build`). |
| `profile_id` | `ProfileId` | yes | lowercase ascii + digits + `.`/`-`/`_`. |
| `profile_version` | string | yes | Semver. |
| `created_at` | string | yes | RFC3339. |
| `author` | `PackageAuthor` | yes | §5.3. |
| `source` | `PackageSource` | yes | §5.3. |
| `targets` | `Vec<PackageTarget>` | default `[]`; must be non-empty | §5.3. |
| `assumptions` | `PackageAssumptions` | yes | §5.3. |
| `input` | `PackageInput` | yes | §5.3. |
| `permissions` | `PackagePermissions` | yes | §5.3. |
| `changelog` | `Vec<PackageChangelogEntry>` | default `[]`; must be non-empty | §5.3. |
| `hashes` | `PackageHashes` | yes | §5.3. |
| `files` | `PackageFiles` | yes | §5.3. |
| `trust` | `PackageTrust` | default | §5.3. |
| `signatures` | `Vec<PackageSignature>` | default `[]` | §5.3. |
| `metadata` | map string→string | default `{}` | Text-safety scanned (§5.4). |

### 5.3 Nested package types and validation (`package/types.rs`, `package/validation.rs`)

| Type | Fields | Validation |
|------|--------|-----------|
| `PackageAuthor` | `name`, `contact`, `attribution` | All non-empty. |
| `PackageSource` | `kind`, `uri`, `revision`, `built_by`, `generated_by` | `kind` ∈ {`bundled`,`local_user`,`registry`,`synthetic_fixture`}; others non-empty. |
| `PackageTarget` | `target_id`, `target_kind`, opt `app_id`, `process_name`, `title_regex`, `steam_appid`, `app_version` | `target_id`/`target_kind` non-empty; at least one of `app_id`/`process_name`/`title_regex`/`steam_appid`; `title_regex` must compile; `app_version` non-empty if present. |
| `PackageAssumptions` | `os`, `synapse_schema_version`, `display`, `benchmark_ids` | `os` ∈ {`windows`}; `synapse_schema_version` must equal `synapse_core::SCHEMA_VERSION`. |
| `DisplayAssumptions` | `min_width`, `min_height`, `dpi_scale_min`, `dpi_scale_max` | widths/heights positive; DPI range positive and `min <= max`. |
| `PackageInput` | `backends: Vec<Backend>`, `firmware`, `models` | `backends` non-empty and unique; each dependency validated. |
| `PackageDependency` | `id`, `version`, opt `digest` | `id` non-empty, `version` semver, `digest` (if present) is `sha256:` + 64 hex. |
| `PackagePermissions` | `license_spdx`, `contribution_terms`, `use_scope`, `execution`, `contribution` | License ∈ {`MIT`,`Apache-2.0`,`MIT OR Apache-2.0`}; `contribution_terms` ∈ {`DCO-1.1`,`none`}; `use_scope` must not be `unknown`; `share_audit_allowed` requires `export_allowed`; `execution` must be `local_only=true` and `remote_server_allowed=false`. |
| `PackageExecutionPermissions` | `local_only`, `remote_server_allowed` | See above. |
| `PackageContributionPermissions` | `export_allowed`, `share_audit_allowed` | See above. |
| `PackageChangelogEntry` | `version`, `at`, `summary` | `version` semver, `at` RFC3339, `summary` non-empty + text-safety. |
| `PackageHashes` | `profile_toml_sha256`, `package_sha256`, `assets` map | Both required digests are `sha256:` + 64 hex; asset keys non-empty, values valid digests. |
| `PackageFiles` | `profile_toml`, `assets` | `profile_toml` non-empty; each asset non-empty. |
| `PackageTrust` | `policy` (default `"local_unsigned_allowed"`), `required_signers` | `policy` ∈ {`local_unsigned_allowed`,`signed_required`}; signers are valid package-ids, unique. |
| `PackageSignature` | `signer_id`, `key_id`, `algorithm`, `signature` | `signer_id` valid package-id; `key_id` is `sha256:`+64hex; `algorithm` ∈ {`ed25519`}; `signature` is `ed25519:`+128hex; (signer,key,algo) unique. |

Cross-field rule: if `trust.policy == "signed_required"` there must be at least one signature.

### 5.4 Metadata text-safety (`validate_manifest_text_safety`, validation.rs 341–374)

Author/source text fields, changelog summaries, and all metadata keys/values are scanned (case-insensitive `contains`) against a prompt-injection poison list (`METADATA_POISON_MARKERS`, validation.rs 18–33), e.g. `"ignore previous instructions"`, `"system prompt"`, `"tool call"`, `"exfiltrate"`, `"run powershell"`, `"act_run_shell"`. A hit → `ProfileError::Parse`.

### 5.5 Digest & signature payload (`package/digest.rs`)

- `package_manifest_digest(bytes)` → `"sha256:" + lowercase hex` of the raw bytes.
- `same_digest(a, b)` compares the hex part case-insensitively (both must carry `sha256:`).
- `package_signature_payload(manifest)` builds a deterministic, line-oriented, newline-joined byte payload enumerating every signed field in a fixed order (header line `synapse.profile_package.signature.v1`, then each field). `package_signature_payload_digest` is the SHA-256 of that payload. This is the message signers sign over.

---

## 6. Error Types (`crates/synapse-profiles/src/error.rs`)

### 6.1 `ProfileError` (lines 6–43)

| Variant | Fields | `code()` (`synapse_core::error_codes`) |
|---------|--------|----------------------------------------|
| `Io` | `path`, `source: io::Error` | `PROFILE_PARSE_ERROR` |
| `Parse` | `path`, `message` | `PROFILE_PARSE_ERROR` |
| `VersionIncompatible` | `path`, `schema_version`, `supported_version` | `PROFILE_VERSION_INCOMPATIBLE` |
| `KeymapInvalid` | `path`, `alias`, `binding`, `message` | `PROFILE_KEYMAP_INVALID` |
| `HudRegionInvalid` | `path`, `name`, `message` | `PROFILE_HUD_REGION_INVALID` |
| `NotFound` | `profile_id` | `PROFILE_NOT_FOUND` |
| `Watch` | `path`, `message` | `PROFILE_PARSE_ERROR` |
| `StatePoisoned` | — | `PROFILE_PARSE_ERROR` |

`code()` maps each variant to a stable string error code (lines 45–58).

### 6.2 `ProfileLoadError` (lines 61–87)

A flattened, `Clone`/`Eq` reporting struct used for accumulated per-file load failures (stored in `ProfileState.last_errors`).

| Field | Type | Notes |
|-------|------|-------|
| `path` | `PathBuf` | From the source error; empty `PathBuf` for `NotFound`/`StatePoisoned`. |
| `code` | `&'static str` | From `ProfileError::code()`. |
| `message` | `String` | `error.to_string()`. |

`ProfileLoadError::from_error(&ProfileError)` performs the conversion.

---

## 7. TOML Schema Example

A real bundled productivity profile (`crates/synapse-profiles/profiles/vscode.toml`):

```toml
id = "vscode"
label = "Visual Studio Code"
schema_version = 2
use_scope = "productivity"
mode = "hybrid"
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "Code.exe"
title_regex = ".* - Visual Studio Code(?: \\[Administrator\\])?$"

[[matches]]
exe = "VSCodium.exe"
title_regex = ".* - VSCodium(?: \\[Administrator\\])?$"

[capture]
target = "foreground_window"
min_update_interval_ms = 50
cursor_visible = true

[keymap]
command_palette = "ctrl+shift+p"
copy = "ctrl+c"
save = "ctrl+s"
terminal = "ctrl+`"

[backends]
default = "auto"
keyboard_default = "auto"
mouse_default = "auto"
pad_default = "auto"

[metadata]
"registry.compatibility_target" = "vscode.windows"
```

A game profile adds `[detection]`, `[ocr]`, `[[hud]]`, and `[[event_extensions]]` — see `crates/synapse-profiles/profiles/luanti.minetest.toml` for a full example (pixel mode, HUD contrast fields with `fraction_of_window` regions and `color_ratio` extractors, and process/perception/action event extensions).

### 7.1 Package manifest example

A profile package manifest (`crates/synapse-profiles/tests/fixtures/profile_registry/profile_package_manifest/happy_package_manifest.toml`):

```toml
schema_version = 1
kind = "profile_package"
package_id = "profile.luanti.minetest"
package_version = "0.1.0"
profile_id = "luanti.minetest"
profile_version = "1.0.0"
created_at = "2026-05-27T09:30:00Z"

[author]
name = "Synapse Agent"
contact = "synthetic@example.invalid"
attribution = "Synthetic Luanti benchmark package fixture for Synapse issue #456."

[source]
kind = "bundled"
uri = "https://github.com/ChrisRoyse/Synapse"
revision = "m5-r02-fixture"
built_by = "codex"
generated_by = "synapse-profile-package-manifest-v1"

[[targets]]
target_id = "luanti.minetest"
target_kind = "game"
app_id = "luanti"
process_name = "luanti.exe"
title_regex = "^Luanti 5\\.16\\.[0-9]+ \\[(Singleplayer|Multiplayer)\\].*"
app_version = "5.16.1"

[assumptions]
os = "windows"
synapse_schema_version = 1
benchmark_ids = ["luanti.minetest"]

[assumptions.display]
min_width = 1280
min_height = 720
dpi_scale_min = 1.0
dpi_scale_max = 2.0

[input]
backends = ["software", "vigem"]

[[input.models]]
id = "minecraft_like_local_world_v0"
version = "0.1.0"
digest = "sha256:bbbb...bbbb"   # 64 hex chars

[permissions]
license_spdx = "MIT OR Apache-2.0"
contribution_terms = "DCO-1.1"
use_scope = "operator_owned_test"

[permissions.execution]
local_only = true
remote_server_allowed = false

[permissions.contribution]
export_allowed = true
share_audit_allowed = false

[[changelog]]
version = "0.1.0"
at = "2026-05-27T09:30:00Z"
summary = "Initial synthetic Luanti package manifest."

[hashes]
profile_toml_sha256 = "sha256:7fde...98b3"  # 64 hex chars
package_sha256 = "sha256:dddd...dddd"

[files]
profile_toml = "crates/synapse-profiles/profiles/luanti.minetest.toml"
assets = ["..."]
```
