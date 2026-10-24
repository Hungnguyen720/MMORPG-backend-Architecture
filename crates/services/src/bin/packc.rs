//! Content pack compiler: YAML source directory in, one binary pack out.
//! Validates every cross-reference, so a broken pack fails here rather
//! than on a live server.
//!
//!     packc tenants/default -o build/default.pack

use combat_core::ContentPack;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut src = None;
    let mut out = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" | "--out" => out = it.next(),
            "-h" | "--help" => {
                println!("packc <tenant dir> [-o out.pack]");
                return;
            }
            s => src = Some(s.to_string()),
        }
    }
    let Some(src) = src else {
        eprintln!("usage: packc <tenant dir> [-o out.pack]");
        std::process::exit(2);
    };
    let pack = match ContentPack::load_dir(&src) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{src}: {e}");
            std::process::exit(1);
        }
    };
    let bytes = pack.to_bytes();
    let out = out.unwrap_or_else(|| format!("{}.pack", pack.tenant.id));
    if let Some(dir) = std::path::Path::new(&out).parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).expect("creating output directory");
    }
    std::fs::write(&out, &bytes).unwrap_or_else(|e| panic!("{out}: {e}"));
    // Round trip, so what we wrote is exactly what servers will load.
    let back = ContentPack::load(&out).expect("reloading the written pack");
    assert_eq!(back.hash, pack.hash);
    println!(
        "{out}: \"{}\" ({}), {} classes, {} abilities, {} mobs, {} zones, {} items, {} bytes, hash {:016x}",
        pack.tenant.name,
        pack.tenant.id,
        pack.classes.len(),
        pack.abilities.len(),
        pack.mobs.len(),
        pack.zones.len(),
        pack.items.len(),
        bytes.len(),
        pack.hash
    );
}
