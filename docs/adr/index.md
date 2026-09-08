# Architecture Decision Records

An ADR states one decision as true now — `docs/adr/NNNN-slug.md`, numbered sequentially. See `~/.claude/skills/docs/ADR-FORMAT.md` for the full convention: what it does and doesn't contain, and how a rewrite is shown before it's written.

## Handling drift

Code changes; an ADR is rewritten in place to match, reading as though it always said that. Git history — `git log -p docs/adr/` — carries what it used to say and why it changed, not a note in the file itself.
