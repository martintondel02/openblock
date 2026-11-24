//! Workspace automation: `cargo xtask <command>`.
//!
//! Commands are introduced as needed by the milestones in
//! `docs/plan/01-milestones.md`. This binary intentionally has no dependencies.

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("help") | None => {
            println!("openblock xtask");
            println!();
            println!("  help     show this message");
            println!();
            println!("Planned: dist, assets, packs, bench, pregen, docs.");
        }
        Some(other) => {
            eprintln!("xtask: unknown command '{other}' (try `cargo xtask help`)");
            std::process::exit(2);
        }
    }
}
