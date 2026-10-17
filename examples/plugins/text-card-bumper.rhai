// A text-card bumper: two seconds of black, then a card that stretches to
// fill the rest of the gap. The card's words come from the overlay script,
// not from the background. `shared/bumpers/card.rhai` is the overlay script the
// bumper overlay work adds; the timeline builds without it.
fn hooks() { ["bumper"] }

fn build(ctx) {
    let word = ctx.channel.vocabulary.random(ctx.rng);
    let title = if ctx.next == () { "" } else { ctx.next.title };

    let t = timeline();
    t.push(segment().black().fixed(2.0));
    t.push(segment().black()
        .overlay("shared/bumpers/card.rhai", #{ title: title, word: word ?? "" })
        .flex(3.0, 20.0, 1)
        .looped());
    t
}
