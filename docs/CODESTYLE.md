# Code style

## Formatting and lints

- `cargo fmt` with [`rustfmt.toml`](../rustfmt.toml): edition 2024, Unix newlines, field-init and
  `?` shorthand.
- `cargo clippy --all-targets --all-features -- -D warnings` with [`clippy.toml`](../clippy.toml);
  a warning fails CI.
- Markdown follows [`.markdownlint-cli2.jsonc`](../.markdownlint-cli2.jsonc).

## Naming

- Functions say what they do to what; types say what they are for, not how.
- Tests are named for the behaviour they pin, as a sentence.
- Constants carry their unit or role in the name when the type does not.

## Error handling

- `anyhow::Result` in the binary; every `?` that crosses an I/O or API boundary gets a
  `.context()` naming what failed and on what.
- An error the process cannot recover from in place is returned up to `main`, which exits non-zero
  so the kubelet restarts the pod. Errors are not logged and then dropped.
- Transient API errors are left to the watcher's backoff, counted, and logged at `warn`.
- `unwrap`/`expect` only in tests.
- A type read from input has no `serde(flatten)`, `untagged` or internal `tag`: serde reads them
  through a buffer, and the refusal loses its key and line.

## Logging

`tracing` with structured fields, the message a fixed phrase. `info` for state changes, `warn` for
retried errors, `error` for input the process ignores rather than acts on, `debug` for waiting
states.

## Layout

- One module per concern.
- Pure logic stays free of I/O so it can be tested directly; I/O takes its client or socket as a
  parameter so tests can point it at a local stand-in.
- Tests sit in a `#[cfg(test)] mod tests` at the end of the file they test; tests that need a
  network namespace sit in `tests/`.
- What another slipmesh component needs too goes to [sliplib](https://github.com/slipmesh/sliplib),
  not here.

## Tests

- A test pins behaviour of this repository's code: not another crate's, not an error message's
  wording, nothing that holds by construction.
- `cargo mutants` leaves no viable mutant missed; what it may skip is listed with the reason in
  `.cargo/mutants.toml`.

## Comments

Code says what it does; a comment says only why it has to be that way - an external constraint
or a non-obvious consequence, including how an external component behaves. No history of how the
code got here, and no pointers to files or functions in other repositories.
