//! The Edit page's read and check routes, and the taste-profile panel's text
//! splice.
//!
//! * `GET /api/channels/{ch}/file` → the deployed `channel.yaml` text, comments
//!   intact.
//! * `POST /api/channels/{ch}/validate {yaml}` → whether the loader the daemon
//!   uses would keep that text in the lineup, and the line it complains about.
//!   Writes nothing.
//! * `POST /api/channels/{ch}/profile {yaml, op, pool?, entry?}` → the pools
//!   of that text that a plugin scores, with their `profile:` entries
//!   (`op: "read"`), or the text with one entry added, updated or removed in
//!   one pool's `profile:` block (`op: "add" | "update" | "remove"`). The
//!   channel files carry hand-written comments, so only the one pool's
//!   `profile:` block is rewritten and every other line is kept as it was.

use super::apply::channel_path;
use super::pools::apply_edit;
use super::*;

type Answer = Result<(u16, Value), String>;

/// `GET /api/channels/{ch}/file`
pub(super) fn api_file(state: &mut AppState, key: &str) -> Answer {
    let path = match channel_path(state, key)? {
        Ok(p) => p,
        Err(answer) => return answer,
    };
    let yaml = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((
        200,
        json!({ "channel": key, "path": path.display().to_string(), "yaml": yaml }),
    ))
}

/// The first `line N` a loader message names.
pub(super) fn line_of(reason: &str) -> Option<u64> {
    let at = reason.find("line ")? + "line ".len();
    let digits: String = reason[at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// `POST /api/channels/{ch}/validate`
pub(super) fn api_validate(state: &mut AppState, key: &str, body: &str) -> Answer {
    #[derive(serde::Deserialize)]
    struct Request {
        yaml: String,
    }
    let req: Request = serde_json::from_str(body).map_err(|e| format!("bad request body: {e}"))?;
    let path = match channel_path(state, key)? {
        Ok(p) => p,
        Err(answer) => return answer,
    };
    Ok(
        match config::validate_channel_candidate(&state.options.station, &path, &req.yaml) {
            Ok(()) => (200, json!({ "valid": true })),
            Err(reason) => {
                let line = line_of(&reason);
                (
                    200,
                    json!({ "valid": false, "error": reason, "line": line }),
                )
            }
        },
    )
}

fn parse(yaml: &str) -> Result<config::ChannelConfig, String> {
    serde_norway::from_str(yaml).map_err(|e| e.to_string())
}

/// The plugin-scored pools of `channel`, each with its inline `profile:`.
fn profile_pools(channel: &config::ChannelConfig) -> Vec<Value> {
    let mut seen = HashSet::new();
    channel
        .rule
        .all_blocks()
        .flat_map(|b| b.pools.iter())
        .filter(|p| p.plugin.is_some() && seen.insert(p.name.as_str()))
        .map(|p| json!({ "name": p.name, "entries": p.profile }))
        .collect()
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn is_blank_or_comment(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

/// The scalar after `name:` on `line`, when that is a `name:` line: the
/// column the key starts at, and the value without quotes or a trailing
/// comment.
fn name_line(line: &str) -> Option<(usize, String)> {
    let dash = line.trim_start().strip_prefix("- ");
    let body = dash.unwrap_or(line.trim_start());
    let col = line.len() - body.trim_start().len();
    let rest = body.trim_start().strip_prefix("name:")?;
    let value = rest.split(" #").next().unwrap_or(rest).trim();
    let value = value.trim_matches(|c| c == '"' || c == '\'');
    Some((col, value.to_string()))
}

/// Replace, insert or delete the `profile:` block of the pool named `pool`.
/// `block` is the new block at column 0 (empty to delete it). Refuses, rather
/// than guess, when the pool's name is ambiguous, is not a block-style
/// `name:` line, or its `profile:` block holds a comment that a rewrite would
/// drop. Every other line, and the file's line endings, are kept.
fn splice_profile(text: &str, pool: &str, block: &str) -> Result<String, String> {
    let crlf = text.contains("\r\n");
    let lines: Vec<&str> = text.split('\n').collect();
    let named: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| name_line(l).is_some_and(|(_, v)| v == pool))
        .map(|(i, _)| i)
        .collect();
    let at = match named.as_slice() {
        [one] => *one,
        [] => {
            return Err(format!(
                "pool {pool:?} has no block-style `name:` line in this file (a block file's pool, or a `{{ name: ... }}` pool, is edited as YAML)"
            ));
        }
        _ => {
            return Err(format!(
                "`name: {pool}` appears more than once in this file, so the pool's block is ambiguous; edit it as YAML"
            ));
        }
    };
    let (col, _) = name_line(lines[at]).expect("matched just above");
    // The item starts at its dash line: this line, or the nearest line above
    // that opens a list item at a lesser indent.
    let start = if lines[at].trim_start().starts_with("- ") {
        at
    } else {
        (0..at)
            .rev()
            .find(|&i| {
                !is_blank_or_comment(lines[i])
                    && indent_of(lines[i]) < col
                    && lines[i].trim_start().starts_with("- ")
            })
            .unwrap_or(at)
    };
    let dash_col = lines[start]
        .trim_start()
        .starts_with("- ")
        .then(|| indent_of(lines[start]));
    let mut end = lines.len();
    for (i, l) in lines.iter().enumerate().skip(at + 1) {
        if is_blank_or_comment(l) {
            continue;
        }
        let ind = indent_of(l);
        if ind < col || (dash_col == Some(ind) && l.trim_start().starts_with("- ")) {
            end = i;
            break;
        }
    }
    let existing = (start..end)
        .find(|&i| indent_of(lines[i]) == col && lines[i].trim_start().starts_with("profile:"));
    let eol = if crlf { "\r" } else { "" };
    let pad = " ".repeat(col);
    let new_block: Vec<String> = block
        .lines()
        .map(|l| {
            if l.is_empty() {
                eol.to_string()
            } else {
                format!("{pad}{l}{eol}")
            }
        })
        .collect();
    let keep = |r: std::ops::Range<usize>| lines[r].iter().map(|s| s.to_string());
    let mut out: Vec<String> = Vec::new();
    match existing {
        Some(p) => {
            let mut last_body = p;
            for (i, l) in lines.iter().enumerate().take(end).skip(p + 1) {
                if is_blank_or_comment(l) {
                    continue;
                }
                if indent_of(l) <= col {
                    break;
                }
                last_body = i;
            }
            if (p..=last_body)
                .any(|i| lines[i].trim_start().starts_with('#') || lines[i].contains(" #"))
            {
                return Err(format!(
                    "pool {pool:?}'s `profile:` block holds a comment, which a rewrite would drop; edit it as YAML"
                ));
            }
            out.extend(keep(0..p));
            out.extend(new_block);
            out.extend(keep(last_body + 1..lines.len()));
        }
        None => {
            let mut last = at;
            for (i, l) in lines.iter().enumerate().take(end).skip(at) {
                if !is_blank_or_comment(l) {
                    last = i;
                }
            }
            out.extend(keep(0..last + 1));
            out.extend(new_block);
            out.extend(keep(last + 1..lines.len()));
        }
    }
    Ok(out.join("\n"))
}

/// `POST /api/channels/{ch}/profile`
pub(super) fn api_profile(state: &mut AppState, key: &str, body: &str) -> Answer {
    #[derive(serde::Deserialize)]
    struct Request {
        yaml: String,
        op: String,
        #[serde(default)]
        pool: String,
        #[serde(default)]
        entry: Value,
    }
    let req: Request = serde_json::from_str(body).map_err(|e| format!("bad request body: {e}"))?;
    if let Err(answer) = channel_path(state, key)? {
        return answer;
    }
    let channel = match parse(&req.yaml) {
        Ok(c) => c,
        Err(e) => {
            return Ok((200, json!({ "pools": [], "error": e, "line": line_of(&e) })));
        }
    };
    if req.op == "read" {
        return Ok((200, json!({ "pools": profile_pools(&channel) })));
    }
    let pool = channel
        .rule
        .all_blocks()
        .flat_map(|b| b.pools.iter())
        .find(|p| p.name == req.pool)
        .ok_or_else(|| format!("no pool named {:?} in this text", req.pool))?;
    if pool.plugin.is_none() {
        return Err(format!(
            "pool {:?} is not scored by a plugin, so it has no profile to edit",
            req.pool
        ));
    }
    let mut profile = pool.profile.clone();
    apply_edit(&mut profile, &req.op, &req.entry)?;
    let block = pools::profile_yaml(&profile);
    let yaml = splice_profile(&req.yaml, &req.pool, &block)?;
    // The splice is text surgery, so check it did what the entries say.
    let after = parse(&yaml).map_err(|e| format!("the edited text no longer parses: {e}"))?;
    let landed = after
        .rule
        .all_blocks()
        .flat_map(|b| b.pools.iter())
        .find(|p| p.name == req.pool)
        .map(|p| p.profile.clone());
    if landed.as_ref() != Some(&profile) {
        return Err(format!(
            "could not place the profile edit in pool {:?}'s text; edit it as YAML",
            req.pool
        ));
    }
    tracing::info!(
        event = "ui.edit.profile",
        channel = key,
        pool = %req.pool,
        op = %req.op,
        entries = profile.len(),
        "profile edit spliced into the edit text"
    );
    Ok((200, json!({ "yaml": yaml, "pools": profile_pools(&after) })))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "# leading comment\nnumber: 1\nrule:\n  blocks:\n    - mode: all\n      pools:\n        - name: movies\n          plugin: p.rhai\n          # keep this\n          profile:\n            - { keyword: heist, weight: 2.0 }\n          ignore_keywords: [x]\n        - name: shows\n          plugin: p.rhai\n";

    #[test]
    fn line_of_reads_the_first_line_number() {
        assert_eq!(line_of("bad at line 12 column 3"), Some(12));
        assert_eq!(line_of("no position"), None);
    }

    #[test]
    fn replacing_a_block_keeps_every_other_line() {
        let out = splice_profile(
            TEXT,
            "movies",
            "profile:\n  - keyword: heist\n    weight: 3.0\n",
        )
        .unwrap();
        assert!(out.starts_with("# leading comment\n"));
        assert!(out.contains("          # keep this\n"));
        assert!(out.contains(
            "          profile:\n            - keyword: heist\n              weight: 3.0\n"
        ));
        assert!(out.contains("          ignore_keywords: [x]\n"));
        assert!(out.contains("        - name: shows\n"));
    }

    #[test]
    fn deleting_and_inserting_a_block() {
        let gone = splice_profile(TEXT, "movies", "").unwrap();
        assert!(!gone.contains("profile:"));
        assert!(gone.contains("ignore_keywords: [x]"));
        let added =
            splice_profile(TEXT, "shows", "profile:\n  - keyword: a\n    weight: 1.0\n").unwrap();
        assert!(added.ends_with(
            "        - name: shows\n          plugin: p.rhai\n          profile:\n            - keyword: a\n              weight: 1.0\n"
        ));
    }

    #[test]
    fn crlf_survives_and_a_profile_before_name_is_replaced_not_duplicated() {
        let crlf = TEXT.replace('\n', "\r\n");
        let out = splice_profile(
            &crlf,
            "shows",
            "profile:\n  - keyword: a\n    weight: 1.0\n",
        )
        .unwrap();
        assert!(
            out.contains("\r\n") && !out.replace("\r\n", "").contains('\n'),
            "{out:?}"
        );
        let reordered = "rule:\n  blocks:\n    - mode: all\n      pools:\n        - plugin: p.rhai\n          profile:\n            - { keyword: a, weight: 1.0 }\n          name: movies\n";
        let out = splice_profile(
            reordered,
            "movies",
            "profile:\n  - keyword: b\n    weight: 2.0\n",
        )
        .unwrap();
        assert_eq!(out.matches("profile:").count(), 1, "{out}");
        assert!(
            out.contains("keyword: b") && !out.contains("keyword: a"),
            "{out}"
        );
    }

    #[test]
    fn a_block_with_a_comment_or_an_ambiguous_name_is_refused() {
        let commented = TEXT.replace(
            "            - { keyword: heist, weight: 2.0 }\n",
            "            # why\n            - { keyword: heist, weight: 2.0 }\n",
        );
        assert!(
            splice_profile(&commented, "movies", "")
                .unwrap_err()
                .contains("comment")
        );
        let twice = format!("{TEXT}    - mode: all\n      pools:\n        - name: movies\n");
        assert!(
            splice_profile(&twice, "movies", "")
                .unwrap_err()
                .contains("more than once")
        );
        assert!(
            splice_profile(TEXT, "nope", "")
                .unwrap_err()
                .contains("no block-style")
        );
    }
}
