// Determinism fixture for #391 (ADR 0013): pick() builds a real scoring
// table — not a scalar — stashes the whole table in the pick() envelope's
// `workspace`, and audit() derives every stage record purely from that
// workspace, never re-reading ctx. Exercised by determinism.rs, which proves
// both passes emit byte-identical audit trails and that a second generation
// never observes the first's workspace (workspace has no storage of its own
// — see score.rs's PLUGIN_CLOCK for the shape ADR 0013 deliberately avoided).

fn hooks() { ["pool_provider"] }
fn capabilities() { ["catalog_read"] }
fn sources() { #{ movies: `item.type == "movie"` } }

fn pick(ctx) {
    let ids = [];
    for item in ctx.sets.movies { ids.push(item.entry_id); }

    // Reduced into the modulus immediately — ctx.seed is a full 63-bit value
    // and multiplying it unreduced overflows Rhai's checked i64 arithmetic.
    let seed = ctx.seed % 999999937;

    let scored = [];
    let idx = 0;
    for id in ids {
        let key = ((seed + 1) * (idx + 1) * 7919) % 999999937;
        scored.push(#{ id: id, key: key });
        idx += 1;
    }
    scored.sort(|a, b| if a.key < b.key { -1 } else if a.key > b.key { 1 } else { 0 });

    // A second pass builds the working set proper: one row per candidate,
    // carrying its rank and every id it out-ranked. This is the non-trivial
    // part of `workspace` — a scoring table, not a scalar — and `beat`'s
    // length is what makes a leaked/accumulated workspace observable: it
    // would grow if a later generation's table absorbed an earlier one's.
    let n = scored.len();
    let table = [];
    let i = 0;
    while i < n {
        let beat = [];
        let j = i + 1;
        while j < n {
            beat.push(scored[j].id);
            j += 1;
        }
        table.push(#{ id: scored[i].id, key: scored[i].key, rank: i + 1, beat: beat });
        i += 1;
    }

    let out = [];
    for row in table { out.push(row.id); }

    #{ picks: out, workspace: #{ table: table, candidates: n } }
}

// Reads ONLY workspace — never re-derives from ctx. A leaked or
// re-accumulated workspace shows up here as a `candidates` count that no
// longer matches the fixed catalog size, or a `beat` list that grew.
fn audit(ctx, picks, workspace) {
    let out = #{};
    for row in workspace.table {
        out[row.id] = #{
            stage: "pool",
            by: "plugin:workspace-audit",
            verdict: "ranked #" + row.rank + " of " + workspace.candidates,
            detail: #{
                rank: row.rank,
                score: row.key,
                candidates: workspace.candidates,
                beat: row.beat,
            }
        };
    }
    out
}
