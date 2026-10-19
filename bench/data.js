window.BENCHMARK_DATA = {
  "lastUpdate": 1792426750372,
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
          "id": "cf06eccb8511e5e7af418dcf7cb0ce3ba6bcd102",
          "message": "fix(render): read a generated struct from an object and nothing else (#64)\n\nA generated struct also read a JSON array by position, as serde's derive\ndoes, which no JSON Schema object describes. Inside an untagged union that\nlet an object branch take a list meant for a later list branch: `[\"Ada\"]`\nbecame `{name: \"Ada\"}`, and the same happened one level down in nested lists\nand shared members.\n\nThe hand-written struct visitor drops its `visit_seq`. Under the derive\nstrategy a struct's `Deserialize` derives on a private copy declared remote\nto it, read through `support::Object`, which refuses a sequence with the same\nmessage. Lenient decoding already read structs from objects alone.\n\nCloses #60.",
          "timestamp": "2026-10-19T02:26:47+02:00",
          "tree_id": "8a1cdb54fab43a76ba2f8dde7d70029188ae10a9",
          "url": "https://github.com/romnn/progeny/commit/cf06eccb8511e5e7af418dcf7cb0ce3ba6bcd102"
        },
        "date": 1792369924856,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "okta.hand-written cpu",
            "value": 27.061282333333335,
            "range": "± 0.07",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "okta.hand-written peak rss",
            "value": 2998.037759780884,
            "range": "± 0.8",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written cpu",
            "value": 9.272484666666665,
            "range": "± 0.04",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written peak rss",
            "value": 1252.1484375,
            "range": "± 0.6",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written cpu",
            "value": 0.7117786666666667,
            "range": "± 0",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written peak rss",
            "value": 334.8450517654419,
            "range": "± 1.1",
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
          "id": "efbd57e7396af9bf59457170e1e029568c28764a",
          "message": "fix(shape): read a list beside a union as the list the union admits (#67)\n\nA schema that lists `enum`/`const` values beside its own `anyOf`/`oneOf`\nwas generated as the union's type alone, and the list was dropped without\na report: `{enum: [a], anyOf: [{const: a}, {type: integer}]}` read `7`.\n\nThe merged view now narrows such a list to the values the union admits,\njudged exactly from `type`, `enum`, `const`, the boolean schemas and nested\nunions, with `oneOf`'s exactly-one rule and numbers compared by value. The\nschema reads as that list. Where the union is one branch and perhaps\n`null` and admits listed values alone, as OpenAPI 3.0 writes a nullable\nreference to an enum, the union is kept with its reference, and `null`\nis counted once. A value that cannot be checked, or a list of objects, is\nkept and reported, and an empty intersection admits nothing. An empty\n`enum` is read as no list.\n\nCloses #61.",
          "timestamp": "2026-10-19T04:19:08+02:00",
          "tree_id": "f35311f22b631fcd9dbd35bcd14c94ba390fc90d",
          "url": "https://github.com/romnn/progeny/commit/efbd57e7396af9bf59457170e1e029568c28764a"
        },
        "date": 1792376688049,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "okta.hand-written cpu",
            "value": 26.921170333333336,
            "range": "± 0.52",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "okta.hand-written peak rss",
            "value": 3000.9921875,
            "range": "± 2.5",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written cpu",
            "value": 9.265079,
            "range": "± 0.1",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written peak rss",
            "value": 1251.609375,
            "range": "± 0.6",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written cpu",
            "value": 0.6989490000000002,
            "range": "± 0.01",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written peak rss",
            "value": 335.5052080154419,
            "range": "± 1.1",
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
          "id": "8af64c22ce56e4ec8e42d8ad5f10dcd3e3f4721e",
          "message": "fix(shape): stop reporting a zero lower bound spelled as a float (#68)\n\n`minimum: 0.0` on an integer, the way schemars writes every bound, was\nreported as a `minimum` the generated `u64` does not express, although it\nexpresses exactly that. A bound now counts as zero by its text, so `0.0`,\n`-0.0` and `0e0` are zero. Under `exact-numbers` a bound like `1e-400`\nkeeps its text and is still reported; without it `serde_json` has already\nread it as `0.0`, and the loader reports it as an inexact number.\n\nCloses #65.",
          "timestamp": "2026-10-19T04:35:10+02:00",
          "tree_id": "89e18d808396a3bffe0721867f0fe54c4c094dc7",
          "url": "https://github.com/romnn/progeny/commit/8af64c22ce56e4ec8e42d8ad5f10dcd3e3f4721e"
        },
        "date": 1792377548571,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "okta.hand-written cpu",
            "value": 19.089367666666668,
            "range": "± 0.42",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "okta.hand-written peak rss",
            "value": 3000.576822280884,
            "range": "± 6.3",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written cpu",
            "value": 6.610619,
            "range": "± 0.07",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written peak rss",
            "value": 1255.294270515442,
            "range": "± 2.8",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written cpu",
            "value": 0.5021243333333333,
            "range": "± 0",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written peak rss",
            "value": 334.1263017654419,
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
          "id": "9be5911d02bf244fd50a448f82b4666d2afe6e3f",
          "message": "feat(shape): size integers by their format, opt-in (#69)\n\n`[formats] integers = \"by-format\"` gives an integer whose `format` names a\nwidth (`int8` through `int64`, `uint8` through `uint64`, `uint`) that Rust\ntype, when every value its bounds and listed values admit fits it.\nA format progeny does not know, parts that disagree, or a bound or listed\nvalue outside the width keep the wide type and are reported.\nThe default stays `i64`/`u64`.\n\nBounds now live in the merged view as exact values, so a bound equal to its\ntype's range end is no longer reported as unexpressed.\n\nCloses #66",
          "timestamp": "2026-10-19T12:14:40+02:00",
          "tree_id": "fe9deb1c3bfda322b5e955132b9cc662fcb89b1d",
          "url": "https://github.com/romnn/progeny/commit/9be5911d02bf244fd50a448f82b4666d2afe6e3f"
        },
        "date": 1792405197522,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "okta.hand-written cpu",
            "value": 26.70028566666667,
            "range": "± 0.22",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "okta.hand-written peak rss",
            "value": 3000.513020515442,
            "range": "± 3.6",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written cpu",
            "value": 9.180933666666666,
            "range": "± 0.11",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written peak rss",
            "value": 1249.9401035308838,
            "range": "± 0.7",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written cpu",
            "value": 0.6823653333333333,
            "range": "± 0",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written peak rss",
            "value": 334.6822910308838,
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
          "id": "7f17ea17a499d843b514d36326edd5a72a73af80",
          "message": "fix(bundle): read a definition another namespace repeats as one type (#72)\n\nA name two namespaces of a bundle define was reported as\n`colliding-type-name` even when both definitions are the same schema, as\ncodex's app-server bundle writes `AbsolutePathBuf` and `RequestId` in\n`/definitions/v2` and `/definitions`.\n\nA later definition equal to the first once `title`, `description`,\n`$comment` and `examples` are set aside wherever a schema sits, read by\nthe same draft, is now that schema, and nothing is reported. Its own `$id`\nis set aside too where neither definition holds a reference. Every schema\nof the repeat maps onto the one at the same place in the first\ndefinition, so a `$ref` to it by pointer, `$id` or anchor reaches one\ntype, chains of repeats settle on one schema, and a name first defined\ninside a repeat names that schema. A definition that differs in anything\nelse is still reported.\n\nCloses #70",
          "timestamp": "2026-10-19T16:25:10+02:00",
          "tree_id": "04683cf1d680eec2e0ac9ebdccfaba293c46b3cf",
          "url": "https://github.com/romnn/progeny/commit/7f17ea17a499d843b514d36326edd5a72a73af80"
        },
        "date": 1792420211943,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "okta.hand-written cpu",
            "value": 25.601564666666665,
            "range": "± 0.07",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "okta.hand-written peak rss",
            "value": 3002.423176765442,
            "range": "± 3.2",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written cpu",
            "value": 8.816478666666667,
            "range": "± 0.03",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written peak rss",
            "value": 1251.8046875,
            "range": "± 1.6",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written cpu",
            "value": 0.6657633333333333,
            "range": "± 0",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written peak rss",
            "value": 334.16796875,
            "range": "± 0.2",
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
          "id": "363c8e3fa6b4d53585629091b58b90f991167553",
          "message": "fix(shape): read a nested anyOf before another branch as its own branches (#73)\n\nA nested union that was a branch of another, but not the last, made the\nouter union `wild-union`: the type generated for the nested union holds\nevery payload its branches do not fit, so it took the payloads of every\nbranch after it.\n\nA nested `anyOf` that declares nothing beside its branches admits exactly\nwhat they do. Where the outer union would otherwise degrade for want of a\ntag, it is now matched again with every such nested `anyOf` read as its\nbranches, in order, and a `null` among them makes the outer union\noptional. The nested union keeps its own type wherever it is referenced\non its own. A nested `oneOf` is not spread, because it refuses a payload\ntwo of its branches admit, and neither is one with a discriminator, a\n`type` or any other keyword beside its branches. Ambiguity checks count a\n`null` a spread lifts as making a member optional.\n\nCorpus: wild-union occurrences fall in runway, qdrant, lithic, logfire,\nopenai and opensearch.\n\nCloses #71",
          "timestamp": "2026-10-19T18:13:16+02:00",
          "tree_id": "d421c697ffb64d236e2239712fcb58a66176fb98",
          "url": "https://github.com/romnn/progeny/commit/363c8e3fa6b4d53585629091b58b90f991167553"
        },
        "date": 1792426749530,
        "tool": "customSmallerIsBetter",
        "benches": [
          {
            "name": "okta.hand-written cpu",
            "value": 27.453357999999998,
            "range": "± 0.45",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "okta.hand-written peak rss",
            "value": 3003.46484375,
            "range": "± 2.7",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written cpu",
            "value": 9.367625666666667,
            "range": "± 0.05",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "oxide.hand-written peak rss",
            "value": 1251.3307285308838,
            "range": "± 2",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written cpu",
            "value": 0.7171810000000001,
            "range": "± 0.02",
            "unit": "s",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          },
          {
            "name": "petstore-31.hand-written peak rss",
            "value": 335.32421875,
            "range": "± 2",
            "unit": "MiB",
            "extra": "3 kept, 0 discarded, rustc 1.99.1 (58cf8f9c3 2026-10-13)"
          }
        ]
      }
    ]
  }
}