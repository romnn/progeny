---
title: CLI
weight: 1
---

# CLI

`progeny-cli` ships the same front end under two names: `progeny`, and `cargo progeny` for those who would rather not remember a second one.

```text
progeny <description> [--config <progeny.toml>] [--out-dir <directory>]
        [--decoding <lenient|strict>]
```

| Argument | Meaning |
| --- | --- |
| `<description>` | The OpenAPI document, JSON or YAML. |
| `--config <path>` | A [`progeny.toml`]({{< relref "configuration.md" >}}). Without one, every key takes its default. |
| `--out-dir <path>` | Where to write the generated files. Required whenever there is anything to write. |
| `--decoding <mode>` | How the client reads responses, overriding the configuration: `lenient` (the default) tolerates and reports drift, `strict` refuses it. `strict` also leaves out the `types::read` module, so the generated response types change with it. |
| `-h`, `--help` | Print the usage text. |
| `-V`, `--version` | Print the version of the library that would generate the output. |

## Output

Every [diagnostic]({{< relref "../diagnostics.md" >}}) is printed to standard output, one per line, followed by a summary:

```text
progeny: 3 repaired, 12 degraded, 1 worth a look
progeny: wrote 7 files to generated/
```

## Exit status

The CLI exits non-zero when:

- the document is rejected, or a file cannot be read or written;
- a diagnostic is denied by the configuration's [`[deny]`]({{< relref "configuration.md#deny" >}}) table, so a build can refuse to proceed on, say, any degradation;
- an entry under `[[deny.accept]]` no longer occurs, so it cannot let the same finding back in unreviewed.

The denied diagnostics and the unmatched entries are printed to standard error. The files are written before the exit decision, so the output of a refused run can still be inspected.
