# Building on Calyx — The Complete Builder's & Agent's Handbook

**What this is.** A self‑contained, domain‑neutral, deep reference for building *any* intelligent system on top of **Calyx**, applicable to *any* project and *any* domain — fraud/risk, clinical registries, IoT/sensor fleets, product catalogs, log/observability pipelines, recommendation, scientific corpora, document management, and anything else with a corpus and outcomes. Hand it to a person or to an AI agent and they will understand what Calyx is, everything it can do, the exact math it bakes in, the single most important design decision (embeddings vs. structured encoding), how to combine both, and the disciplined method for extracting maximum grounded intelligence. Constants and formulas below are Calyx's own; treat them as defaults you can tune.

> **Calyx in one sentence.** Calyx is an **association‑native database**: its native record is a **constellation** — one input measured through a *panel* of many independent frozen **lenses**, each producing its own typed vector kept separate (never flattened) — and on top of that record it bakes in the whole "formula for intelligence": it counts the **associations between** lenses, measures in **bits** which lenses carry real signal about outcomes, distills the minimal **kernel** that explains a corpus, **guards** generation against out‑of‑distribution drift, **predicts** consequences, and **self‑optimizes** — all in‑process, deterministic, no external services.

> ## ★ THE FOUNDATIONAL PRINCIPLE — read this before anything else
>
> **Building on Calyx means committing to first principles.** You must decompose the *entire* project into its **atomic, irreducible, independently‑measurable units**, and then compute **ALL** associations among them **from the ground up** — every base relationship, not a sampled, assumed, or hand‑picked subset. This is the *only* way to **truly understand** a system, and this exhaustive ground‑up association structure **is the information Calyx actually derives its intelligence from.**
>
> Records are inert. A row, a document, a value — on its own it carries almost no intelligence. **The complete web of associations among the atoms is the raw material of intelligence.** Everything else in this handbook — bits, sufficiency, the kernel, the guard, the oracle, self‑optimization — is a way of *measuring, distilling, and composing* that ground‑up association structure. The kernel is its compressed generator; the bits are its grounded weights; the guard, oracle, and answers are built *from* it.
>
> **Therefore, never shortcut it.** If you skip the decomposition to atoms, or leave base associations uncomputed, the intelligence you extract is partial, biased, and ungrounded — you will have measured a shadow of the system, not the system. The discipline is always the same: **atoms → all base associations → differentiate → kernel → compose.** Decompose everything; compute every association from the ground up; only then does Calyx have the full information it needs to derive maximum grounded intelligence.

**How to read this.** §1–§4 are the model. §5 is the core decision (embed vs. encode) and the single most important section for structured data. §6 is the extraction *method*. §7 is the subsystem‑by‑subsystem deep reference with the math. §8–§9 are interfaces and recipes. §10 shows the same mechanics across many domains. §11 is tuning/ops. §12–§14 are doctrine, a formulas/constants/errors reference, and a glossary.

---

## 1. What Calyx is

**The thesis.** Calyx is "the formula for intelligence, implemented as a database." Three things in one: (1) the engine of a **calculus of association** — four native verbs; (2) a **universal database** that serves every paradigm's root purpose on one ordered transactional core; (3) an **oracle/kernel substrate** for grounded prediction and knowledge.

**The native record is not a row or a vector — it is a constellation.** One input, measured through a *panel* of many frozen instruments ("lenses"), each producing its own typed slot‑vector that is **kept separate and never concatenated into one opaque blob**. On top of that record Calyx bakes in association‑counting, information‑theoretic measurement, kernel discovery, a fail‑closed guard, a provenance ledger, reversible self‑optimization, and grounded consequence prediction.

**The four verbs** (this is the whole pipeline):

| Verb | Meaning | What it produces |
|---|---|---|
| **Measure** | view one input through a panel of lenses | a constellation (many typed slots) |
| **Count** | derive the associations *between* slots and between records | cross‑terms, an agreement graph, a between‑record graph |
| **Differentiate** | quantify the unique **bits** each lens/association adds about a real outcome | signal in bits, sufficiency, redundancy, causality |
| **Compose** | find the kernel, guard generation, answer, predict, self‑optimize | index, answers, guard verdicts, predictions, tuned params |

**Three trust principles, enforced in code:**
- **Grounding is mandatory.** Claims are measured against *anchored real outcomes*. Anything not grounded is tagged **provisional**, never "trusted."
- **No‑flatten.** Slots stay typed and separate end‑to‑end. The guard scores each slot independently; you never average a panel into one number to make a decision.
- **Fail closed.** Unknown lens, shape mismatch, uncalibrated guard, missing data, non‑finite value ⇒ a **structured error** (`{code, message, remediation}`), never a silent wrong answer.

**The living‑system map** (each life‑like property maps to a concrete engine): perception = lenses · memory = the store · cognition = associations + search · differentiation = the bits engine · homeostasis = the self‑optimizer · foresight = the oracle · immune boundary = the guard. Calyx claims *operational* intelligence and life‑like behavior — never consciousness.

---

## 2. When to use Calyx, and the universality claim

**Universality principle.** Serve the root purpose of **every** data paradigm from first principles, not by bolting engines together:

| You want… | In Calyx it is… | Mechanic |
|---|---|---|
| a **vector database** | a **1‑lens** Calyx | one embedder lens + ANN |
| **full‑text search** | a **sparse lens** + BM25 | sparse slot + inverted index |
| a **document / key‑value / columnar store** | collections over one ordered core | scalars + metadata + column families |
| a **graph database** | the association graph + traversal | cross‑terms + between‑record edges + best‑first walk |
| a **time‑series store** | recurrence series + temporal lenses | event series + recency/periodic/positional |
| a **feature store / analytics** | slots + scalars + measured bits | encoders + mutual information |
| **retrieval + reranking** | multi‑lens fusion + a reranker lens | RRF + cross‑encoder |
| an **agent's memory / knowledge layer** | constellations + kernel + oracle + ledger | the whole stack |

**Reach for Calyx** when you need *grounded intelligence over a corpus*: semantic recall, "which features actually matter (in bits)", relationship discovery, deduplication, anomaly detection, grounded question‑answering, consequence prediction, imputation, forecasting, or provenance — and you want it embedded, deterministic, and free.

**Do not reach for Calyx** when a plain transactional row‑store with no intelligence requirement is all you need and you will never ask an association/bits/kernel/guard/oracle question — although even then, Calyx's universal core can serve the row‑store role.

---

## 3. Core concepts in depth

**Lens.** A frozen, content‑addressed measurement instrument implementing `measure(input) -> vector`. Examples span every modality: a text embedder, an image encoder, a numeric normalizer, a one‑hot encoder, a temporal decay, a graph‑structural signature. **Invariants:** a lens is *frozen* — its weights/spec never change; if the runtime drifts, that becomes a **new** lens id, never a silent reuse. Its identity is a hash of `(name, weights, corpus, output‑shape)`. Adding a lens is *one call*; its worth is *one number* (bits). This "plug‑in lens is the key" property is sacred — never make it harder.

**Slot.** One lens's typed output vector inside a record, addressed by a stable slot id and a human‑readable key. Slots carry lifecycle state (active / parked / retired), a quantization policy, an optional axis tag, and flags like *retrieval‑only* (used post‑retrieval, not for primary recall) and *excluded‑from‑dedup*. **Slots stay separate — never flattened.**

**Panel.** A versioned set of slots — the lenses you measure every input through. Panels are **hot‑swappable**: `add_lens` bumps the version and enqueues **lazy backfill** (old records fill in the background; new records are searchable immediately). Park/retire transitions are non‑destructive; retired slots stay readable for history.

**Constellation.** One input measured through the panel = the atomic record. It carries the typed slots, exact scalar measurements, verbatim metadata, grounded anchors, and a provenance reference. Its id is a content address of `(input bytes, panel version, vault salt)` — so identical input under the same panel yields the same id ⇒ **idempotency**.

**Anchor.** A grounded real‑outcome observation attached to a record: a label, a reward, a pass/tie/thumbs, an identity/style hold, or a recurrence, with a source and a confidence. **Bits are measured *about* anchors.** An anchor is "grounded" iff its source is non‑blank and its confidence is finite in `(0,1]`. Without grounded anchors, every bits/sufficiency result is **provisional**.

**Cross‑term.** A derived association between two slots (agreement / delta / interaction / concat). Between records, associations become **edges** in a nearest‑neighbor graph. The cross‑term is the atom of "counting."

**Kernel.** The minimal generating core of a corpus — the small set of records (a feedback‑vertex‑set, on the order of ~1%) from which the rest of the corpus's intelligence can be reconstructed. It doubles as an index and an answer path, and can be computed at **any scope**.

**Guard.** The per‑slot, fail‑closed, conformally‑calibrated gate that decides whether a query/output is inside the trusted region — per slot, never averaged.

---

## 4. Data model

- **Constellation** — `{ id, vault_id, panel_version, created_at, input_ref, modality, slots: map<slot_id, slot_vector>, scalars: map<string,f64>, metadata: map<string,string>, anchors: [anchor], provenance, flags }`. Validated fail‑closed: panel version > 0, every slot vector valid, scalar values finite, non‑empty keys, every anchor valid.
- **Slot vector shapes** — `Dense{dim, data}` · `Sparse{dim, entries:[{idx,val}]}` · `Multi{token_dim, tokens:[[..]]}` (multi‑vector / late interaction) · `Absent{reason}`. **`Absent` is an explicit absence — never interpret it as a zero vector.** Validation: dense length == dim and finite; sparse indices in range, no duplicates; multi tokens all == token_dim.
- **Modality** — the accepted input kinds: `Text, Code, Image, Audio, Video, Structured, Mixed`, plus scientific modalities (protein/DNA/molecule). **`Structured` is first‑class**, and records natively carry `scalars` (exact numeric measurements) and `metadata` (verbatim strings) so you keep exact values alongside vectors.
- **Anchor** — `{ kind, value, source, observed_at, confidence∈[0,1] }`. Kinds cover labels, rewards, pass/tie/thumbs, identity/style holds, and recurrence. Values: bool / number / enum / one‑hot / text / vector.
- **Signal** — a measured bits result: `{ bits, confidence_interval, n_samples, estimator, timestamp }`. Each slot carries a `bits_about` map (per outcome axis).
- **Identity & determinism.** All ids are content addresses: a 128‑bit hash over **length‑delimited ordered parts** (so `["ab","c"]` ≠ `["a","bc"]`). Lens id = hash of `(name, weights_sha, corpus_hash, output_shape)`; record id = hash of `(input_bytes, panel_version_be, vault_salt)`. Determinism and idempotency fall out for free; a determinism probe requires byte‑identical repeat output.
- **Extension traits.** Implement `Lens` (measure), `Index` (ANN/inverted), `VaultStore` (storage), or `Estimator` (information measures) to extend the engine. All are object‑safe and `Send + Sync`.

---

## 5. THE CORE DECISION — embeddings vs. structured encoding (the most important section)

For any input you ask: **do I derive meaning by embedding raw bytes into a vector, or by treating the explicit fields directly?** This one choice determines whether your intelligence is grounded and cheap or fuzzy and noisy.

### 5.1 The principle — where does the meaning live?

- **Unstructured — text, images, audio, video, code, scientific sequences.** The meaning is *latent in the raw bytes*; a pixel or a byte is not itself the concept. You **must** use a learned **embedder lens** to project into a vector where **geometry = meaning** (cosine ≈ semantic distance). "Association" here = nearness in embedding space; you retrieve with approximate nearest neighbors and fuse with rank fusion.
- **Structured — tables, records, fields: numbers, categories, booleans, ordinals, timestamps, references.** The meaning is *already explicit*. A duration of `42`, a status of `active`, a flag `true` already **are** the concept. You should **not** run a learned embedder over them, because it (a) destroys the exact, auditable value, (b) replaces information theory (the right tool) with cosine geometry (the wrong one), and (c) forfeits Calyx's ability to measure the association between a field and an outcome **directly, in bits**. "Association" here = **statistical dependence** (mutual information, correlation, transfer entropy), not geometric nearness.

> **Rule of thumb:** *Embed what is latent; encode what is explicit; measure both against grounded anchors in bits so they live on one axis.* Never embed a value that is already meaningful.

### 5.2 Every way to *use* structured data (deterministic encoders — no learned embedder)

Each becomes a frozen lens → a slot. Apply as many as fit; more diverse encodings ⇒ more captured associations.

**Per‑field value encoders**
1. **Scalar normalization variants** (each its own lens on the same field):
   - `raw` (as‑is, for already‑bounded values);
   - `min‑max` → `(x−min)/(max−min)` into `[0,1]`;
   - `z‑score` → `(x−μ)/σ`;
   - `log / log1p` → `sign(x)·log(1+|x|)` for heavy tails / multiplicative scales;
   - `rank / quantile` → the empirical CDF value in `[0,1]` (monotone, outlier‑robust, distribution‑free);
   - `robust‑scale` → `(x−median)/IQR`;
   - `winsorized` → clip to percentiles then scale.
   Different normalizations expose different structure; a mutual‑information estimator sees different neighborhoods under each.
2. **Cyclic encoding** — periodic fields (hour, weekday, month, angle, phase) → `[sin(2πx/P), cos(2πx/P)]` so the ends wrap (23:00 near 01:00). Linear scaling breaks this.
3. **Binning / discretization** — continuous → bucket id (equal‑width, equal‑frequency, or domain thresholds), then one‑hot or ordinal. Exposes non‑linear regimes and enables discrete estimators and stratification.
4. **One‑hot** — low‑cardinality category → a one‑hot vector (stable, first‑appearance bucket order).
5. **Ordinal** — ordered category → normalized position in `[0,1]`, preserving order.
6. **Boolean / flag** → `{0,1}`; a set of flags → multi‑hot.
7. **Feature hashing (the hashing trick)** — high‑cardinality id/string → a signed dense vector: for each token, add `sign_hash(token)·unit(hash(token) mod d)`. Bounded dimension `d`, collisions tolerated; no vocabulary to maintain.
8. **Count / frequency encoding** — encode a category by its occurrence count/frequency (a popularity signal).
9. **Target / mean encoding (grounded, careful)** — encode a category by its *mean anchored outcome* (e.g. mean conversion rate). Powerful, but compute on **held‑out** folds to avoid leakage and treat as **provisional** until validated; it consumes the anchor.
10. **Set / bag / multi‑hot** — set‑valued fields (tags, memberships, permissions, baskets) → a **sparse** vector. Membership without imposing order; enables sparse ANN and set‑overlap similarity.

**Whole‑record & cross‑field encoders**
11. **Record vector** — assemble all numeric fields into one unit‑normed dense vector: a hand‑built, weightless "tabular embedding". Enables cosine find‑similar *and* stays exact.
12. **Derived / engineered features** — ratios, products, differences, rates (`rate = events/time`, `margin = price − cost`, `density = mass/volume`, `utilization = used/capacity`), each an explicit frozen lens. This is where domain knowledge injects signal directly.
13. **Interaction encodings** — explicit cross‑field terms (`A·B`, `A/B`, polynomial features, one‑hot crosses like `region × channel`) as lenses; the association engine also derives interaction cross‑terms automatically, but explicit ones you name are indexable and interpretable.
14. **Aggregations over relations** — for one‑to‑many links, encode count / sum / mean / min / max / last / std / entropy of the child set as fields (e.g. "number of linked events", "mean linked value").

**Relational / structural encoders**
15. **Reference → graph structural signature** — build the reference/foreign‑key graph; encode each record by its **degree, in/out degree, betweenness, eigenvector/PageRank centrality, clustering coefficient, and neighbor‑label histogram**. Captures *position in the relationship graph* without a learned graph network.
16. **Path / hierarchy encoding** — tree/DAG position: depth, ancestor set, sibling rank, path‑hash — for taxonomies, org charts, category trees.
17. **Between‑record association graph** — a nearest‑neighbor graph over any of the above slots → the relationship graph *between* entities, queried by traversal / best‑first reach.

**Temporal / sequential encoders**
18. **Temporal lenses** — timestamps → recency (decay: linear / exponential half‑life / step), periodicity (time‑of‑day, day‑of‑week fit), positional (forward/backward sequence sin/cos). Retrieval‑only and **never dominant** (a bounded post‑retrieval boost, so time nudges ranking but cannot overrule content).
19. **Recurrence series** — model repeated events as a time series → periodicity, cadence, overdue hazard, next‑occurrence prediction.
20. **Lag / delta features** — change since last observation, rolling mean/variance, streak length, time‑since‑last.

### 5.3 Every way to *derive insight* from structured data (information‑theoretic — the real payoff)

Once fields are slots and anchors are attached, Calyx extracts intelligence **without any embedder**, in **bits**:
1. **Bits per field (mutual information)** — how much each field predicts a grounded outcome; rank features by *real* signal; park anything below the signal floor.
2. **Redundancy** — pairwise mutual information / normalized MI / correlation shows which fields duplicate each other (drop the redundant); **effective rank / total correlation** says how many *truly independent* fields you have.
3. **Panel sufficiency** — `I(fields; outcome) ≥ H(outcome)`: do your fields collectively explain the outcome? If not, the deficit says *which field is short and by how many bits*.
4. **Synergy (interaction information)** — field *combinations* that carry signal beyond the individuals (neither field alone predicts, but their product does).
5. **Directional causality (transfer entropy)** — over time, which field/event *drives* which; turns correlation into an arrow.
6. **Association graph over fields** — which fields co‑vary; for dashboards and dimensionality reduction.
7. **Anomaly / blind‑spot detection** — records inconsistent across their own fields (one lens confident where its neighbors disagree).
8. **Grounding kernel** — the minimal set of records/fields that explains the corpus; "answer over your data"; coverage/grounding‑gap reports.
9. **Small‑sample estimates** — conjugate Bayesian posteriors for rate/consistency when data is thin.
10. **Guarded validation** — fail‑closed out‑of‑distribution checks on incoming/generated records, per field.
11. **Prediction / imputation** — consequence what‑if, root‑cause abduction, and completion of missing fields from the trusted region.

This is why structured data is often *more* valuable than unstructured: the values are exact, so the intelligence is grounded and auditable, not a fuzzy nearest‑neighbor.

### 5.4 The special case — structured data with **text labels** (embed those, as an extra slot)

Data is rarely *purely* structured or *purely* unstructured — **it depends on how the data looks and is set up.** A structured record, field, or category value often carries a **natural‑language label or description** — a word, a sentence, or several. That text **should be embedded** and added as *another slot on the same record*, alongside the exact encoders:
- A category value → embed the **sentence(s) that describe what it means**, so "similar in meaning" works even when the symbols differ.
- A record with a `description` / `notes` / `summary` field → embed the prose; keep the numbers as scalar lenses.
- An enum whose **docstring** carries the real semantics → embed the docstring as the category's meaning.
- A row with no prose but rich structure → **generate a one‑sentence natural‑language summary of the row** and embed *that* — a hand‑built semantic view of a structured record.

Now the same record has **both** an exact structured encoding (auditable, information‑theoretic) **and** a semantic embedding (fuzzy, meaning‑aware). **How much prose to embed depends on the data:** a bare number → don't embed; a symbol with rich meaning → embed its description; a field already prose → embed it directly.

### 5.5 Decision framework — vector vs. structured encoding

| Signal in your data | Use an **embedder lens** (vector/ANN) | Use a **structured encoder** (exact/bits) |
|---|---|---|
| Free text, images, audio, video, code | ✅ required | — |
| A field that is a **sentence/description** | ✅ embed it (extra slot) | keep numeric siblings as encoders |
| High‑cardinality string you want *fuzzy* similarity on | ✅ (or a commissioned entity‑embedding) | feature‑hash if you only need exact identity |
| Numbers, counts, prices, durations, measurements | ❌ don't embed | ✅ scalar variants |
| Categories, enums, flags, ordinals | ❌ don't embed the symbol | ✅ one‑hot / ordinal / hash |
| Foreign keys / relations | (optional entity‑embedding) | ✅ graph structural signature |
| Timestamps / sequences | — | ✅ temporal lenses + recurrence |
| You need **exact filters, audit, determinism, causality, bits** | — | ✅ structured |
| You need **"find things that mean the same"** | ✅ embedding | — |

**Heuristics:** if you can write the value down and it's already the concept → encode it. If the concept is only *implied* by the raw content → embed it. If a field has both a number and a description → do both. Prefer bits over cosine whenever values are explicit.

### 5.6 Combining insights when a record has **both** (the hybrid — the whole point of a constellation)

A constellation is a **panel of many lenses of different kinds measuring one input at once** — deterministic encoders for the structured fields, learned embedders for the text/image fields, temporal lenses for the timestamps — all as **separate, typed slots** in one record. That co‑existence is exactly what lets you fuse structured and unstructured intelligence:
1. **Measure each slot independently, then fuse.** Search each slot (structured *and* embedding) and combine with **rank fusion** — "records whose *numbers* are similar AND whose *description* means the same." The guard checks each slot with its own threshold.
2. **Put everything on one axis: bits about a shared anchor.** A structured scalar slot and a text‑embedding slot are both measured in **bits about the same grounded outcome**, so you can compare "how much does the *description embedding* predict the outcome vs. the *duration field*." Sufficiency then asks whether *structured + embedded together* explain the outcome, and the deficit says which side to strengthen.
3. **Cross‑terms between a structured slot and an embedding slot** capture *"does the text match the numbers"*; a mismatch flags an anomaly (mislabeled, mispriced, description‑vs‑data drift).
4. **Kernel & prediction span all slot kinds** — imputation can fill a missing *number* using evidence from the *text*, and vice versa.
5. **Route by strength** — exact structured slots for filtering, analytics, and causality; embedding slots for semantic recall, dedup‑by‑meaning, and the answer assistant.

**Three archetypes to copy:** **structured‑only** (config/analytics corpus — encoders → associations → bits/sufficiency/causality → kernel + what‑if, no embedders); **embedding‑only** (text/media search — embedders → ANN + fusion → kernel answer + guard, a grounded vector DB); **hybrid** (records with fields *and* prose/media — both families in one panel, fused per §5.6). The hybrid is where Calyx beats a bolted‑together stack: the fusion, the bits, the kernel, the guard, and the provenance are one engine over one no‑flatten record.

---

## 6. THE METHOD — extracting maximum intelligence (kernel‑first)

The mindset every builder and agent applies to any Calyx system: **decompose to first principles → surface all associations at their base → distill the kernel → build every capability outward from the kernel.**

### 6.1 The core stance
1. **The unit of intelligence is the association, not the record.** Never ask "what is in this row" — ask "what is associated with what, how strongly, in bits, about which grounded outcome." Calyx makes the association first‑class (the cross‑term, the bits, the edge).
2. **Every system has a kernel — find it, then generate everything from it.** There is a minimal generating core (a small feedback‑vertex‑set) from which the rest of the corpus's intelligence can be reconstructed. Maximum intelligence is **derive the kernel, prove it explains the corpus, compose all capabilities on top of it** — not "measure everything forever." The kernel is both the compression and the launchpad.

> **One‑line strategy:** *Break the project into atomic measurements, weave every base association, differentiate which associations carry grounded bits, distill those into the kernel that generates the whole corpus, and build guard / prediction / imputation / search / forecasting / self‑optimization outward from that kernel.*

### 6.2 The loop

```
   ① DECOMPOSE            ② ASSOCIATE               ③ DIFFERENTIATE            ④ DISTILL → COMPOSE
 first principles:      see ALL associations       keep only what carries      derive the KERNEL, then
 atoms = inputs,        AT THEIR BASE:             grounded signal (bits):     build every capability
 fields, candidate      pairwise cross-terms +     mutual information, prune    FROM it: index, answer,
 lenses, anchors        between-record graph +     redundancy, find synergy    guard, predict, impute,
 (explicit? latent?)    temporal lead/lag          & causality, sufficiency    forecast, self-optimize
      │                       │                          │                            │
      └───────────────────────┴──────────  feedback: deficits → propose new lenses → re-kernel  ──────────┘
```

- **① Decompose.** Reduce the project to atoms: entities, every field, explicit‑vs‑latent, groundable outcomes. Model *measurable aspects*, not "tables." Ask relentlessly: "can this be split into a more primitive, independently‑measurable thing?" Each atom → a candidate lens.
- **② Associate.** Instrument maximally, then weave **every** base association: within‑record cross‑terms, the between‑record nearest‑neighbor graph, and temporal lead/lag. Read the agreement graph — it is the map.
- **③ Differentiate.** Collapse to the load‑bearing structure: bits per lens/association, prune redundancy, find synergy and causality, check sufficiency `I(panel;outcome) ≥ H(outcome)`.
- **④ Distill → compose.** Derive the kernel at every scope; from it build index, answer, guard, prediction, imputation, forecasting, self‑optimization. Every higher capability is built *from* the kernel.
- **Feedback.** Deficits name missing bits → propose a lens → re‑weave → re‑kernel. The kernel tightens; capabilities sharpen; the intelligence objective rises.

### 6.3 The agent's checklist
Atoms decomposed to the primitive? Every atom measured, and the lens set *diverse* (check effective rank)? Embedding anything explicit (waste) or encoding anything latent (loss)? Rich symbols' descriptions embedded too? Real outcomes anchored (else all bits provisional)? All base associations woven? Which carry bits, which are redundant, where's the synergy, where are the arrows? Sufficiency met — and if not, which lens is short by how many bits? What's the minimal kernel, does it explain the corpus, where are the gaps? Are guard/prediction/imputation/forecast/search built off the kernel? Is the growth loop running?

### 6.4 The full build playbook
- **Phase 0 — Model.** Inventory entities/fields; tag types; mark explicit‑vs‑latent; identify anchors. Output: atoms + candidate lenses + anchors.
- **Phase 1 — Encode.** Register the applicable encoders per field (§5.2); embed text labels (§5.4); assemble diverse panels; let the capability gate admit/park/retire by measured signal and redundancy.
- **Phase 2 — Ingest.** Canonically byte‑encode → measure → store the constellation, idempotent and provenanced. **No side store.**
- **Phase 3 — Associate.** Weave within‑record cross‑terms + the between‑record graph + temporal lead/lag; produce the abundance report and agreement graph.
- **Phase 4 — Differentiate.** Attach anchors; measure bits; enforce the contract; compute effective rank, synergy, causality, periodicity, posteriors; check sufficiency; route deficits to propose‑lens.
- **Phase 5 — Kernel.** Derive the kernel at each scope; verify recall; produce the grounding‑gap report.
- **Phase 6 — Compose.** Turn on answer/define, guarding, consequence what‑if, root‑cause abduction, field completion, forecasting, find‑similar, dedup, reactive triggers.
- **Phase 7 — Grow.** Deficits auto‑propose lenses; the self‑optimizer tunes fusion/quant/thresholds (shadow‑tested, reversible); run the readiness predicate.
- **Phase 8 — Lower & verify.** For real‑time/deterministic consumers, **lower** the needed intelligence into fingerprinted, reproducible frozen artifacts — never call Calyx from a hot loop — then verify by reading the actual bytes and proving observed == expected.

---

## 7. Subsystem deep reference (with the math)

### 7.1 The lens registry (Measure)
A lens is content‑addressed by a **frozen contract** `(name, weights_sha, corpus_hash, shape, modality, dtype, norm)`. `lens_id = hash(name, weights_sha, corpus_hash, output_shape_fingerprint)`. **Registration is fail‑closed** — a frozen contract is mandatory; a determinism probe (measure twice, require byte‑identical output) upgrades trust; duplicate ids are rejected. **Norm policies:** none / finite‑only / L2‑unit / declared‑by‑model, checked within tolerance. **Runtimes** cover algorithmic (weightless encoders), HTTP/transformer/ONNX text embedders (incl. sparse SPLADE, multi‑vector, cross‑encoder reranker), static lookup, multimodal adapters, external command, temporal (recency/periodic/positional), and commissioned (frozen‑from‑corpus). **Capability card & gate:** profiling produces a card (signal, spread, separation, cost, coverage); the gate then **Admits / Parks / Retires** a lens — *retire* if it correlates above the ceiling with an existing lens, *park* if it has no grounded signal, is collapsed, or is below the bit floor, else *admit*. Placement chooses CPU/GPU under RAM/VRAM budgets; compression quantizes slots only while recall holds (else falls back to raw).

### 7.2 Search & navigation (part of Compose)
Per‑slot indexes: **HNSW** in RAM (deterministic levels), **DiskANN/Vamana** on disk (paged, RobustPrune + beam search), **SPANN** (RAM centroids + on‑disk posting lists), and a **kernel‑first funnel** (3‑hop). **BM25** inverted index for lexical/sparse lenses (k1≈1.2, b≈0.75). **Multi‑vector MaxSim** late interaction for token‑level slots. **Fusion = Reciprocal Rank Fusion:** `score(d) = Σ_slots 1/(K + rank_slot(d))` with `K = 60` (also weighted‑RRF and single‑lens); a deterministic classifier picks the strategy and a planner enforces caps (top‑k, ef, slot count, cost). **Temporal post‑retrieval boosts** nudge ranking within a bounded fraction (never dominant). Capabilities: semantic search, hybrid dense+sparse, neighbors (find‑similar), define, agree/disagree, guarded search, traverse.

### 7.3 Associations — Derived Data Abundance (Count)
`n` inputs × `N` lenses yield `n·(N + C(N,2) + 1)` signals: the `N` measured slots, the `C(N,2) = N(N−1)/2` pairwise **cross‑terms**, and the whole‑panel signal.

| Cross‑term | Exact formula | Output |
|---|---|---|
| **Agreement** | cosine `Σaᵢbᵢ / (√Σaᵢ² · √Σbᵢ²)` | scalar (edge weight when clamped to `[0,1]`) |
| **Delta** | `aᵢ − bᵢ` | vector |
| **Interaction** | Hadamard `aᵢ·bᵢ` | vector |
| **Concat** | `[a‖b]` | vector (the only kind allowing unequal dims) |

**Materialization policy:** agreement always eager; interaction eager iff its pair‑gain ≥ the bit floor (≈0.05 bits), else lazy; delta/concat lazy (computed on demand, LRU‑cached). **Agreement graph:** aggregate all agreement scalars per slot pair into a weighted undirected edge list (which lenses co‑vary). **Between‑record graph:** a nearest‑neighbor association graph over records (relationships *between* entities). **Temporal cross‑terms:** for two event series, `lead_lag = median(t_b − t_a)` over co‑occurring pairs within a window (positive ⇒ B follows A). **Blind‑spot detector:** flags a record when one lens is confident while its neighbors disagree beyond a threshold. **Reactive engine:** after each ingest, bounded audited triggers fire on **new‑region / recurs / drift**, feeding subscriptions.

### 7.4 Intelligence in bits (Differentiate)
Everything base‑2. **Mutual information** via a **k‑nearest‑neighbor (KSG) estimator**: for each point, take the distance `ε` to its k‑th neighbor in the *joint* space (max‑norm across the two marginals), count neighbors within `ε` in each marginal (`nx`, `ny`), and average the local term `ψ(k) + ψ(n) − ψ(nx+1) − ψ(ny+1)` (digamma ψ), converting nats→bits by `/ ln 2`; clamp ≥ 0. Continuous↔discrete routes through one‑hot labels. A **deterministic bootstrap** gives a (conservatively widened) confidence interval. Sample floor ≈ 50.

- **Differentiation contract:** admit a lens iff signal ≥ a small bit floor (≈0.05 bits) and max pairwise correlation ≤ a ceiling (≈0.6); a **stratified override** admits a globally‑weak lens that is the sole carrier of a rare‑but‑critical stratum (bits are never multiplied by raw frequency).
- **Panel sufficiency:** `sufficient ⇔ I(panel;anchor) ≥ H(anchor)` — the threshold *is* the outcome entropy, no slack. When insufficient, the total deficit is split across slots **inversely to each slot's marginal bits** (the weakest slots absorb the largest "missing bits"), and each deficit suggests: add an outcome anchor / propose a lens / gather more samples.
- **Total correlation** `TC(Φ) = Σ H(slotₖ) − H(Φ)`; **effective rank** `n_eff ≈ n·(1 − TC/Σ H_marginal)` (or stable rank `trace²/‖·‖²`) — how many truly non‑redundant lenses you have.
- **Transfer entropy** `T(A→B) = I(B_future; A_past, B_past) − I(B_future; B_past)`, with a lag sweep; direction = the larger of `T(A→B)`, `T(B→A)`.
- **Interaction information** (three slots): sign classifies **redundant / synergistic / unclear** from whether the confidence interval straddles zero.
- **Marginal value** `= panel_bits − panel_without_lens_bits` (bits lost if a lens is removed). **Data‑processing‑inequality ceiling** `= I(panel;outcome)` — never claim derived `C(N,2)` signal beyond it.
- **Periodicity:** Lomb–Scargle periodogram + slotted autocorrelation + permutation false‑alarm probability. **Change‑point / hazard:** two‑sided CUSUM + a renewal "overdue" hazard. **Two‑sample drift:** kernel MMD with a permutation p‑value. **Small samples:** conjugate posteriors (Gamma‑Poisson for rates, Beta‑Bernoulli for consistency) with credible intervals.

### 7.5 The grounding kernel (Compose)
Pipeline: strongly‑connected components → betweenness → greedy selection of the top fraction by `score = 0.40·degree + 0.40·betweenness + 0.20·groundedness` → an approximate directed feedback‑vertex‑set (~1%). The kernel doubles as an **index** (fast grounded recall) and an **answer path** (BFS reach with `hop_score = edge_weight · 0.9^hop`). A **recall gate** (default target ≈0.95) validates that the kernel explains held‑out queries; a **grounding‑gap** report names where anchors are missing. Compute the kernel **at any scope** — a subset, a domain, or the whole corpus.

### 7.6 The fail‑closed guard (Compose)
Scores **each required slot independently** with cosine against a per‑slot threshold τ (never an averaged/flattened gate). **Combination:** all‑required (AND of all slots) or k‑of‑n. **Conformal calibration:** τ is the smallest candidate cosine where empirical bad‑cases' false‑accept‑rate ≤ target AND a binomial bound holds (needs enough bad scores; per‑category defaults e.g. identity 0.01 / content 0.03 / stylistic 0.05; cold‑start τ ≈ 0.7). **Verdicts:** accept / new‑region (learn) / quarantine / refuse (out‑of‑distribution). Supports **identity‑lock** (canonical entities can't drift). Every verdict is logged to the ledger.

### 7.7 The oracle — prediction, completion, readiness (Compose)
- **Forward prediction:** gather recurrence evidence for an action in a domain, bucket outcomes, pick the top; `raw_confidence = support · separation · sample_support` (top/total, (top−second)/total, total/(total+2)), then cap by `min(raw, self_consistency_ceiling, dpi_ceiling)`.
- **Consequence (butterfly) tree:** DFS, bounded depth (≈4), per‑hop attenuation (×0.7), prune children below a confidence floor (≈0.05), cycle‑guarded; branching is data‑driven from observed grounded edges.
- **Reverse (abductive) walk:** outcome → cause, bounded depth (≈3); grounded confidence `n/(n+1)`.
- **Honesty gate (binding):** returns **Insufficient** with a per‑sensor deficit exactly when `panel_bits < anchor_entropy_bits`; it never emits a confident answer a panel can't support.
- **Completion:** energy‑descent imputation of the *free* slots from trusted‑region attractors (softmax over cosines), gated by the honesty gate; each filled slot tagged inferred / provisional.
- **Time‑of‑next‑occurrence:** cadence = median gap, with a regularity‑ and support‑weighted confidence and an interval.
- **Readiness predicate:** a multi‑tier conjunction (clean self‑consistency, panel sufficient, kernel exists at high recall, calibrated, defended against gaming, mistakes closed) — a falsifiable per‑domain "is this system ready" gate.

### 7.8 Foundation
- **Store (LSM).** Write‑ahead log + group commit → memtables → immutable sorted tables; many column families routed by key; **MVCC snapshots** (⇒ time‑travel / undo / consistent reads); crash‑safe manifest; hot/cold tiering; content‑address dedup (cosine + recurrence signature). Sacred data (log, ledger, base/slot rows, anchors, manifest) is never auto‑deleted; regenerable data (indexes, caches, kernel/guard artifacts) rebuilds from it.
- **Provenance ledger.** Append‑only hash chain (each entry seals the previous), periodic Merkle checkpoints, optional signatures. `verify_chain` re‑walks and re‑hashes; `reproduce` re‑derives a result and bounds drift. Stores hashes/ids, never secrets.
- **Math runtime.** CPU‑SIMD and GPU backends implementing the same ops with **bit‑parity** (within a tolerance), and **measured quantization** (compress only as far as bits/similarity/false‑accept allow; fail closed on intelligence loss). VRAM budgeted.
- **Self‑optimizer.** Reversible, shadow‑tested tuning of fusion weights / quantization / thresholds / index params / small online heads: prepare (reserve rollback) → budget → shadow‑test on held‑out replay → gate on tripwires and per‑metric non‑regression → promote (pointer swap + ledger) or roll back. Drives the intelligence objective.
- **Graph primitives.** Strongly‑connected components, betweenness, eigenvector centrality, spectral structure, and weighted best‑first reach over the association graph.

---

## 8. Interfaces

**The agent tool surface (31 tools).** Purpose in one line each:

| Group | Tool → purpose |
|---|---|
| Vault & panel | `create_vault` (new store) · `add_lens` (register + backfill) · `retire_lens` / `park_lens` (lifecycle) · `list_panel` (inspect) · `profile_lens` (capability card) |
| Ingest & measure | `ingest` (records → constellations) · `ingest_media` (raw media → derived‑text provenance) · `anchor` (attach a grounded outcome) · `measure` (one input → slots) |
| Search & navigate | `search` (multi‑lens fused recall) · `kernel_answer` (grounded answer over the corpus) · `neighbors` (find‑similar) · `agree` / `disagree` (per‑lens support/conflict) · `define` (grounded definition) · `guard_generate` (guarded generation) · `traverse` (walk the association graph) · `skills` / `search_skill` (capability discovery) |
| Intelligence | `abundance` (DDA report + blind spots) · `bits` (MI per lens/pair/panel) · `kernel` (discover the kernel) · `guard.calibrate` / `guard.check` (calibrate/apply the guard) · `propose_lens` (deficit‑driven lens proposal) |
| Provenance & ops | `provenance` (lineage) · `answer_trace` (how an answer was formed) · `verify_chain` (integrity) · `reproduce` (re‑derive + drift bound) · `anneal.status` (self‑optimizer state) |

**Composable flow:** `create_vault → add_lens ×(diverse) → ingest → anchor → bits/abundance → kernel → search/kernel_answer → guard.calibrate/guard.check → propose_lens → provenance/reproduce`.

**Other surfaces:** a command‑line tool mirrors the verbs (with a `{code,message,remediation}` JSON error envelope); a daemon exposes metrics and, when configured, a production tool server over a loopback with mutual‑TLS; a read‑only HTTP API exposes health/measure/search/guard/kernel/provenance behind bearer auth. **In‑process embedding** (depending on the crates directly) is often the best option for latency and determinism.

---

## 9. Recipes

1. **Stand up intelligence over a corpus.** create vault → add a diverse lens set → ingest → anchor outcomes → read `bits`/`abundance` → derive the `kernel` → query with `search`/`kernel_answer`.
2. **Make a panel diverse & sufficient.** enumerate many lenses → let the capability gate admit/park/retire → check effective rank for real diversity → test sufficiency; if insufficient, follow the deficit and `propose_lens`.
3. **Discover relationships.** weave cross‑terms (within‑record) + the between‑record nearest‑neighbor graph → `traverse`.
4. **Find causal/temporal structure.** timestamps + temporal lenses + recurrence series → transfer entropy (direction) → periodicity + hazard (scheduling) → next‑occurrence prediction.
5. **Guard generation.** `guard.calibrate` per slot → `guard.check`/`guard_generate` fail‑closed on out‑of‑distribution → identity‑lock canonical entities. Everything logged.
6. **Predict / impute.** consequence what‑if → reverse abduction for root cause → completion to fill missing fields from the trusted region (honesty‑gated).
7. **Real‑time / deterministic consumers.** keep Calyx at authoring/agent/analytics time. For a hot loop, **lower** the needed intelligence (kernel, bits, associations, imputed values, predictions) into fingerprinted, reproducible frozen data and consume that. Never call Calyx from a deterministic tick.
8. **Universal DB usage.** model relational/document/KV/time‑series/graph/vector/full‑text as collections and lenses over the one core — don't bolt on a second engine.
9. **Self‑optimize.** enable the shadow‑tested reversible tuning loops toward the objective; watch tripwires; every change logged and rollback‑able.
10. **Prove it.** a return value is a claim; the source of truth is the bytes. Read the stored rows / ledger entries / index artifacts and compare observed vs. expected — a green test is supporting evidence, not proof.

---

## 10. Cross‑domain applicability — the same mechanics, any domain

Calyx does not know or care about your domain. The mapping is always the same: pick what a *record*, a *lens*, an *anchor*, and the *kernel* are, and every capability follows. Illustrations (all abstract):

| Domain | A record is… | Example lenses (encode / embed) | An anchor (outcome) | What the kernel gives you | Headline capabilities |
|---|---|---|---|---|---|
| **Risk / fraud** | a transaction or account | amount (z‑score/log), category (one‑hot), device id (hash), merchant graph position, time (temporal), memo (text embed) | fraud confirmed (label) | the accounts/patterns that explain fraud | bits per feature, synergy of feature pairs, OOD guard on new accounts, what‑if on a rule change |
| **Clinical / registry** | a patient‑episode | labs (z‑score), diagnosis codes (multi‑hot), meds (set), care‑graph position, admit time (temporal), notes (text embed) | outcome/readmission (label) | the cohort that explains outcomes | sufficiency of the feature panel, causal arrows over time, imputation of missing labs, grounded Q&A |
| **IoT / sensors** | a device‑window | readings (scalar variants + cyclic), state (ordinal), firmware (categorical), topology position, timestamp | failure/anomaly (label) | the sensors/patterns that explain failures | periodicity + overdue hazard, drift alarms, next‑fault forecasting, blind‑spot detection |
| **Commerce / catalog** | a product/listing | price (log/rank), attributes (one‑hot/set), supplier graph, title+description (text embed) | converted/returned (label) | the catalog core that explains conversion | find‑similar, dedup, description‑vs‑attributes mismatch, what‑if on price |
| **Observability / logs** | an event/trace | level (ordinal), service (categorical), latency (z‑score), dependency graph, time (temporal), message (text embed) | incident/SLO breach (label) | the signals that explain incidents | causal transfer entropy across services, anomaly, forecasting, grounded root‑cause abduction |
| **Recommendation** | a user–item interaction | user/item ids (hash/entity‑embed), context (categorical), recency (temporal), item text (embed) | engaged/retained (reward) | the interactions that explain retention | neighbors, hybrid fusion, sufficiency of the signal set, propose‑lens growth |
| **Science / literature** | a paper/experiment/entity | measurements (scalar), entities (graph position), abstract (text embed), date (temporal) | validated/replicated (label) | the evidence core of the field | kernel answer over the corpus, cross‑domain bridges, grounding gaps |
| **Documents / knowledge** | a document/chunk | length/structure (scalar), type (categorical), links (graph), body (text embed) | useful/cited (thumbs/reward) | the documents that ground the knowledge base | semantic search + rerank, dedup, guarded retrieval, provenance |

The pattern is invariant: **structured fields → deterministic encoders + bits; latent content → embedders; time → temporal lenses; relationships → graph‑structural encoders + the between‑record graph; outcomes → anchors; then associate → differentiate → kernel → compose.**

---

## 11. Tuning & operations

- **Sample sizes.** Mutual‑information and calibration need a floor (≈50 paired samples; more for stable CIs). Below that, results are provisional; the honesty gate and small‑sample posteriors are your friends. Interaction information needs more (three‑way), transfer entropy needs enough lagged pairs.
- **The two contract knobs.** Signal floor (≈0.05 bits) and correlation ceiling (≈0.6) govern lens admission. Raise the floor to prune aggressively; lower the ceiling to demand more diversity. Watch effective rank — if `n_eff ≪ N`, your panel is redundant, not diverse.
- **Sufficiency deficits are a to‑do list.** An insufficient panel names the weakest slots; act on the largest deficits first via `propose_lens` or a better anchor.
- **Guard thresholds.** Tighter τ (higher) = fewer false accepts, more refusals/new‑regions; looser = the reverse. Calibrate per slot against real bad cases; recalibrate when the distribution shifts (watch drift alarms and tripwires).
- **Search caps & fusion.** Keep slot count and ef within the planner caps; RRF is robust across heterogeneous slots — prefer it over hand‑weighting unless you have measured reason.
- **Quantization.** Quantize only while recall/bits/false‑accept hold; the runtime fails closed on intelligence loss and falls back to raw. Use gentler levels for identity/guard slots.
- **Determinism.** Seed all randomness and inject the clock; identical input under the same panel must produce the same id and vectors. Use the determinism probe on new lenses.
- **Provenance & reproduce.** Every mutation writes a ledger entry; `verify_chain` on a schedule; `reproduce` to prove a result re‑derives within tolerance. This is your audit and your regression detector.
- **Real‑time boundary.** Never call Calyx from a latency‑critical deterministic loop; lower the needed intelligence to frozen, fingerprinted artifacts and consume those; prove the hot path reads only lowered data.
- **Failure modes to expect (all fail closed):** insufficient samples, low signal, redundant lens, ungrounded kernel, out‑of‑distribution, chain broken, non‑reproducible drift, quantization intelligence loss, back/disk pressure, stale derived. Treat the error code as the reason; never swallow it.

---

## 12. Binding principles (the universal doctrine)

- **First principles, ground‑up associations (foundational).** Decompose everything to atomic, independently‑measurable units and compute **all** base associations among them from the ground up — that exhaustive association structure is the information Calyx derives intelligence from. Never shortcut the decomposition or leave associations uncomputed; partial association structure ⇒ partial, ungrounded intelligence.
- **Record = constellation · no‑flatten** (slots typed & separate; guards per‑slot) · **frozen lenses** (never mutate; drift ⇒ a new id) · **hot‑swap** (add/retire/park + lazy backfill).
- **Grounding mandatory** — measure against anchored outcomes; ungrounded ⇒ *provisional*. Gates use a real **diverse** panel; a single embedder or synthetic check is diagnostic only.
- **Embed what is latent, encode what is explicit** (§5). Never embed an already‑meaningful value. Dual‑encode text labels on structured records.
- **Differentiation contract** — a bit floor and a correlation ceiling govern admission. **Never sell derived `C(N,2)` signal past the data‑processing‑inequality ceiling.**
- **Kernel at any scope** · **guard wherever generation touches the store** · **fail closed** (a structured error, never a silent fallback/mock) · **measured compression**.
- **The backbone rule** — *plug‑in lenses is THE key*: a new lens is one call, its value one number, the kernel one call at any scope. Reject anything that makes this harder.
- **Single source of truth** — structured data lives in Calyx and nowhere else; extend Calyx rather than adding a side store.
- **Provenance always** · **one change at a time** · **verify against the bytes** (no green‑checkmark stand‑ins).

**Anti‑patterns (refuse):** flattening the panel · embedding an already‑explicit value · selling associations past the ceiling · labeling ungrounded output "trusted" · mutating a frozen lens · making lens plug‑in harder · bolting on a separate search/graph/vector DB · a side store as a source of truth · a harness standing in for real verification.

---

## 13. Master reference — formulas, constants, errors

| Item | Value / formula |
|---|---|
| Association yield per input | `N + C(N,2) + 1`, where `C(N,2) = N(N−1)/2` |
| Differentiation contract | signal ≥ ~0.05 bits; max pairwise correlation ≤ ~0.6; ≥ ~50 samples |
| Panel sufficiency | `sufficient ⇔ I(panel;anchor) ≥ H(anchor)` (no slack) |
| Total correlation / effective rank | `TC = Σ H(slotₖ) − H(Φ)`; `n_eff ≈ n·(1 − TC/Σ H_marginal)` |
| Transfer entropy | `T(A→B) = I(B_f; A_p,B_p) − I(B_f; B_p)` |
| Interaction bits (pair gain) | `gain = pair_bits − max(left_bits, right_bits)` |
| Mutual information estimator | k‑NN (max‑norm joint radius; digamma terms); `bits = nats / ln 2`; sample floor ~50 |
| Agreement (cross‑term) | cosine `Σaᵢbᵢ / (√Σaᵢ² · √Σbᵢ²)`; edge weight = clamp to `[0,1]` |
| Kernel selection | greedy on `0.40·degree + 0.40·betweenness + 0.20·groundedness`; ~1% feedback‑vertex‑set; recall gate ~0.95 |
| Answer / reach hop score | `edge_weight · 0.9^hop` |
| Fusion | Reciprocal Rank Fusion, `K = 60`; BM25 k1≈1.2, b≈0.75 |
| Guard | per‑slot cosine ≥ conformal τ; all‑required or k‑of‑n; cold‑start τ ≈ 0.7 |
| Consequence tree | depth ≈ 4; per‑hop attenuation ≈ 0.7; prune below ≈ 0.05; reverse depth ≈ 3 |
| Grounded confidence | `n/(n+1)` (monotone, never reaches 1) |
| Content address | 128‑bit hash over length‑delimited ordered parts |
| Quantization | measured; CPU↔GPU bit‑parity; compress only within a signal/similarity/false‑accept bound |

**Error discipline.** Every failure returns `{code, message, remediation}` and fails closed. Representative codes: lens frozen‑violation / dim‑mismatch / numerical‑invariant / unreachable; assay insufficient‑samples / low‑signal / redundant; kernel ungrounded; guard provisional / out‑of‑distribution; oracle insufficient; ledger chain‑broken / corrupt / append‑only‑violation; reproduce non‑deterministic / drift‑exceeded; quantization intelligence‑loss; back‑pressure / disk‑pressure; stale‑derived; dataset checksum/rowcount/schema mismatch. **Treat the code as the reason; never degrade to a silent fallback.**

---

## 14. Glossary

**Association‑native** — the primitive is the relationship between measurements, not a row. **Constellation** — one input measured through a panel of lenses. **Lens** — a frozen measurement instrument. **Slot** — one lens's output in a record. **Panel** — the versioned lens set. **Cross‑term** — a derived association between two slots (agreement/delta/interaction/concat). **Derived Data Abundance** — the `N + C(N,2) + 1` signal count. **Anchor** — a grounded real outcome. **Bits** — mutual information about an anchor. **Sufficiency** — `I(panel;anchor) ≥ H(anchor)`. **Effective rank** — the count of truly non‑redundant lenses. **Transfer entropy** — directional temporal dependence. **Kernel** — the minimal core of records that explains a corpus. **Guard** — the per‑slot fail‑closed out‑of‑distribution gate. **Oracle** — grounded consequence prediction / completion. **Honesty gate** — refuses to answer when the panel can't carry the outcome's bits. **Lowering** — freezing Calyx intelligence into deterministic data for real‑time consumers. **Grounding gap** — a region of the corpus lacking anchors. **Verify against the bytes** — read the stored source of truth; don't trust the return value.

---

*Pin to a specific Calyx commit — interfaces and on‑disk formats are pre‑1.0 and unstable. Constants above are Calyx defaults; treat them as tunable. For exact signatures, on‑disk formats, and per‑crate detail, read the Calyx source and its complete technical documentation.*
