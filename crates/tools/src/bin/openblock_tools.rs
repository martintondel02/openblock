//! The `openblock-tools` offline tool binary.

fn main() {
    ob_core::init_logging();
    let (flags, free) = ob_core::parse_args(std::env::args().skip(1));
    let has = |name: &str| flags.iter().any(|(n, _)| n == name);
    if has("version") {
        println!("{}", ob_core::version_banner("openblock-tools"));
        return;
    }
    if free.is_empty() && !has("help") {
        println!("{}", ob_core::version_banner("openblock-tools"));
        println!("usage: openblock-tools <command> [options]");
        println!("commands: version | inspect-world <file> | help");
        return;
    }
    match free.first().map(String::as_str) {
        Some("version") => println!("{}", ob_core::version_banner("openblock-tools")),
        Some("inspect-world") => {
            let Some(path) = free.get(1) else {
                eprintln!("openblock-tools: inspect-world needs a path");
                std::process::exit(2);
            };
            let registry = std::sync::Arc::new(ob_content::Registry::base());
            match std::fs::read(path) {
                Ok(bytes) => match ob_world::from_bytes(registry, &bytes) {
                    Ok(world) => {
                        println!(
                            "world: {} chunks, {} sections, seed {}, ~{} KiB block data",
                            world.chunk_count(),
                            world.section_count(),
                            world.seed(),
                            world.heap_bytes() / 1024
                        );
                    }
                    Err(err) => {
                        eprintln!("openblock-tools: {err}");
                        std::process::exit(1);
                    }
                },
                Err(err) => {
                    eprintln!("openblock-tools: cannot read {path}: {err}");
                    std::process::exit(1);
                }
            }
        }
        Some("help") | None => println!("usage: openblock-tools <version|inspect-world <file>|help>"),
        Some(other) => {
            eprintln!("openblock-tools: unknown command '{other}'");
            std::process::exit(2);
        }
    }
}
