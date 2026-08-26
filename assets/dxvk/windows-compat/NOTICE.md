# DXVK runtime

Capsule ships the unmodified 32-bit and 64-bit Windows DLLs from the official
DXVK 2.7.1 binary release:

<https://github.com/doitsujin/dxvk/releases/tag/v2.7.1>

Downloaded release archive:

- `dxvk-2.7.1.tar.gz`
- SHA-256: `d85ce7c79f57ecd765aaa1b9e7007cb875e6fde9f6d331df799bce73d513ce87`

DXVK is Copyright © 2017 Philip Rebohle and other contributors and is
distributed under the zlib license. The complete upstream license text is in
`LICENSE` beside this notice.

Capsule mounts this directory read-only inside Wine containers. Prefixes hold
symlinks to these assets rather than private copies.
