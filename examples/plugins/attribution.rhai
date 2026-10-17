// An annotate plugin that names who has recently watched each item. It writes
// the line to metadata.attribution; an overlay script reads
// metadata.attribution and draws it (ADR 0016).
//
// Set tautulli_url() and tautulli_api_key() below, then name this script in a
// channel's `annotate:` block with `capabilities: [http]`. A plugin cannot read
// the environment, so the key lives in this file: keep a filled-in copy out of
// tracked files.
//
// Limits: it makes one Tautulli call per item, and an annotate pass shares one
// 30-second http budget, so a long schedule leaves its later items without a
// line. Tautulli's `search` matches title text, so "Alien" also credits
// watchers of "Aliens". Turning this on names every viewer to everyone
// watching the channel; that is a privacy decision for the server's owner.
fn tautulli_url() { "http://tautulli.local:8181" }
fn tautulli_api_key() { "" }
fn max_names() { 3 }

fn hooks() { ["annotate"] }
fn capabilities() { ["http"] }

fn query_safe(text) {
    let out = text;
    out.replace(" ", "%20");
    out.replace("&", "%26");
    out.replace("#", "%23");
    out.replace("+", "%2B");
    out
}

fn annotate(ctx, item) {
    if tautulli_api_key() == "" || item.title == () { return (); }
    let url = tautulli_url() + "/api/v2?apikey=" + tautulli_api_key()
        + "&cmd=get_history&length=50&search=" + query_safe(item.title);
    let rows = parse_json(ctx.http(url, "GET", "")).response.data.data;
    let names = [];
    for row in rows {
        let who = row.friendly_name;
        if who != () && !names.contains(who) { names.push(who); }
    }
    if names.len() == 0 { return (); }
    let line = "Watched recently by ";
    for i in 0..min(names.len(), max_names()) {
        if i > 0 { line += ", "; }
        line += names[i];
    }
    if names.len() > max_names() {
        line += " and " + (names.len() - max_names()) + " others";
    }
    #{ attribution: line }
}
