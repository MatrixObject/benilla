// Which archive wins a model file, and can it be read?
//
// `bn_init` registers the Data directory's *.MPQ in `read_dir` order, and the
// FFI's byte readers walk that list in reverse and stop at the first entry
// that *lists* the path. The winner therefore depends on the directory order,
// which is not sorted. This probe prints the order and each archive's verdict
// for one model file.
//
// Read-only probe: it touches the install and writes nothing.

use benilla_mpq::Archive;

fn main() {
    let data_dir = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    let target = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "world\\critter\\fish\\fish01.m2".to_string());

    let mut archives: Vec<(String, Archive)> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(data_dir).unwrap().flatten() {
        let p = entry.path();
        if !p
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("mpq"))
        {
            continue;
        }
        match Archive::open(&p) {
            Ok(a) => archives.push((p.file_name().unwrap().to_string_lossy().into_owned(), a)),
            Err(e) => skipped.push(format!("{} ({e})", p.display())),
        }
    }
    println!("{} archives opened, {} skipped", archives.len(), skipped.len());
    for s in &skipped {
        println!("  skipped: {s}");
    }

    // The chain's order: benilla_formats::Chain::open sorts by patch level.
    let mut chain_order: Vec<usize> = (0..archives.len()).collect();
    chain_order.sort_by_key(|&i| archives[i].0.to_lowercase());

    println!("\nread_dir order (the FFI's registration order):");
    for (i, (name, a)) in archives.iter().enumerate() {
        let has = a.contains(&target);
        let verdict = if !has {
            "-".to_string()
        } else if a.is_delete_marker(&target) {
            "delete-marker".to_string()
        } else {
            match a.read_file(&target) {
                Ok(b) => format!("ok, {} bytes", b.len()),
                Err(e) => format!("READ FAILED: {e}"),
            }
        };
        println!("  [{i:2}] {name:<28} contains={has:<5} {verdict}");
    }

    println!("\nThe FFI's winner (last that lists it, reverse order):");
    for (name, a) in archives.iter().rev() {
        if !a.contains(&target) {
            continue;
        }
        match a.read_file(&target) {
            Ok(b) => {
                println!("  {name} -> ok, {} bytes", b.len());
                break;
            }
            Err(e) => println!("  {name} -> READ FAILED: {e} (the FFI gives up here)"),
        }
    }
}
