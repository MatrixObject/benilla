fn main() {
    let mut chain = benilla_formats::Chain::open(
        std::path::Path::new("I:/WOW/World of Warcraft 1.12.1.5875 zhCN/Data"),
    )
    .unwrap();
    for name in [
        "Interface\\Minimap\\MinimapArrow.blp",
        "Interface\\Minimap\\ObjectIcons.blp",
    ] {
        let Ok(bytes) = chain.read_file(name) else {
            println!("{name}: MISS");
            continue;
        };
        let Ok((w, h, rgba)) = benilla_formats::blp_to_rgba(&bytes) else {
            println!("{name}: decode fail");
            continue;
        };
        println!("{name}: {w}x{h} {} bytes rgba", rgba.len());
        let cols = 40usize;
        let rows = 24usize;
        for r in 0..rows {
            let mut line = String::new();
            for c in 0..cols {
                let x = (c * w as usize) / cols;
                let y = (r * h as usize) / rows;
                let i = (y * w as usize + x) * 4;
                let a = rgba[i + 3] as usize;
                let lum =
                    (rgba[i] as usize + rgba[i + 1] as usize + rgba[i + 2] as usize) / 3;
                line.push(if a < 40 {
                    '.'
                } else if lum > 200 {
                    '#'
                } else if lum > 120 {
                    '+'
                } else {
                    '-'
                });
            }
            println!("{line}");
        }
    }
}
