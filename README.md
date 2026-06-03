# mirror — DEPRECATED (superseded by `rdev`)

This app is retired. It depended on CE node features (`PUT/DELETE /mesh-sync`) that were **removed
from CE** when device-to-device file sync moved out of the node into an application — the correct
side of the CE primitives-vs-apps boundary.

**Use [`rdev`](https://github.com/ce-net/rdev) instead.** Its `rdev watch` is the continuous 1:1
folder mirror this app provided (now built on CE primitives: `AppRequest` + the `ce-cap` verifier,
no node code), plus remote `exec`, `push`, and `rm`.

```bash
rdev watch ~/ce-net desktop:ce-net    # what `mirror watch` used to do
```

This repository is archived. History is preserved for reference only.
