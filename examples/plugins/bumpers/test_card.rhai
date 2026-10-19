// The align/fill integration fixture: black with a text card naming the
// programme that follows, stretching to fill whatever gap it is handed. The
// segment loops, so this script can take the last draw of any gap.
//
// The overlay path is relative to the channel config's directory, so this
// script is written for a channel under examples/channels/.
fn hooks() { ["bumper"] }

fn build(ctx) {
    let title = if ctx.next == () { "" } else { ctx.next.title ?? "" };
    let t = timeline();
    t.push(segment().black()
        .name("test-card")
        .overlay("../../crates/etv-overlay/fixtures/scripts/text_card.rhai",
                 #{ lines: ["Up next", title], font_size: 48.0 })
        .flex(3.0, 15.0, 1)
        .looped());
    t
}
