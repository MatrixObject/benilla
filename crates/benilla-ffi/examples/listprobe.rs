fn main() {
    let mut chain = benilla_formats::Chain::open(
        std::path::Path::new("I:/WOW/World of Warcraft 1.12.1.5875 zhCN/Data"),
    )
    .unwrap();
    let l = chain.list().unwrap();
    let mut n = 0;
    for e in &l {
        let lower = e.name.to_ascii_lowercase();
        if lower.contains("stormwind") && lower.ends_with(".wmo") {
            println!("{}", e.name);
            n += 1;
            if n > 25 {
                break;
            }
        }
    }
    println!("hits {n}");
    let _ = &mut chain;
}
