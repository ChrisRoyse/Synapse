# compressionprompt.md — Universal Doctrine for Maximum-Signal, Minimum-Token Prompts

**Scope.** Domain-agnostic. Applies to instructions, system prompts, agent doctrines, technical specs, RAG context, few-shot exemplars, tool descriptions, function-call schemas, and any other text you hand to an LLM and want it to *act on*.
**One-line thesis.** Compress along **specificity per token**, not **ambiguity per token**. Cut what the model can regenerate from priors; keep verbatim what has no prior (numbers, paths, names, deny-lists, error codes). Replace prose with structure. Replace structure with canonical terms when the term *is* the structure.

---

## 0. The frame: a prompt is a lossy code

A prompt is a **source code for behaviour**. The LLM is the **interpreter**. Tokens are the **wire format**. Compression is the engineering discipline of making the source as short as possible while preserving the behaviour it specifies.

Three quantities matter:

| Symbol | Name | Meaning |
|---|---|---|
| `T` | Token count | What you pay for, what fills the window, what slows decode |
| `S(P)` | Signal | The set of behaviour-determining constraints the prompt commits the model to |
| `H_model(P)` | Residual ambiguity | The size of the hypothesis space the model still has to guess across after reading `P` |

The objective is **maximise `S(P) / T`** while keeping **`H_model(P)` below the threshold where guessing produces wrong behaviour**. Compression that lowers `T` but raises `H_model` is *worse than no compression*: you saved tokens and bought hallucinations.

This is Shannon's source coding theorem applied backwards. Shannon (1948) showed the entropy of English text is ~50% redundant — meaning a perfect coder could halve the bits while preserving the message. **Modern LLMs already are arithmetic coders** that achieve close to the per-token entropy floor when *reading* text. So prose into an LLM is already partially compressed. Your job as prompt author is to remove the redundancy the model can reconstruct, while not removing the load-bearing specifics it cannot.

---

## 1. The two thermodynamic limits

There are two ceilings on how short a prompt can get without behavioural loss. Knowing both keeps you from chasing the wrong one.

### 1.1 Shannon limit (statistical)

The lower bound on encoded length is `-log p_model(text)`. Tokens that the model would have predicted anyway from prior context carry near-zero information and can be removed. This is the principle every entropy-based prompt compressor (Selective Context, LLMLingua, LLMLingua-2) exploits: rank tokens by self-information under a small reference model, drop the low-self-information ones. LLMLingua-2 achieves 5x compression with <1% performance drop on MeetingBank; LongLLMLingua reports 17.1% performance *gain* at 4x compression in retrieval-augmented QA.

### 1.2 Kolmogorov limit (algorithmic)

The lower bound on description length is the length of the shortest program that produces the meaning. Place notation ("100" = ten tens) is logarithmically smaller than tally marks because it introduces a recursive definition. Dirac bra-ket notation is exponentially smaller than the matrix-formulation prose it replaces because it offloads structure onto symbols the reader already understands. This is the principle that makes **canonical terms-of-art** (STRIDE, RBAC, idempotent, eventual consistency, fail-closed) the cheapest unit of meaning available: each term is a single token (or two) that decodes into a multi-paragraph definition the model has memorised from training.

The two limits are not the same. Shannon compression removes what's **statistically redundant**. Kolmogorov compression removes what's **definitionally redundant**. The first is *automated* (and what entropy-based compressors do). The second is *editorial* and the larger lever for human-authored prompts.

### 1.3 The third, ignored limit: instruction-following budget

LLMs have a measurable degradation in instruction-following as the number of explicit requirements increases — recent work shows performance can drop ~19% as more requirements are tacked on, even when each is individually correct. **Specifying everything is not free**; the model has limited attention to spend on rules. So the true objective is:

> Make every token a constraint the model would not have inferred on its own, and stop before the model's rule-tracking budget runs out.

This is why "underspecified" and "overspecified" are both failure modes, and why removing redundant restatements often *improves* performance rather than degrading it.

---

## 2. What to keep verbatim (the No-Compress List)

Before any compression technique, draw the boundary. The following have **no model prior to fall back on** — if you compress, paraphrase, or summarise them, meaning is lost silently and the model will plausibly hallucinate plausible substitutes.

| Category | Example | Why uncompressible |
|---|---|---|
| Numbers and thresholds | `≥ 0.95`, `300×8`, `1500 lines`, `EWC++` | No prior; the model will round, drop, or invent a different threshold |
| File paths and module names | `crates/context-graph-mejepa/src/lib.rs` | Hallucinatable; one character wrong = nonexistent file |
| Error codes / sentinel strings | `MEJEPA_INSTRUMENT_GRADIENT_LEAK`, `CCREALITY_ENGINE_RETIRED` | These are protocol; paraphrase breaks the contract |
| Named entities | issue numbers (`#406`), commit hashes, model IDs (`claude-opus-4-7`) | Single identity; any drift renders the reference useless |
| Deny-lists and allow-lists | `.env`, `swebench/**`, `tests/oracle_**` | The model's prior says "be cautious of .env"; it does *not* know the specific paths your project bans |
| Verbatim API names | `gh issue edit`, `cargo clippy --no-deps`, `rocksdb::CF_MEJEPA_*` | These are spellings the LLM must reproduce exactly |
| Direct quotations and required phrasings | structured comment headers, the words a tool's output must contain | Substitution breaks downstream parsing |
| Decision boundaries | "fail closed", "never use `git push --force`" | Inverting one word flips the rule |

**Rule.** Anything that the system depends on being **byte-exact** is uncompressible. Compression budget is spent on the *prose around* these anchors, not the anchors themselves.

---

## 3. The compression hierarchy (cut in this order)

When tightening a prompt, work top-down. Each level is more conservative than the one above; if a level damages behaviour, stop and back up to the previous one.

```
Level 1 — Cut what is meta-noise about the prompt itself      (always safe)
Level 2 — Cut restatement and example redundancy              (almost always safe)
Level 3 — Replace prose with structure                        (usually safe; test)
Level 4 — Substitute canonical terms-of-art                   (safe if reader = LLM with that prior)
Level 5 — Use symbolic / mathematical notation                (safe for instruction-shaped content)
Level 6 — Strip function words (telegraphic / headlinese)     (use sparingly; ambiguity risk rises)
Level 7 — Drop low-self-information tokens (LLMLingua-style)  (only with reference-model rerank; not by hand)
```

Beyond Level 7 lies **semantic loss**: you are now removing meaning, not encoding.

---

## 4. Technique catalogue

Each technique below has: **name**, **what it removes**, **mechanism**, **expected savings**, **risk**, **example**. Savings figures are typical English-language English-prompt ranges based on published compression studies (LLMLingua / LLMLingua-2 / Selective-Context / SDE / MetaGlyph) and standard editorial practice.

### 4.1 — Strip self-referential meta

**Removes.** "In this prompt", "the following instructions", "please carefully", "I want you to act as", "your task is to".
**Mechanism.** The model already knows it's reading a prompt. Saying so is pure overhead.
**Savings.** 5–15% for typical first drafts.
**Risk.** None.
**Before / after.**
> ❌ "I would like you to please carefully consider the following task. Your task is to review the code below and identify bugs."
> ✅ "Review the code below. List bugs."

### 4.2 — Delete restatement

**Removes.** Phrases that re-say a previous sentence in different words ("In other words…", "Put differently…", "What this means is…").
**Mechanism.** A second statement of `X` adds zero entropy if `X` was already clear. It adds *negative* entropy if it subtly drifts and creates two slightly different rules.
**Savings.** 10–30% in policy-style documents.
**Risk.** None unless the restatement was actually clarifying a known ambiguity (rare).

### 4.3 — Delete examples the model can regenerate

**Removes.** Trivial examples that demonstrate textbook patterns the model has memorised.
**Mechanism.** "For example, `2 + 2 = 4`" teaches the model nothing about arithmetic. Keep examples only when they encode a **non-obvious** convention, an **edge case**, or a **stylistic constraint** the model would otherwise miss.
**Savings.** 20–50% in few-shot prompts that were copied from tutorials.
**Risk.** Cutting an example that *was* load-bearing for a corner case. Mitigation: keep one example per **non-trivial** behaviour, none per **trivial** behaviour.

### 4.4 — Defined terms (legal-drafting compression)

**Removes.** Repeated long noun phrases.
**Mechanism.** Define once, refer by short tag. UK and US legislative drafting offices recommend defined terms only when (a) the phrase repeats enough that summing the savings > the cost of the lookup, and (b) the tag is **intuitive** — the form itself hints at the meaning. The NZ Parliamentary Counsel Office and Uniform Law Commission both warn that defined terms whose meaning is *not* intuitive cost more than they save because the reader has to thrash back to the definition.
**Savings.** 10–40% in policy docs with repeated jargon.
**Risk.** Bad tags. `the Specified Quantity` is worse than `n`. `the Compression Threshold` is worse than `compression_threshold = 0.95`.
**Rule.** Define a term only if it (a) repeats ≥ 3 times, (b) replaces > 8 words each time, and (c) the tag is self-descriptive.

### 4.5 — Canonical terms-of-art substitution

**Removes.** Long descriptions of standard concepts.
**Mechanism.** Replace the description with the field's accepted name. Each canonical term is a single token (or two) that the model decodes into its full meaning from training. The cost of the term is `O(1)` tokens; the cost of the description is `O(meaning)`. Examples:

| Long form | Canonical | Tokens saved (typical) |
|---|---|---|
| "a way of authenticating users where a server gives them a signed token they then present with each request" | "JWT" or "bearer token" | ~20 |
| "the property that running the operation twice gives the same result as running it once" | "idempotent" | ~15 |
| "if any unknown condition is hit, refuse rather than allowing" | "fail-closed" | ~10 |
| "splitting a system so each component has only the access it needs" | "least privilege" | ~10 |
| "the model produces text it cannot back up with a source" | "hallucination" | ~10 |
| "process where each new value is at least as large as the last" | "monotonic" | ~8 |
| "Threat modelling acronym: Spoofing, Tampering, Repudiation, Info-disclosure, DoS, Elevation" | "STRIDE" | ~25 |

**Risk.** Audience mismatch. The substitution works because the *LLM* reader has the prior. Verify by asking a fresh-session LLM to expand the term — if its expansion matches your intent, the substitution is safe. If the term has multiple senses (e.g., "consistent" in distributed systems vs. UI design), keep the disambiguator.

### 4.6 — Symbolic notation (MetaGlyph principle)

**Removes.** Prose connectors of logical structure.
**Mechanism.** Mathematical/logical symbols (`∈`, `∉`, `⇒`, `¬`, `∩`, `∪`, `∀`, `∃`, `≥`, `≤`) are single tokens that the model has seen in *millions* of math, logic, and code examples. They carry exact, stable meaning. Recent work (MetaGlyph, 2026) reports 62–81% token reduction across instruction tasks by substituting these symbols for English equivalents. Example:

> ❌ "The user must be a member of either the Admins or Owners group, but not the Guests group, and they must have completed two-factor authentication."
> ✅ "`user ∈ {Admins, Owners} ∧ user ∉ Guests ∧ 2FA(user) = true`"

**Savings.** 40–70% for instruction-shaped rules.
**Risk.** Symbol overload reduces *human* readability and may confuse small models. Use sparingly for the highest-density rules. Don't invent novel symbols — only use ones the model has seen as math/logic during pre-training.
**Stable symbols that work.** `∈ ∉ ⊂ ⊆ ∪ ∩ ¬ ∧ ∨ ⇒ ⇔ ∀ ∃ ≥ ≤ ≠ ≈ → ←` plus common code operators (`==`, `!=`, `&&`, `||`).
**Unstable symbols.** Domain-specific glyphs (∅ for null vs. empty set varies; arrows in linguistics ≠ arrows in proof theory). Test with the target model.

### 4.7 — Structure over prose (tables, lists, key:value)

**Removes.** Sentence scaffolding ("there is", "which is", "that has", "in order to").
**Mechanism.** A 2-column table conveys a one-to-one mapping in `N + N + 2` tokens. The equivalent prose ("X corresponds to A, Y corresponds to B, Z corresponds to C") needs `~5N` tokens and forces the model to do the alignment work itself.
**Savings.** 30–60% for any factual mapping, threshold list, or rule set.
**Risk.** Reasoning tasks. Structured generation constraints (JSON mode in particular) can **hurt** reasoning by forcing the model to produce structure before thought (one study found GPT-3.5-turbo dropped from 71% to 49% on a reasoning task when forced into JSON-mode with schema constraint). **Use tables for facts, prose for reasoning.**
**Rule.** If the content is `(key, value)` or `(condition, action)` or `(item, property)`, use a table or list. If the content is a chain of inference, keep it as prose.

### 4.8 — Format choice (markdown > YAML ≈ JSON for instructions)

**Removes.** Punctuation/quoting overhead.
**Mechanism.** Empirical comparison across GPT, Claude, Gemini shows that the optimal input format for instruction-shaped content is usually **markdown** (because the model has seen the most pre-training in this format), with **YAML** close behind for structured config-style content. JSON is **noisier** as input (every value double-quoted, every key escaped) but is the right output format when downstream parsing requires it. TOON (Token-Oriented Object Notation) is ~25% denser than JSON for the same payload. Plaintext is the densest but loses structure cues.
**Savings.** 10–25% switching from JSON-input to markdown- or YAML-input.
**Risk.** Some models (especially reasoning-tuned) are robust across formats; others have measured ±5–10% accuracy differences. Test, don't assume.
**Recommendation.** Use markdown headings for sections, bullet lists for parallel items, tables for mappings, fenced code for code, and YAML or TOON for nested config. Reserve JSON for tool-call payloads.

### 4.9 — Headlinese (function-word omission)

**Removes.** Articles (`the`, `a`), expletive subjects (`there is`, `it is`), redundant auxiliaries (`is being`, `has been`), redundant relative pronouns (`which`, `that`, `who`).
**Mechanism.** Newspaper headline syntax has been studied for a century as a register optimised for maximum information at minimum length. Lemke et al. (2017) showed article omission in headlines is constrained by information theory: articles get dropped precisely when the head noun is locally predictable. The LLM has the same predictability; for instruction-shaped content, articles add no information.
**Savings.** 5–15%.
**Risk.** Reading speed drops for humans. If the prompt is also read by humans, set a threshold of "remove only where the meaning is still unambiguous in the local context". If the prompt is read only by the model, be aggressive.
**Example.**
> ❌ "When the file that contains the configuration is missing, the system should fail in a way that produces a clear error message."
> ✅ "Missing config file → fail with clear error."

### 4.10 — Imperative voice, present tense

**Removes.** Modal hedges (`should`, `would`, `might`), passive voice scaffolding (`is to be done`), future tense (`will be doing`).
**Mechanism.** Strunk & White Rule 11 ("Put statements in positive form") and Rule 12 ("Use the active voice"). ASD-STE100 makes active voice mandatory for the same reason: less ambiguity per word.
**Savings.** 10–20%.
**Risk.** Modal hedges that mark genuine optionality (`MAY`, `SHOULD`, `MUST` per RFC 2119) carry real meaning and must stay if optionality matters.
**Example.**
> ❌ "The build process should be made to be capable of being interrupted by the operator."
> ✅ "Operator can interrupt build."

### 4.11 — Positive over negative

**Removes.** Double negatives, "not un-" constructions, and contraposed rules.
**Mechanism.** Negation is cognitively and statistically costly. "Don't fail to verify" is two negatives where "verify" is one positive. Strunk & White Rule 11.
**Savings.** 5–10%.
**Risk.** Some rules are genuinely about prohibition; preserve them as positive statements of the prohibition ("Never push without verification") rather than smuggling them into double-negated form.

### 4.12 — One instruction per sentence (ASD-STE100)

**Removes.** Compound, multi-clause directives.
**Mechanism.** Simplified Technical English (ASD-STE100) — the controlled-language standard for aerospace maintenance documentation — mandates one instruction per sentence because mis-execution of compound instructions is the documented failure mode that gets aircraft killed. The same applies to LLMs: compound instructions are where models drop steps.
**Savings.** Variable; sometimes increases token count slightly but **always** improves instruction-following.
**Risk.** None. This is a robustness move that happens to also tighten prose.
**Example.**
> ❌ "After receiving the user input, validate it for SQL injection patterns and HTML escape it before storing it in the database while also logging the attempt."
> ✅ "Validate input for SQL injection. HTML-escape it. Store it. Log the attempt."

### 4.13 — Controlled vocabulary (one word per concept)

**Removes.** Synonym sprawl (`start / begin / commence / initiate / kick off`).
**Mechanism.** ASD-STE100's most powerful rule: pick **one word for each meaning** and stick to it across the whole document. The model otherwise has to spend attention deciding whether `commence` and `start` mean the same thing in context. Standardising vocabulary across a prompt corpus eliminates that overhead and reduces the prompt corpus's overall entropy.
**Savings.** Not direct (per-prompt savings are small); the win is in consistency and the ability to grep-audit your own prompt.
**Risk.** None.
**Rule.** Pick a verb for each action (start, stop, verify, read, write, fail) and never use a synonym.

### 4.14 — Anaphora and ellipsis (controlled)

**Removes.** Repeated noun phrases.
**Mechanism.** Pronouns and ellipsis are free in human language because humans track discourse referents. LLMs do too, but only over a short window. Use anaphora **locally** (within a paragraph) for common subjects. Don't use it across long structural distances (don't say "it" in section 4 referring to something defined in section 1).
**Savings.** 5–10%.
**Risk.** **Unresolved anaphora is a documented driver of hallucination** — empirical work links anaphora-rich queries to higher hallucination rates. Use anaphora only when the antecedent is **unambiguous in the prior sentence**.

### 4.15 — Number and unit normalisation

**Removes.** Spelled-out numbers, redundant unit declarations.
**Mechanism.** `5` is one token; `five` is one token but the digit is more useful to a reasoning model. `300×8` is denser and clearer than "three hundred examples times eight categories". Numeric literals also survive translation and tokenisation better than spelled-out forms.
**Savings.** 1–5%. Small but free.
**Risk.** None.

### 4.16 — Abbreviation and acronym (tokenizer-aware)

**Removes.** Long compound technical names.
**Mechanism.** GPT-family tokenizers (`cl100k_base`, `o200k_base`) treat established acronyms as single tokens: `NLP`, `API`, `LLM`, `JWT`, `SQL`, `JSON`, `HTTP`. Replacing the spelled-out form once-defined is essentially free compression because the acronym lands in the model's pretraining vocabulary.
**Savings.** 5–20% in technical writing.
**Risk.** Ambiguous acronyms (`PR` = pull request? press release? proportional representation?). Define on first use if there's any chance of collision, then use the acronym.
**Tokenizer rule.** Before adopting an abbreviation, check that it tokenises to one or two tokens, not five. `tiktoken` makes this verifiable in seconds. Some abbreviations expand badly (`NeurIPS` may tokenise to four tokens; `ICLR` to two).

### 4.17 — Whitespace, punctuation, capitalisation hygiene

**Removes.** Trailing whitespace, double spaces, inconsistent capitalisation, redundant punctuation.
**Mechanism.** Tokenizer-level: a single trailing space changes the token. BPE tokenizers are extremely sensitive to leading whitespace and capitalisation. `" The"` and `"The"` and `" the"` are *three different tokens*. This matters most for **prefix caching**: a single character difference at the start of a cached prefix invalidates the cache. Cache miss rates above 40% can double effective cost.
**Savings.** Tokens directly: 1–3%. Cost savings via prefix caching: up to 50% on high-volume systems.
**Risk.** None.
**Rule.** Decide on whitespace conventions once for the whole prompt corpus and enforce them mechanically.

### 4.18 — Lists over numbered series

**Removes.** "First, do A. Second, do B. Third, do C."
**Mechanism.** Bullet or numbered lists with implicit ordering tokenise smaller than the prose enumerators.
**Savings.** 5–15%.
**Risk.** Order-sensitive procedures need explicit numbering (`1.`, `2.`, `3.`). For unordered constraints, use `-`.

### 4.19 — Hierarchical headings (progressive disclosure)

**Removes.** Repeated context-setting phrases.
**Mechanism.** A markdown heading carries the same context as the phrase "In the context of authentication," prepended to every paragraph in a section. Headings let the model condition on the section topic without paying for the topic restatement in every sentence.
**Savings.** 5–15% in long documents.
**Risk.** None if heading levels are consistent.

### 4.20 — Reference, don't restate

**Removes.** Inline restatement of policy/protocol defined elsewhere.
**Mechanism.** "Follow FSV protocol per `docs/futurebuild/specs/FSV-PROTOCOL.md`" is shorter than embedding the FSV protocol in every prompt. The model treats the reference as a pointer; if the referenced file is loaded into context separately (or via a tool), the reference resolves; if not, the model still understands the *type* of protocol intended.
**Savings.** Variable, but easily 50–90% when factoring out shared boilerplate.
**Risk.** If the referenced doc drifts, the prompt drifts. Reference only stable, versioned documents.

### 4.21 — Negative space (deny-list compression)

**Removes.** Enumerating every allowed case.
**Mechanism.** "Allowed: `.py`, `.rs`, `.toml`, `.md`, `.yaml`, `.json`, `.txt`, ..." (50 lines) vs. "Allow all source files. Deny: `.env`, `.pem`, `.key`". Negative space is almost always shorter when the deny set is bounded and the allow set is open.
**Savings.** 50–90% on permission-style content.
**Risk.** If your security posture requires allow-list discipline (not deny-list), don't invert. Allow-lists are safer; deny-lists are denser. The choice is policy, not compression.

### 4.22 — Type signatures as documentation

**Removes.** Prose descriptions of function inputs and outputs.
**Mechanism.** A type signature `fn verdict(diff: &PatchDiff, oracle: Oracle) -> Verdict` carries the same information as three sentences of prose. The model has strong priors for type-signature-shaped content.
**Savings.** 60–80% on API documentation.
**Risk.** None if the type language is mainstream (Rust, TypeScript, Python type hints, Haskell). Risk grows with exotic type DSLs.

### 4.23 — Examples as specification (input → output pairs)

**Removes.** Prose specifications of behaviour.
**Mechanism.** Two well-chosen `input → output` pairs often compress what would take a paragraph of "if-this-then-that" prose. Few-shot learning literature shows 2–5 examples is typically the sweet spot; beyond that, marginal value declines and token cost rises linearly.
**Savings.** 30–60% on behaviour-specification tasks.
**Risk.** Examples that under-cover the space create false generalisations. Pick examples on the **decision boundary**, not the easy cases.

### 4.24 — Tokenizer-aware phrasing

**Removes.** Phrases that tokenise badly.
**Mechanism.** Some phrasings encode into far fewer tokens than synonyms. "config" is 1 token; "configuration" is 2 in `cl100k_base`. "Do not" is 2 tokens; "Don't" is 1. "Will not" is 2 tokens; "won't" is 1 (and is shorter to read). At scale, choosing the tokenizer-friendly form across a system prompt of 4k tokens can save 200–400 tokens.
**Savings.** 2–10%. Pure free win if you don't change meaning.
**Risk.** Don't sacrifice clarity for tokenizer micro-optimisation. Apply only when the choice is genuinely a wash.

### 4.25 — Glossary at top, dense body below

**Removes.** Inline definitions throughout the body.
**Mechanism.** Define every project-specific term once at the top in a glossary table; the body uses them densely without re-introducing. The model conditions on the glossary while parsing the body. This is the structure of the ASD-STE100 dictionary + writing rules. It's also what every well-drafted legal statute does.
**Savings.** Compounds with body length; a 5-line glossary saves restatement across hundreds of body lines.
**Risk.** None if the glossary stays in context with the body.

### 4.26 — Constraint over instruction

**Removes.** Procedural step lists when a postcondition would do.
**Mechanism.** "Produce output that satisfies: schema X, length ≤ 200 tokens, no profanity" is shorter and more robust than "First check the schema, then check the length, then check for profanity, then..." Modern LLMs are better at satisfying declared constraints than executing prescribed procedures, especially for self-checking tasks.
**Savings.** 20–40%.
**Risk.** If you genuinely need a specific *procedure* (because side effects matter), don't compress to a postcondition.

### 4.27 — Density via density (the Semantic Density Effect)

**Removes.** Padding, hedging, low-content tokens.
**Mechanism.** Empirical work on Semantic Density Effect (SDE) measures `(S − R) × C / W` where `S` = semantically loaded tokens, `R` = redundancy fraction, `C` = concreteness, `W` = total words. Prompts with SDE > 0.80 outperform diluted counterparts by ~8.4 percentage points on average, with zero token overhead. Density is a measurable property and it *causes* better outputs, not just cheaper ones.
**Savings.** Compresses while *improving* accuracy when done right.
**Risk.** Density chased blindly degenerates into ambiguity. The SDE formula reminds you that concreteness (`C`) is a multiplier — dense **and** specific beats dense alone.

### 4.28 — Compress few-shot exemplars

**Removes.** Verbose example narratives.
**Mechanism.** Few-shot examples follow Pareto: a handful of decision-boundary examples dominate. Rank exemplars by mutual information with the target task and keep top-`k`. LongLLMLingua reports 4x reduction on retrieval-augmented contexts *with performance improvement*.
**Savings.** 50–90%.
**Risk.** Same as 4.23 — under-coverage. Mitigation: select exemplars by adversarial difficulty, not by surface frequency.

### 4.29 — Default-elision (assume sensible defaults)

**Removes.** Stating defaults the model already assumes.
**Mechanism.** "Use English" — the model defaults to English. "Be concise" — increasingly a system-level default. Find your model's strong defaults and stop restating them. *But* — make explicit the defaults you want **changed** ("Use Rust 2024 edition, not 2021").
**Savings.** 5–15%.
**Risk.** Defaults shift between model versions. Document which defaults you rely on so you catch regressions when models upgrade.

### 4.30 — Comment compression (code inside prompts)

**Removes.** Verbose code comments.
**Mechanism.** Code blocks inside prompts cost tokens like any text. Strip non-load-bearing comments from example code; keep only those that mark **why** (constraint, invariant, gotcha), per the standard "no WHAT comments" rule.
**Savings.** 10–30% in code-heavy prompts.
**Risk.** None.

### 4.31 — Algorithmic compression (LLMLingua-class)

**Removes.** Tokens the model would have predicted from a smaller reference model.
**Mechanism.** Use a small reference LM (GPT-2 small, or distilled BERT-class compressor like LLMLingua-2) to score per-token self-information; drop low-score tokens up to a target ratio. State of the art: 5x compression with <1% performance drop on QA/summary; 20x at the extreme with notable degradation.
**Savings.** 3x–20x.
**Risk.** Resulting text is no longer human-readable, may be brittle across model upgrades, and silently drops constraints you didn't realise were low-entropy from the compressor's view. **Only safe when you can A/B test against the original.**
**Use when.** Prompt is hot-path, repeated millions of times, and you have an evaluation harness. Don't apply to one-shot prompts.

### 4.32 — Soft prompts / embedding compression

**Removes.** The text entirely.
**Mechanism.** Train a continuous embedding (soft prompt, prompt tuning, AutoCompressor) that replaces a long text prompt with a few learned vectors. Out of scope for hand-authoring but worth knowing as the theoretical end of the compression curve.
**Savings.** 100x+ tokens, at the cost of model-specific training and complete loss of human-readability.
**Risk.** Not portable across models; not auditable; not editable.

---

## 5. The universal procedure

A repeatable, domain-agnostic protocol for compressing any prompt.

### 5.1 — Define the load-bearing set

Before cutting, list every concrete commitment the prompt makes: every number, path, error code, identity, threshold, allow/deny entry, required output format. This is your **No-Compress List** (§2). Highlight or mark it. The rest is the **compression budget**.

### 5.2 — Categorise the rest

For each remaining sentence, tag it:

| Tag | Meaning | Action |
|---|---|---|
| `meta` | Talks about the prompt itself | Delete (4.1) |
| `restate` | Re-says a prior sentence | Delete (4.2) |
| `example-trivial` | Demonstrates a textbook pattern | Delete (4.3) |
| `example-edge` | Demonstrates a non-obvious case | Keep, compress (4.3, 4.23) |
| `definition` | Defines a term used downstream | Lift to glossary (4.25), keep |
| `rule` | A constraint on behaviour | Densify (4.6, 4.10, 4.12) |
| `reasoning` | Chain of inference the model should follow | Keep as prose (4.7 caveat) |
| `boilerplate` | Repeated across prompts | Factor out via reference (4.20) |

### 5.3 — Apply Level 1–4 cuts

Sequentially. After each level, re-read the prompt as if you'd never seen it. Does behaviour still feel determined? If yes, continue. If a rule has become ambiguous, back up.

### 5.4 — Re-structure

After deletion, ask: is this still prose where it should be a table? Is this a procedure that should be a constraint? Is this a long noun phrase repeating that should be a defined term?

### 5.5 — Round-trip test

Open a **fresh** LLM session. Paste only the compressed prompt. Ask the model: "Expand this prompt into the full English instructions it implies. List every constraint." Diff against the original. Anything in the original that didn't survive the round-trip was load-bearing and you compressed too far. Anything that survived but you didn't author was a model-prior expansion — fine, as long as it matches your intent.

This test is the single most important step. It mechanically separates "compression" (lossless against the model) from "ablation" (lossy against the model).

### 5.6 — Measure

Track three numbers:

- **`T`** — token count via `tiktoken` (or model-native tokenizer)
- **`SDE`** — semantic density: `S(P) / T` (loaded tokens over total)
- **`H_model`** — quality of model output on a fixed evaluation set, before vs. after

Compression is *successful* iff `T` ↓ **and** quality holds (or improves). `T` ↓ with quality ↓ is just ablation in disguise.

### 5.7 — Stop

When further compression no longer satisfies §5.5 or §5.6, stop. Compression is a sub-linear-return discipline; the last 10% of size reduction often costs the last 30% of quality.

---

## 6. Anti-patterns (do not do these)

These look like compression but are not. They reduce `T` while reducing `S(P)/T`. Recognise and reject them.

### 6.1 — Vague-word substitution

Replacing a specific term with a more abstract one *seems* like compression but is ablation.

> ❌ Original: "Fail closed with `MEJEPA_INSTRUMENT_GRADIENT_LEAK` if a gradient is detected flowing into a frozen-target embedder slot."
> ❌ "Compressed": "Handle gradient errors safely."

The "compressed" version saved tokens by deleting the specific failure mode, the specific error code, the specific architectural invariant, and the specific direction (fail closed, not fail open). It is shorter but tells the model *nothing it can act on*. This is the failure mode the user this doctrine is written for is most prone to. Watch for it constantly.

### 6.2 — Synonym churn

Using a different word each time you mean the same thing — under the impression that variety is good prose — increases the model's disambiguation cost. ASD-STE100 forbids this for a reason.

### 6.3 — Over-symbolisation

Throwing math symbols at content that wasn't logical-structural. Symbols are leverage for logical structure (membership, implication, quantification). They are not leverage for narrative or reasoning. `the system shall ⇒ ...` is worse than `the system shall ...`.

### 6.4 — Compressing the reasoning chain

JSON-mode-with-schema is documented to *hurt* reasoning by ~20 percentage points on some benchmarks because it forces the model to produce structure before completing chain-of-thought. Don't compress reasoning into bullets if the model needs the prose to think.

### 6.5 — Cargo-cult acronyms

Inventing a project-specific acronym that the model has *not* seen during training. Your reader thinks it's compression; the model treats it as an opaque token. Pre-existing acronyms (`API`, `LLM`, `SQL`) carry meaning. Newly-coined (`ZPRT-7`, `the FQAM gate`) carry only a label.

### 6.6 — Compressing examples on the decision boundary

If your prompt has a few-shot example showing the exact edge case the model fails on without it, deleting that example "to save tokens" will silently degrade the very thing the example was buying.

### 6.7 — Ambiguity-as-compression

The failure mode that triggered this document. Replacing "fail-closed on unknown principals with structured error code `AUTHZ_UNKNOWN_PRINCIPAL`" with "handle auth safely" is ambiguity, not compression. The model fills in the *modal* interpretation, which may not be yours.

### 6.8 — Tokenizer-blind compression

Rewriting a phrase you thought was shorter but tokenises larger. `"cfg"` is 1 token, but `"cnfg"` is 2 because it's not in vocabulary. Verify with `tiktoken` before claiming any character-level win is also a token-level win.

### 6.9 — Compressing critical negatives

The word "never" in a deny rule cannot be paraphrased to "avoid" without weakening it. The word "must" cannot become "should" without inverting the obligation level. RFC 2119 keywords are an explicit compression discipline: every modal word carries a strict meaning. Respect it.

### 6.10 — Removing the citation

A claim with a source is auditable. A claim without a source becomes assertion. If your prompt cites a doc (`per FSV protocol §7`), the citation is the audit trail. "Compressing" by removing the citation breaks future verification.

---

## 7. Per-domain playbook

The universal procedure (§5) applies everywhere. Domain-specific levers vary by what the No-Compress List looks like.

### 7.1 — Software engineering instructions

| Lever | Notes |
|---|---|
| Keep verbatim | File paths, function names, type signatures, build commands, error codes, deny-lists |
| Compress hard | Tutorial-style explanations, motivation, "best practices" prose |
| Symbol over prose | Constraints (`function size ≤ 30 lines`), formats (`fn(x: T) -> U`) |
| Defined terms | `FSV`, `RCA`, `MCP`, `CLAUDE.md` — each replaces a paragraph |
| Format | Markdown headings + code blocks + tables. Avoid JSON in input. |

### 7.2 — Policy / governance / hardening (this project's `hardeningprompt.md`)

| Lever | Notes |
|---|---|
| Keep verbatim | RFC 2119 modal words (`MUST`, `MUST NOT`, `SHOULD`); axis names; deny-lists |
| Compress hard | Threat-model narrative; "why this matters" prose |
| Defined terms | Each axis becomes a tag (`§14.2 Security`) referred to by tag |
| Structure | Per-axis: name, primitives table, threat-model template, checklist |
| Reference | Cross-link related axes instead of re-explaining |

### 7.3 — Legal / contract / spec

| Lever | Notes |
|---|---|
| Keep verbatim | Defined terms, dates, sums, parties, jurisdictions, statute citations |
| Compress hard | Recitals, throat-clearing ("Whereas ..."), defensive over-specification |
| Defined terms | Mandatory; this is where the technique originated |
| Avoid | Synonym variation; one defined term per concept, used consistently |
| Format | Numbered clauses for cross-reference (every clause must be referenceable) |

### 7.4 — RAG / retrieval context

| Lever | Notes |
|---|---|
| Keep verbatim | Source-of-truth documents per their authoring |
| Compress hard | Adjacent-context noise via LLMLingua-class entropy filtering |
| Order | Place the highest-signal document at the start or end (lost-in-the-middle effect) |
| Re-rank | Use BM25 / embedding rerank before compression to maximise the signal density |

### 7.5 — Tool descriptions / function-call schemas

| Lever | Notes |
|---|---|
| Keep verbatim | Parameter names, types, required/optional flags, return shapes |
| Compress hard | "Use this when..." narrative |
| Format | JSON schema with `description` fields short and constraint-shaped |
| Examples | One or two; pick decision-boundary inputs |

### 7.6 — Customer-facing instructions / FAQs

| Lever | Notes |
|---|---|
| Keep verbatim | Product names, prices, support hours, contact methods |
| Compress hard | Marketing copy, generic reassurances |
| Caution | Domain-of-art substitution risks here are higher because the reader is *not* an LLM with deep priors; verify acronyms |

---

## 8. Worked examples

### 8.1 — Software engineering: a hardening rule

**Before (137 tokens):**
> One of the most important things to remember when writing code that handles authentication is that you should always make sure to fail in a closed manner when you encounter any kind of unknown or unexpected situation, because failing open in such cases can lead to security vulnerabilities where an attacker might be able to bypass authentication checks. It is important that you also produce a structured error code so that the calling code can handle the failure programmatically rather than having to parse free-form text.

**After (32 tokens):**
> Auth on unknown state: fail closed. Emit `AUTHZ_UNKNOWN_PRINCIPAL`. Never fail open.

**What got cut.** Meta-framing ("One of the most important things..."), restatement ("fail in a closed manner... failing open"), motivation explanation ("can lead to security vulnerabilities"), "important that" filler.
**What was kept.** The rule (fail closed), the error code (verbatim), the prohibition (never fail open), the precondition (unknown state).
**Compression ratio.** ~4.3x.
**Risk check (§5.5).** Round-trip: an LLM expands "Auth on unknown state: fail closed. Emit `AUTHZ_UNKNOWN_PRINCIPAL`. Never fail open." into the full meaning correctly because all four components have priors.

### 8.2 — Policy: deny-list compression

**Before (96 tokens):**
> The system supports many different file types as input, including but not limited to source code files in various programming languages such as Python (`.py`), Rust (`.rs`), TypeScript (`.ts`), JavaScript (`.js`), Go (`.go`), as well as configuration files like TOML (`.toml`), YAML (`.yaml`), and JSON (`.json`), and documentation files like Markdown (`.md`) and plain text (`.txt`). However, certain files should never be read or modified, including environment files, private keys, and credentials.

**After (24 tokens):**
> Read/write any source or config file. Deny: `.env`, `.env.*`, `*.pem`, `*.key`, `**/credentials.json`.

**Compression ratio.** ~4x.
**Why it works.** The "allow" side is open and known; the "deny" side is closed and unknown — exactly the §4.21 negative-space pattern.

### 8.3 — RAG: compressing few-shot exemplars

**Before (380 tokens, 5 exemplars).** A few-shot block where five `(question, answer)` pairs are presented in full prose narrative.

**After (180 tokens, 5 exemplars).** Same five pairs presented as:
```
Q: <question>
A: <answer>
```
with prose narrative stripped, and questions/answers themselves edited per §4.9 (function-word omission) and §4.10 (imperative voice).

**Compression ratio.** ~2.1x.
**Round-trip.** Model regenerates the original prose narratives correctly from the bare pairs because narrative is exactly what models reconstruct well.

### 8.4 — A real-world test: try this on a large doctrine prompt

Choose a large doctrine prompt with repeated axis/policy prose. Apply the procedure:

1. **Load-bearing set.** RFC 2119 keywords, axis names, primitive lists, deny-list paths, threshold numbers.
2. **Likely cuts.** Per-axis "why this matters" preambles, repeated "in this section we will..." framings, parallel restatements between universal and project-specific sections.
3. **Likely structural improvements.** Per-axis: replace the prose intro with a 4-row table — `Goal | Primitives | Threat-model template | Per-PR checklist`. Reserve prose for the case studies and worked examples.
4. **Reference compression.** Each axis cross-references the canonical operator doctrine for the universal rule and only keeps the project-specific delta locally.
5. **Expected reduction.** 40–60% by token count, with no behavioural loss (verify by §5.5 round-trip on a sample of three axes before applying to all 14).

---

## 9. When *not* to compress

Compression is not free. The following situations argue against further compression:

1. **The prompt is read once and discarded.** Engineering cost > token savings. Don't optimise.
2. **The reader is a human, not (only) an LLM.** Humans need more redundancy than LLMs. Don't strip below human readability if humans will audit, edit, or review.
3. **The audience LLM lacks the priors.** Compressing via canonical terms-of-art only works if the model has the prior. Verify with a fresh-session expansion test before deploying.
4. **The prompt is the legal/binding artefact.** Contracts, RFCs, statutes — clarity is the product, not a constraint to be optimised against.
5. **You haven't measured.** Compression without an evaluation harness is gambling. If you can't A/B test against the original, don't go below Level 4 in the hierarchy.
6. **The model is changing.** Aggressive Level 7 (algorithmic) compression is tuned to a specific reference model's entropy estimates. Major model upgrades can invalidate the compression silently. Re-test on upgrade.
7. **You're tempted to inject ambiguity to "save space".** This is the user's instinct that triggered this document. The answer is: no, ambiguity is not compression. See §6.1, §6.7.

---

## 10. Measurement: what does "good compression" look like?

Two empirical signals, both required.

**Signal A — token count.** Use `tiktoken` (or the model-native tokenizer). Don't approximate from word count. `pip install tiktoken; enc = tiktoken.encoding_for_model("gpt-4o"); len(enc.encode(text))`.

**Signal B — task performance.** Hold a frozen evaluation set of inputs that exercise the prompt's intended behaviour. Run before and after compression on the same model. **Compression is successful iff** task accuracy (or whatever quality metric you track) does not regress beyond a noise threshold while `T` drops.

**Composite metric.** SDE (Semantic Density Effect) approximates: `SDE(P) = (S − R) × C / W` where `S` = semantically-loaded tokens (specific nouns, active verbs, numbers, named entities), `R` = redundancy fraction (proportion repeating prior content), `C` = concreteness score (fraction of `S` that is concrete vs. abstract), `W` = total word count. Target `SDE > 0.8`. Empirically associated with +8 percentage points accuracy improvement at zero token cost.

**Pre-merge gate (suggested).** For shared prompt corpora (CLAUDE.md, system prompts, agent doctrines), institute a per-change check: `tiktoken` diff (must not regress without justification), 5-input eval (must not regress accuracy), `SDE` ≥ floor (won't admit dilution).

---

## 11. Quick-reference card

A 1-page summary suitable for printing or pinning into a system prompt header.

```
COMPRESSION DOCTRINE — universal rules

Keep verbatim:
  numbers • file paths • error codes • named entities •
  deny-lists • API names • RFC 2119 modal words • citations

Cut first (always safe):
  meta-self-reference • restatement • trivial examples •
  "please" / "kindly" / "I would like" • articles where unambiguous

Then replace:
  long noun phrase     → defined term (≥ 3 reuses, intuitive tag)
  textbook description → canonical term-of-art (verify model has prior)
  prose mapping        → table
  procedure            → constraint (postcondition)
  prose logical rule   → ∈ ∉ ⇒ ¬ ∧ ∨ symbol form
  prose enumeration    → bullet list
  prose if-then        → input → output example pair

Format:
  markdown for instructions • YAML for nested config •
  JSON for tool-call payloads • plaintext only when single-line

Anti-patterns:
  vague-word substitution • synonym churn • cargo-cult acronyms •
  compressing reasoning chains • removing citations •
  ambiguity-as-compression

Verify: round-trip via fresh LLM • diff vs original • frozen eval set
```

---

## 12. Citations and source map

The doctrine above synthesises across information theory, controlled-natural-language design, legal-drafting tradition, prompt-compression research, and tokenizer engineering. The substantive sources behind each section are below.

### Information theory & compression
- Shannon, C. (1948). *A Mathematical Theory of Communication.* Bell System Tech. J. — entropy, redundancy of English at ~50%.
- Miller, G. A. (1956). *The Magical Number Seven, Plus or Minus Two.* Psychological Review 63(2):81. — chunking, channel capacity.
- Kolmogorov, A. N. (1965). On Tables of Random Numbers. — algorithmic information / Kolmogorov complexity.
- Ehret, K. (2017). *Compression-based measures of linguistic complexity.* — gzip as Kolmogorov approximation.
- Bentz, C. et al. (2017). *Entropy Rate Estimates for Natural Language* (MDPI Entropy). — natural language entropy bounds.

### Prompt-compression research
- Jiang, H. et al. (2023). *LLMLingua: Compressing Prompts for Accelerated Inference of LLMs.* EMNLP. — coarse-to-fine, budget controller, 20x compression.
- Jiang, H. et al. (2024). *LongLLMLingua.* — question-aware, position-bias mitigation, 4x compression with +17% perf in RAG QA.
- Pan, S. et al. (2024). *LLMLingua-2: Data Distillation for Task-Agnostic Prompt Compression.* ACL Findings. — BERT-class compressor, 3x–6x faster.
- Li, Y. et al. (2023). *Selective Context: Unlocking Context Constraints via Self-Information-Based Content Filtering.* — entropy-based token pruning.
- Various (2025). *Prompt Compression for Large Language Models: A Survey.* NAACL. — survey of hard vs soft prompt compression.
- (2026). *Semantic Density Effect.* — formula `SDE = (S − R) × C / W` and the +8.4pp accuracy gain at SDE > 0.8.
- (2026). *MetaGlyph: Semantic Compression of LLM Instructions via Symbolic Metalanguages.* — 62–81% token reduction via math symbols.
- (2026). *CROP: Cost-Regularized Optimization of Prompts.* — 80.6% token reduction with maintained accuracy via length-regularised APO.
- (2026). *Big-Otok.* — token-cost asymptotic notation; complement to Big-O.
- (2026). *Context Codec / CCL.* — typed semantic atoms; complement to raw token compression.

### Controlled natural language & technical writing
- ASD-STE100 (AeroSpace and Defence Industries Association of Europe). *Simplified Technical English.* — ~900-word controlled dictionary, writing rules, one-word-one-meaning, one-instruction-per-sentence.
- Ogden, C. K. (1930). *Basic English: A General Introduction with Rules and Grammar.* — 850-word controlled English vocabulary; demonstration that 850 words can replace 20,000.
- Strunk, W. & White, E. B. (1959). *The Elements of Style.* — Rules 13 (omit needless words), 14 (active voice), 15 (positive form), 16 (definite, specific, concrete).
- Gowers, E. (1948). *Plain Words.* — "use no more words than are necessary; use the most familiar words; use precise and concrete words rather than vague and abstract words".

### Legal & legislative drafting
- UK Office of the Parliamentary Counsel. *Drafting Guidance* (2024). — defined terms, brevity, signposting.
- New Zealand Parliamentary Counsel Office. *Principles of Clear Drafting.* — short, simple, precise sentences; defined terms only when truthful and helpful.
- Uniform Law Commission (US). *Drafting Rules and Style Manual.* — accuracy, brevity, clarity, consistency, simplicity.
- IETF (1997). *RFC 2119: Key words for use in RFCs to Indicate Requirement Levels.* — `MUST`, `SHOULD`, `MAY` as compression operators.

### Linguistic & cognitive foundations
- Lemke, R., Horch, E., & Reich, I. (2017). *Optimal encoding! Information Theory constrains article omission in newspaper headlines.* EACL.
- Léon, J. (2008–2017). Studies of newspaper headlinese register and function-word omission.
- Cowan, N. (2001). *The magical number 4 in short-term memory* (later refinement of Miller 1956).
- Various. *Chunking and Redintegration in Verbal Short-Term Memory.* PMC.

### Mathematical notation as compression
- Schlimm, D. (Cambridge). *Mathematical Notations* — design principles for notation systems.
- *Compression is all you need: Modeling Mathematics* (2026) — place notation, hierarchy, parsimony bound.
- *Dirac Notation as the Archetype of Ontological Density* — bra-ket as lossless symbolic compression.

### Tokenizer mechanics
- OpenAI. *tiktoken* repository — BPE tokenizer, `cl100k_base`, `o200k_base`, ~4 bytes per token average for English.
- Various practitioner write-ups on prefix caching, token economics, tokenizer-aware prompt design (2025–2026).

### LLM behaviour: ambiguity, format, underspecification
- Zhou et al. (2022). *Chain-of-Thought reduces CPS from 0.15 to 0.06 vs vague prompts.*
- *What Makes a Good Query? Measuring the Impact of Human-Confusing Linguistic Features on LLM Performance* (2026). — ambiguity, anaphora → +hallucination risk.
- *What Prompts Don't Say: Understanding and Managing Underspecification in LLM Prompts* (2025). — instruction-following can drop 19% as requirements grow.
- *Let Me Speak Freely? Impact of Format Restrictions on LLM Performance* (2024). — JSON-mode hurts reasoning, helps classification.
- He, X. et al. (2024). *Does Prompt Formatting Have Any Impact on LLM Performance?* — up to 42% accuracy delta JSON vs markdown.

---

## 13. Closing meta-rule

> **A prompt is read by a system that already knows the language. Write for that reader.**
>
> Spend your tokens where the reader has no prior: the verbatim, the specific, the project-particular. Save tokens where the reader has strong priors: the framing, the textbook background, the synonyms-of-synonyms. The art is knowing which side of that line each sentence is on. Round-trip every compression to verify you stayed on the right side.

— end of `compressionprompt.md`
