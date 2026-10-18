window.BENCHMARK_DATA = {
  "lastUpdate": 1792358318938,
  "repoUrl": "https://github.com/romnn/progeny",
  "entries": {
    "Generated-crate compile cost": [
      {
        "commit": {
          "author": {
            "email": "roman.dahm@gmx.de",
            "name": "Roman Dahm",
            "username": "romnn"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "666adf55dccac482f8d861c7d6e9eedcde13d2a7",
          "message": "fix(ci): rank bundle in the layer table, and keep a strict client free of lenient-only code (#57)\n\n* refactor(doc): own a bundle's root types, and rank bundle in the layer table\n\n`bundle` and `doc` imported each other, so `bundle` could not be ranked and `xtask lint-layers`\nfailed on every push since bundles landed. `Direction` and `Selected` are part of the parsed\ndocument, so they live in `doc` now; `bundle` builds them and sits between `doc` and `resolve`.\n`progeny::Direction` is unchanged.\n\n* fix(support): leave the lenient-only report method out of a strict client\n\n* chore(fmt): format the workspace with rustfmt",
          "timestamp": "2026-10-18T22:12:12+02:00",
          "tree_id": "3ccb4e6dc075177589e747849ac4e2838c7f8048",
          "url": "https://github.com/romnn/progeny/commit/666adf55dccac482f8d861c7d6e9eedcde13d2a7"
        },
        "date": 1792354635507,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "okta.hand-written cpu",
            "value": 26.266756333333333,
            "range": "± 0.10",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "okta.hand-written peak rss",
            "value": 3011.97265625,
            "range": "± 2.2",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written cpu",
            "value": 9.00385,
            "range": "± 0.03",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written peak rss",
            "value": 1251.4296875,
            "range": "± 2.0",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written cpu",
            "value": 0.6800196666666666,
            "range": "± 0.00",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written peak rss",
            "value": 334.1184892654419,
            "range": "± 0.1",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "roman.dahm@gmx.de",
            "name": "Roman Dahm",
            "username": "romnn"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "e8a101647e17428289900a7e18f3bfc8514f4ea8",
          "message": "feat(xtask): check corpus documents in parallel with --jobs (#58)\n\n`corpus`, `regen-snapshots` and `payloads` take `--jobs N`, defaulting to the smallest of four,\nthe cores, and what the free memory covers at about 4 GiB per document generated, 10 GiB per\ncrate compiled and 15 GiB per crate compiled through the derive. Documents run on a rayon pool\nand report in manifest order, so output and snapshots are the same whatever N is. Each worker\ncompiles in a cargo target directory of its own, because cargo locks one for a whole build, and\na document selected twice is checked once. CI keeps its compile and payload steps at `--jobs 1`.",
          "timestamp": "2026-10-18T23:14:51+02:00",
          "tree_id": "92a32e579f211f4404b123a4449e65ade6bce123",
          "url": "https://github.com/romnn/progeny/commit/e8a101647e17428289900a7e18f3bfc8514f4ea8"
        },
        "date": 1792358318189,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "okta.hand-written cpu",
            "value": 16.833516333333336,
            "range": "± 0.06",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "okta.hand-written peak rss",
            "value": 3010.485676765442,
            "range": "± 1.7",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written cpu",
            "value": 5.7376553333333336,
            "range": "± 0.02",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written peak rss",
            "value": 1256.778645515442,
            "range": "± 0.9",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written cpu",
            "value": 0.44289933333333337,
            "range": "± 0",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written peak rss",
            "value": 332.8776035308838,
            "range": "± 1.2",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          }
        ]
      }
    ]
  }
}