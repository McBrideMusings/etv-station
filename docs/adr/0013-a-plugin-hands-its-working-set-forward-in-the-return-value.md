# A plugin hands its working set from pick() to audit() in the return value

`pick()` returns `#{ picks: [...], workspace: <opaque> }` instead of a bare array. The station holds `workspace` for the length of one generation and hands it back as the third argument to `audit(ctx, picks, workspace)`; it is never read or written by the station itself, the same opaque treatment `metadata` already gets. A script that puts nothing there gets nothing back.
