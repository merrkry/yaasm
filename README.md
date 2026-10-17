# Yet Another Agent Skills Manager

YAASM is a Rust CLI for managing Git, GitHub, and live local agent skills on Linux, macOS, and Windows. It exposes skills through stable links in your configured agent directories.

See [the design reference](docs/design.md) for configuration, path rules, lockfiles, and failure behavior.

## Deploy skills

Put `config.toml` in `~/.config/yaasm` or select an explicit configuration and data directory with `--config-dir` and `--data-dir` together. All commands accept those global options.

```console
yaasm deploy
yaasm update
yaasm deploy --locked
```

`deploy` reconciles the lockfile and links. `update` refreshes network sources and the lockfile. Run `deploy` separately to apply the updated versions. `deploy --locked` preserves the lockfile and can still fetch missing content.

## Build and deploy a remote bundle

On a machine with Git and network access, prepare the remote versions without touching local skills or targets:

```console
yaasm update
yaasm bundle --output ./remote-bundle
```

`bundle` always uses the current locked versions. Its output must be a new directory with an existing parent. Copy the complete directory and the matching remote configuration and lockfile to the destination machine. Local skill paths and targets can use that machine's settings.

Deploy offline:

```console
yaasm deploy --bundle ./remote-bundle
```

Local skills keep their live paths, so edits to their bodies are immediately visible through the links.

After a successful deployment, you can remove the input bundle without breaking deployed skills. Keep a copy for future offline deployment invocations.

For Windows deployment, select remote sources without upstream symlinks. See [the bundle contract](docs/design.md#remote-bundles) for format, integrity checks, and platform restrictions.

## Use immutable build inputs

For a sandbox or Nix build, supply read-only `config.toml` and `lock.toml` and a separate writable workspace. For example, with `config` pointing to that input directory:

```sh
yaasm --config-dir "$config" --data-dir "$TMPDIR/yaasm-build" \
	bundle --output "$TMPDIR/remote-bundle"
```

Save or copy the resulting directory to your build output. Allow Git and network access during construction. Local skill paths need not exist.

On the deployment machine, apply its configuration and writable local skill bodies, then run `deploy --bundle` with its own data directory. Keep configuration, lockfile, and bundle inputs stable during each invocation, as required by [the locking contract](docs/design.md#concurrency-and-interruption).

## Development

Use Rust 1.89 or newer and Git with `clone --revision` support. Run the checks from the repository root:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
```
