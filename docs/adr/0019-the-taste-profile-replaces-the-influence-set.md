# The taste profile replaces the influence set; the station resolves, the script weighs

A pool steers a scorer with a taste profile on the pool itself, not with a CEL set and a weight in `config:`. The station parses and resolves each entry (keyword spelling, item id or title, set members) and fails the load or generation naming a bad one. It hands the resolved entries to the script as `ctx.profile` and computes no score. `taste-cosine.rhai` nets the entries into favor and disfavor maps and owns every weight, share and cosine, per ADR 0002.
