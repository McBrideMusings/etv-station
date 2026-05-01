use ersatztv_playout::playout::Playout;

fn main() {
    let playout = Playout::new(vec![]);
    println!(
        "etv-station bootstrap. linked schema version: {}",
        playout.version
    );
}
