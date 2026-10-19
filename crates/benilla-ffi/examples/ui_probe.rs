// Stage 3 probe: load the stock FrameXML off the WoW install's chain, dump
// the widget tree through the FFI, and summarize it.

fn main() {
    let dir = std::ffi::CString::new(
        "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data",
    )
    .unwrap();
    let w = benilla::bn_init(dir.as_ptr());
    if w.is_null() {
        println!("bn_init failed");
        return;
    }

    let frames = benilla::bn_ui_load(w);
    println!("bn_ui_load -> {frames} frames");
    if frames == 0 {
        benilla::bn_shutdown(w);
        return;
    }

    let mut buf = vec![0u8; 16 * 1024 * 1024];
    let n = benilla::bn_ui_dump(buf.as_mut_ptr() as *mut i8, buf.len() as u32);
    println!("bn_ui_dump -> {n} bytes");
    if n == 0 {
        benilla::bn_shutdown(w);
        return;
    }
    let text = String::from_utf8_lossy(&buf[..n as usize]).into_owned();

    // TSV stats: F lines / R lines, sample named frames, texture + text hits.
    let (mut nf, mut nr, mut tex_hits, mut text_hits) = (0u32, 0u32, 0u32, 0u32);
    let mut named = Vec::new();
    let mut tex_samples = Vec::new();
    let mut text_samples = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.is_empty() {
            continue;
        }
        let get = |i: usize| f.get(i).copied().unwrap_or("");
        match get(0) {
            "F" => {
                nf += 1;
                if !get(1).is_empty() && named.len() < 30 {
                    named.push(format!(
                        "{} ({} {}x{} @ {},{})",
                        get(1),
                        if get(2) == "1" { "shown" } else { "hidden" },
                        // F: 1=name 2=shown 5=left 6=top 7=width 8=height
                        get(7),
                        get(8),
                        get(5),
                        get(6),
                    ));
                }
            }
            "R" => {
                nr += 1;
                let ty = get(3);
                let extra = get(8);
                if ty == "Texture" && !extra.is_empty() {
                    tex_hits += 1;
                    if tex_samples.len() < 8 {
                        tex_samples.push(extra.to_string());
                    }
                } else if ty == "FontString" && !extra.is_empty() {
                    text_hits += 1;
                    if text_samples.len() < 8 {
                        text_samples.push(extra.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    println!("lines: {nf} frames, {nr} regions");
    println!("textures with a path: {tex_hits}");
    for s in &tex_samples {
        println!("  tex {s}");
    }
    println!("fontstrings with text: {text_hits}");
    for s in &text_samples {
        println!("  txt {s}");
    }
    println!("named frames:");
    for s in &named {
        println!("  {s}");
    }

    benilla::bn_shutdown(w);
}
