window.BENCHMARK_DATA = {
  "lastUpdate": 1792366595678,
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
          "id": "afed8d215e7327b9f78c622f0d0f663ab62b40bf",
          "message": "feat(shape): read string enums in a union that lists null as one optional enum (#59)\n\n* feat(xtask): check corpus documents in parallel with --jobs\n\n`corpus`, `regen-snapshots` and `payloads` take `--jobs N`, defaulting to the smallest of four,\nthe cores, and what the free memory covers at about 4 GiB per document generated, 10 GiB per\ncrate compiled and 15 GiB per crate compiled through the derive. Documents run on a rayon pool\nand report in manifest order, so output and snapshots are the same whatever N is. Each worker\ncompiles in a cargo target directory of its own, because cargo locks one for a whole build, and\na document selected twice is checked once. CI keeps its compile and payload steps at `--jobs 1`.\n\n* feat(shape): read string enums in a union that lists null as one optional enum\n\n`oneOf: [{enum: [\"a\", null]}, {enum: [\"b\"]}]`, the 3.0 `nullable` spelling of each branch, is\n`Option` of one enum of every listed string, where it used to degrade to arbitrary JSON. A\nlisted `null` counts only where the branch's own `type` admits it.",
          "timestamp": "2026-10-18T23:15:38+02:00",
          "tree_id": "4eda0ceb77473c1be5295d714fbc59e9709d0169",
          "url": "https://github.com/romnn/progeny/commit/afed8d215e7327b9f78c622f0d0f663ab62b40bf"
        },
        "date": 1792358676033,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "okta.hand-written cpu",
            "value": 27.345729000000002,
            "range": "± 0.54",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "okta.hand-written peak rss",
            "value": 3011.171875,
            "range": "± 4.2",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written cpu",
            "value": 9.232153666666667,
            "range": "± 0.12",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written peak rss",
            "value": 1250.6119785308838,
            "range": "± 1.2",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written cpu",
            "value": 0.7443779999999999,
            "range": "± 0.01",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written peak rss",
            "value": 332.78125,
            "range": "± 1",
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
          "id": "446662525ad7ccf2461444247df783ec2295bde4",
          "message": "fix(shape): reach a union's catch-all past branches that read values their schema refuses (#62)\n\nA last branch that admits every value of its kind, `{}`, `{type: object}` or\n`{type: array}`, is reached only by what the branches before it refuse. An\nearlier branch whose type accepts what its own schema refuses, such as an\nunlisted enum value or a member union's fallback, took the catch-all's\npayloads, so the union now degrades there instead. An open earlier struct of\nplain members still stays typed, as does an unconstrained object or list\ncatch-all, which degraded before.\n\nA later branch is taken to carry only the members it declares, now written\ndown on `rejects`. Judging an open later branch by every payload it validates\ninstead degrades about 200 more unions across the corpus.\n\nPart of #22.",
          "timestamp": "2026-10-19T00:59:06+02:00",
          "tree_id": "cd1e5c6d135234a4cd85c8f0ac0dff412b28a320",
          "url": "https://github.com/romnn/progeny/commit/446662525ad7ccf2461444247df783ec2295bde4"
        },
        "date": 1792364690775,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "okta.hand-written cpu",
            "value": 29.847318666666666,
            "range": "± 0.26",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "okta.hand-written peak rss",
            "value": 3010.950520515442,
            "range": "± 3.9",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written cpu",
            "value": 10.062881666666668,
            "range": "± 0.13",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written peak rss",
            "value": 1251.727864265442,
            "range": "± 2.7",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written cpu",
            "value": 0.745753,
            "range": "± 0.01",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written peak rss",
            "value": 332.65625,
            "range": "± 1",
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
          "id": "11cc5f648955a2a20a51ca7b125a0a0e29583b39",
          "message": "fix(shape): let a later number branch keep the fractions an earlier integer cannot hold (#63)\n\n`oneOf: [{type: integer, enum: [1]}, {type: number, enum: [2.5]}]` degraded,\nbecause the integer lists its values and the number branch lists one it does\nnot. The generated `i64` refuses `2.5`, so the later branch is reached, and\nthe union stays typed. A value counts as refused only by what it is: a\nfractional number against an integer, or a negative one against an unsigned\ninteger. `2.0` equals `2`, which the integer reads, so it still degrades.\n\nCompletes #22.",
          "timestamp": "2026-10-19T01:31:19+02:00",
          "tree_id": "5c3e804d33fdc64b838a1f10a370136abb39aada",
          "url": "https://github.com/romnn/progeny/commit/11cc5f648955a2a20a51ca7b125a0a0e29583b39"
        },
        "date": 1792366594875,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "okta.hand-written cpu",
            "value": 25.883588000000003,
            "range": "± 0.38",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "okta.hand-written peak rss",
            "value": 3011.591145515442,
            "range": "± 1.9",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written cpu",
            "value": 8.674301333333332,
            "range": "± 0.08",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written peak rss",
            "value": 1252.2877597808838,
            "range": "± 1.7",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written cpu",
            "value": 0.6636719999999999,
            "range": "± 0.01",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written peak rss",
            "value": 333.5377597808838,
            "range": "± 1",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          }
        ]
      }
    ]
  }
}