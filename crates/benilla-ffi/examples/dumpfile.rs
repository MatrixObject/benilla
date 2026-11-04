// Scratch: dump one file from the 1.12.1 MPQ chain.
fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    let want = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "interface\\framexml\\spellbookframe.lua".into());
    let only = std::env::args().nth(2);
    let mut files: Vec<String> = std::fs::read_dir(data)
        .expect("data dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.to_lowercase().ends_with(".mpq"))
        .collect();
    files.sort();
    if let Some(only) = &only {
        files.retain(|n| n.eq_ignore_ascii_case(only));
    }
    for f in &files {
        let p = format!("{data}\\{f}");
        let Ok(a) = benilla_mpq::Archive::open(&p) else {
            continue;
        };
        if a.contains(&want) {
            println!("[{f}] CONTAINS");
            match a.read_file(&want) {
                Ok(bytes) => {
                    println!("--- {want} from {f} ---");
                    println!("{}", String::from_utf8_lossy(&bytes));
                    return;
                }
                Err(e) => println!("read failed: {e}"),
            }
        }
    }
    println!("not found: {want}");
}
