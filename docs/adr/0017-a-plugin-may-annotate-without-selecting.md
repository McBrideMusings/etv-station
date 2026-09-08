# A plugin may implement `annotate` instead of `pool_provider`, to attach metadata without selecting anything

A channel names an `annotate:` script alongside its `rule:`, independent of whether any pool on that channel uses a plugin. The script declares the `annotate` hook and is called once per item on the already-built schedule, returning only a `metadata` patch merged into what the item already carries — it never sees or affects ordering, selection, or timing. One `annotate:` script per channel; it is not settable per-block.
