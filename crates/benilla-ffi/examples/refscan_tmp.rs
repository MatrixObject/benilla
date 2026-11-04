// Scratch: find which FrameXML file defines a global by substring scan.
fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    let needle = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "MATERIAL_TEXT_COLOR_TABLE".into());
    let mut chain = benilla_formats::Chain::open(std::path::Path::new(data)).unwrap();
    if let Some(path) = std::env::args().nth(2) {
        let bytes = chain.read_file(&path).unwrap();
        println!("{}", String::from_utf8_lossy(&bytes));
        return;
    }
    let entries = chain.list().unwrap();
    let names: Vec<String> = entries
        .into_iter()
        .map(|e| e.name)
        .filter(|n| {
            let l = n.to_ascii_lowercase();
            l.starts_with("interface\\") && (l.ends_with(".lua") || l.ends_with(".xml"))
        })
        .collect();
    println!("{} framexml lua files", names.len());
    for name in names {
        let Ok(bytes) = chain.read_file(&name) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        if let Some(pos) = text.find(&needle) {
            let start = text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
            let end = text[pos..].find('\n').map(|i| pos + i).unwrap_or(text.len());
            println!("{} :: {}", name, &text[start..end]);
            let ctx_start = text[..pos].match_indices('\n').rev().nth(20).map(|(i, _)| i + 1).unwrap_or(0);
            let ctx_end = text[pos..].match_indices('\n').nth(20).map(|(i, _)| pos + i).unwrap_or(text.len());
            println!("--- context ---\n{}\n---", &text[ctx_start..ctx_end]);
        }
    }
}
