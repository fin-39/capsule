# Contributing

Contributions are welcome under the project's MIT license.

## Development setup

Install the contributor dependencies listed in `README.md`, then run:

```console
make check
make test
```

Before opening a change, also run `make release` when it affects packaging or
runtime launch behavior. Graphical and FUSE smoke tests are ignored unless the
required live session or fixture environment variables are present.

## Scope and safety

Capsule's launch argument construction and filesystem validation are security
sensitive. Keep executable paths explicit, quote every value crossing a shell
boundary, preserve fail-closed behavior, and add tests for authority changes.
Do not commit capsules, game files, credentials, logs, crash dumps, downloaded
installers or locally generated runtime state.

Use generic synthetic fixtures in tests and issue reports. Never include a
user name, home path, account identifier, authentication token or proprietary
application payload.
