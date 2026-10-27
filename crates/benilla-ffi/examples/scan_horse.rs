// Scan MPQs for all BLPs whose path mentions "ridinghorse" (uses the embedded
// (listfile) each MPQ ships).
fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    let needle = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ridinghorse".to_string());
    let needle = needle.to_lowercase();

    let files: Vec<String> = std::fs::read_dir(data)
        .expect("read Data dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.to_lowercase().ends_with(".mpq"))
        .collect();

    for f in &files {
        let p = format!("{data}\\{f}");
        let a = match benilla_mpq::Archive::open(&p) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let Ok(list_bytes) = a.read_file("(listfile)") else {
            continue;
        };
        let text = String::from_utf8_lossy(&list_bytes);
        let mut hits = Vec::new();
        for line in text.lines() {
            let l = line.trim();
            let ll = l.to_lowercase();
            if ll.contains(&needle) {
                hits.push(l.to_string());
            }
        }
        if !hits.is_empty() {
            println!("{f}:");
            for h in hits {
                println!("   {h}");
            }
        }
    }
}
