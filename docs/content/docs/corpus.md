---
title: Conformance corpus
weight: 5
---

# Conformance corpus

progeny is measured against 78 real-world published API descriptions rather than against hand-picked examples. They cover both OpenAPI dialects, both serialization formats, and the constructs that break generators: thousands of `anyOf`s, discriminator-heavy schemas, self-contradicting examples, and descriptions several megabytes large.

## The documents

`corpus/manifest.toml` lists each document with its upstream URL and why it is in the corpus. The vendor documents are not committed — they total about 117 MB and their redistribution rights vary by publisher — so `task corpus:fetch` downloads them into `corpus/cache/`. The exceptions are the committed specs under `corpus/specs/`, such as the hand-written `petstore-31.yaml`, which are always available to offline tests.

Pull requests and `main` run only pinned documents: the committed ones, and the GitHub-hosted ones the manifest fetches at a recorded commit (`rev`). Each must match the digest in its snapshot, so a publisher cannot change their result. The other documents are live vendor endpoints and run only in the nightly full corpus. Moving a pin means editing its `rev` and re-recording its snapshot with `task corpus:snapshots`, a change reviewed like any other.

## What every run checks

`task corpus` runs over every document, and `task corpus -- --pinned` over the pinned ones:

- **Round trip.** Each document is read into the model and written back, and the two are compared by value.
- **References.** Every reference is resolved and accounted for.
- **Determinism.** Each document is parsed twice and generated twice, and the results are compared.
- **Diagnostics snapshots.** Every finding is recorded in a per-document snapshot, so a change in what progeny reports shows up in review.

Beside the corpus, `corpus/convergence/` describes one API by hand in both the 3.0 and the 3.1 dialect. The two must read into the same model, generate the same source, and give up the same things.

### Diagnostics snapshots

`corpus/snapshots/<document>.jsonl` holds one JSON line per [diagnostic]({{< relref "diagnostics.md" >}}), with its keys in a fixed order, so a snapshot diff reads as a behaviour change rather than as churn. `task corpus:snapshots` re-records them after a deliberate change.

## The quick tier

Compiling a generated crate is not cheap, so the compile and payload checks run over eight documents on every pull request, listed in `corpus/tier.toml`:

| Document | Why |
| --- | --- |
| `petstore-31` | hermetic and committed — the one document that needs no network |
| `github-31` | 3.1, about 4 MB, the size ceiling |
| `airflow` | 3.1, 639 `anyOf`, `prefixItems`, 626 null-type lines |
| `cloudflare` | 3.0, very large, 29 self-contradicting example payloads |
| `okta` | 3.0 YAML, 56 discriminators |
| `lithic` | 3.1, 99 `oneOf`, 86 `const`, `patternProperties` |
| `influxdb` | 3.0 YAML, binary bodies |
| `oxide` | 3.0, the predecessor's flagship consumer document |

Each pull request:

- **compiles and lints** the tier's generated crates with Clippy warnings denied (`task corpus:compile`), through the hand-written serde path, the derive, and strict decoding — a warning in generated code is a defect its consumer meets in their own build;
- **round-trips payloads** (`task payloads`): every example payload a tier document carries is deserialized into the type generated for it, through both serde strategies;
- **runs the differential harness** (`task differential`), asserting the two serde renderings agree on the wire;
- **sends requests**: a generated client against the generated server of the same description, over a real socket — `petstore-31` in `task example`, and every servable operation of `petstore-31` and `oxide` in `task probe`, with `petstore-31` probed through the derive as well.

Nightly, the full corpus is compiled and linted (`task corpus:compile:all`) and the wire probe runs over the whole tier, because a shape that occurs in one document is a shape the tier is blind to.

## What it found

`corpus/findings.md` records what the measurements settled, with the numbers behind each decision: which constructs real documents use and how often, the defects each new gate found on its first run, and what each rendering costs to compile.

## Benchmarks

The compile cost of a generated crate is a measured property. On every commit to `main`, the bench workflow generates three documents from the quick tier — `petstore-31`, `oxide` and `okta`, the vendor ones pinned to fixed upstream commits — and measures compiling each at one compiler job. The [benchmark dashboard]({{< relref "/_index.md" >}}bench/) charts that history.

The hosted runners are a pool of different processors, whose times differ by up to a factor of two on one commit, so the headline figures come from valgrind, which runs the compiler on a synthetic processor of its own and gives the same figures on every runner:

- the instructions one compile executes, counted by cachegrind;
- its peak heap, and its average heap weighted by the instructions between massif's snapshots.

Only the compiler that builds the generated crate runs under valgrind; its dependencies are built natively first. The native CPU time and peak RSS are still recorded, but they are not stable enough to judge by alone: each CPU time records the runner's processor, and the dashboard keeps one series per processor model, so a change of runner never reads as a change of performance.

The bench workflow pins both tools that move these figures: the Rust release, in its `dtolnay/rust-toolchain` step, and valgrind, as an exact Ubuntu package version. Moving either is a commit, and the new version starts a new segment of the history. Locally, `task bench:compile -- --valgrind valgrind --json out.json` records the same figures.

Locally, `task bench:compile` A/B-tests the compile cost of two renderings and `task bench:runtime` the serde runtime cost. Both need an idle machine, and both refuse to record a figure taken outside their measurement discipline rather than quietly writing one down. The recorded figures live in `corpus/baseline.toml` and `corpus/runtime.toml`.
