# Calendar conditioning filters blocks

A block can declare the calendar dates it airs on, evaluated at channel resolution (`resolve_channel`) before blocks concatenate into one flat item list — a non-matching dated block is dropped there. Cascade priority among dated blocks is the order they are written in `rule.blocks` — there is no `priority:` field. A channel with dated blocks must also declare at least one undated block, which airs unconditionally as the default; a channel lacking one fails at load. Time of day is decided inside a block, by its schedule (ADR 0021).
