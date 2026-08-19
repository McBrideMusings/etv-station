# A cascade level can append layers, not just replace the whole config

The station→channel→block overlay cascade gains a fourth form: `overlay: {extend: {layers: [...]}}`, which keeps the config the level above resolved and appends layers to it. Replacement stays the default and `clear` still wipes the chain. An extend may also name a different `script:` or replace `config:` wholesale; it may not carry geometry, and it may not reach into a layer it inherited.

## Why

ADR 0007 and #48 settled that the cascade resolves *configuration*, not composition: one channel runs one overlay process, and a deeper level picks which whole config that process runs. The module docs stated the reasoning plainly — a guide config is a bag of independent strings, an overlay config is one composed picture, and half of one picture over half of another is not a picture.

That reasoning is still right about *merging*. It turned out to be wrong about *stacking*, and the deployed station is what proved it.

The station has exactly one shape it needs and could not express: graphics every channel shares, plus one thing only the channel knows. The Now/Next snipe — a scrim, a label, a title, an episode row, and the Rhai script driving them — is identical on all 60 channels. Each channel's bug is not: it is that channel's own `logo.png` at a height tuned to the artwork, ranging from 16px to 145px across the fleet. Under replacement, a channel that wanted its own bug had to restate the shared half too.

So the shared graphic was pasted into 60 channel files — 25 lines each, ~1500 lines of duplication — by a throwaway script, because no other mechanism existed. Every subsequent edit to the snipe means editing 60 files or re-running that script. The duplication was not carelessness; it was the cascade's shape showing through. A bug is evidence the architecture permits it, and this one was permitted by design.

Two cheaper fixes were tried against the real config first and both failed on the same fact:

- **One shared spec file referenced by all 60 channels** (`overlay: {file: …}`, which already exists) cannot work, because the varying part is inside the shared file. `path: logo.png` would resolve against the shared file's directory, not the channel's.
- **A `${CHANNEL_DIR}` token in `path:`** fixes the path but not the height, which genuinely differs per channel. Templating both fields is two special-cased tokens standing in for a general mechanism.

## What we chose, and the rejected alternatives

- **Append-only stacking, chosen.** A level says `extend:` and its layers are appended after the inherited ones, each re-rooted against its own config file's directory before the append — which is why a cascade level travels paired with its directory. The station declares the snipe once; each channel declares only its logo. Deleting the shared graphic becomes one edit instead of 60. Appending is the *only* composition allowed: no field merge, no layer-`id` matching, nothing that reaches inside an inherited layer, so "half of one picture over half of another" remains impossible.

- **Merge by layer index, rejected.** Would let a channel override an inherited layer's fields rather than only adding to it. Rejected because it makes a channel's file depend on the *order* of layers in a file it does not own: inserting a layer at the station level silently re-targets every channel's override, and nothing in either file records the coupling. Append has no such hidden index.

- **Layer `id`s with merge-by-name, rejected.** Fixes index fragility but reintroduces exactly what #48 ruled out — half a picture composed from two scopes — and adds a naming surface every author must now coordinate on. The station's real requirement was only ever "add my logo to whatever is shared", which append satisfies without it.

- **Leaving replacement alone and living with the duplication, rejected.** This is the status quo, and it works: the fleet renders correctly today. Rejected because the cost is unbounded and recurring — it scales with channel count and with every future shared graphic — and because the duplication actively hides drift. Sixty copies that are supposed to be identical will not stay identical, and nothing checks that they are.

## Consequences

- Geometry can only come from the level declaring a complete spec, so ADR 0007's rule — that `width`/`height`/`framerate`/`pixel_format` are baked into a running process's fifo and cannot change at a block boundary — is now enforced by the *shape* of an extend rather than only by the check that follows it. The check remains for the replace path.
- `clear` empties the whole chain rather than one level of it, so an extend beneath a cleared ancestor has nothing to extend and fails at config load naming that. The alternative — silently resurrecting the station's config under a channel that explicitly cleared it — would make `clear` mean two different things depending on what came after.
- A script's layer indices are now a contract between the station spec and any channel extending it. The station's layers come first, so a shared script addresses fixed low indices and anything a channel appends sits above the range the script touches, untouched by it.
