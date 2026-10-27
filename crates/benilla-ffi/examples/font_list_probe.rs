// One-off probe: which font faces does the chain carry, and which of them
// cover CJK? Reads the FrameXML/GlueXML Fonts.xml declarations, then tests
// each face for a real 确 glyph (vs the.notdef box) through the FFI.
use benilla::*;

fn lit_for(world: *mut BnWorld, font: &str, text: &str) -> usize {
    let cf = std::ffi::CString::new(font).unwrap();
    let ct = std::ffi::CString::new(text).unwrap();
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let (mut w, mut h) = (0u32, 0u32);
    let (mut a, mut b, mut c, mut d) = (0f32, 0f32, 0f32, 0f32);
    let n = bn_ui_text_bitmap(
        world as *const _,
        cf.as_ptr(),
        16.9,
        1,
        ct.as_ptr(),
        buf.as_mut_ptr(),
        buf.len() as u32,
        &mut w,
        &mut h,
        &mut a,
        &mut b,
        &mut c,
        &mut d,
    );
    if n == 0 {
        return 0;
    }
    buf[..n as usize].chunks(4).filter(|p| p[3] > 0).count()
}

fn main() {
    let data = std::env::args()
        .nth(1)
        .unwrap_or_else(|| r#"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data"#.to_string());
    let chain = benilla_formats::open_chain(std::path::Path::new(&data)).expect("chain opens");

    let mut faces: Vec<String> = Vec::new();
    for p in [
        r"Interface\FrameXML\Fonts.xml",
        r"Interface\FrameXML\zhCN\Fonts.xml",
        r"Interface\GlueXML\Fonts.xml",
        r"Interface\FrameXML\GlobalFonts.lua",
    ] {
        match chain.read(p) {
            Ok(bytes) => {
                let s = String::from_utf8_lossy(&bytes);
                println!("== {p} ({} bytes)", s.len());
                if p.ends_with("Fonts.xml") {
                    let head: String = s.chars().take(700).collect();
                    println!("{head}");
                }
                let mut i = 0usize;
                while let Some(at) = s[i..].find("font=\"") {
                    let start = i + at + 6;
                    let end = s[start..].find('"').map(|e| start + e).unwrap_or(s.len());
                    let v = s[start..end].to_string();
                    if v.to_ascii_lowercase().ends_with(".ttf") && !faces.contains(&v) {
                        faces.push(v);
                    }
                    i = end.max(start);
                }
            }
            Err(e) => println!("== {p}: {e}"),
        }
    }
    // A known glue face even if a Fonts.xml is missing.
    for extra in [r"Fonts\FZLBJW.TTF", r"Fonts\FRIZQT__.TTF"] {
        if !faces.iter().any(|f| f.eq_ignore_ascii_case(extra)) {
            faces.push(extra.to_string());
        }
    }
    println!("faces: {faces:?}");

    let cdir = std::ffi::CString::new(data).unwrap();
    let world = bn_init(cdir.as_ptr());
    if world.is_null() {
        eprintln!("bn_init failed");
        std::process::exit(1);
    }
    println!("{:<34} {:>6} {:>6} {:>8}", "face", "确", "U+20000", "latin");
    for f in &faces {
        let cjk = lit_for(world, f, "确");
        let nbp = lit_for(world, f, "\u{20000}");
        let latin = lit_for(world, f, "Account");
        let mark = if cjk > nbp * 2 && cjk > 0 {
            "CJK"
        } else {
            "   "
        };
        println!("{f:<34} {cjk:>6} {nbp:>6} {latin:>8}  {mark}");
    }
    bn_shutdown(world);
}
