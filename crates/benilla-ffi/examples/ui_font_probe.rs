// One-off probe: bn_ui_text_bitmap (FontString -> RGBA) end to end.
// Checks: face off the chain, CJK coverage, outline sizes, pen metrics, and
// that the extended dump rows carry the font/justify columns.
use benilla::*;

fn raster(
    world: *const BnWorld,
    font: &str,
    size: f32,
    outline: u32,
    text: &str,
) -> (u32, u32, u32, f32, f32, f32, f32, usize) {
    let cf = std::ffi::CString::new(font).unwrap();
    let ct = std::ffi::CString::new(text).unwrap();
    let mut buf = vec![0u8; 8 * 1024 * 1024];
    let (mut w, mut h) = (0u32, 0u32);
    let (mut ox, mut oy, mut pw, mut ph) = (0f32, 0f32, 0f32, 0f32);
    let n = bn_ui_text_bitmap(
        world as *const _,
        cf.as_ptr(),
        size,
        outline,
        ct.as_ptr(),
        buf.as_mut_ptr(),
        buf.len() as u32,
        &mut w,
        &mut h,
        &mut ox,
        &mut oy,
        &mut pw,
        &mut ph,
    );
    let lit = if n > 0 {
        buf[..n as usize].chunks(4).filter(|p| p[3] > 0).count()
    } else {
        0
    };
    (n, w, h, ox, oy, pw, ph, lit)
}

fn main() {
    let data_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| r#"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data"#.to_string());
    let cdir = std::ffi::CString::new(data_dir).unwrap();
    let world = bn_init(cdir.as_ptr());
    if world.is_null() {
        eprintln!("bn_init failed");
        std::process::exit(1);
    }

    // Display size at 1080p: min(size, 32) * H/768 (the host's drawn_px law).
    let px = 12.0f32 * 1080.0 / 768.0;
    let cases: &[(&str, f32, u32, &str)] = &[
        ("Fonts\\FRIZQT__.TTF", px, 0, "Account:"),
        ("Fonts\\FRIZQT__.TTF", px, 1, "Account: nezha"),
        ("Fonts\\FRIZQT__.TTF", px, 2, "THICK OUTLINE"),
        ("Fonts\\FRIZQT__.TTF", px, 1, "确定"), // CJK coverage
        (
            "Fonts\\FRIZQT__.TTF",
            px,
            1,
            "|cffffd200Quest ready|r: 确定",
        ),
        ("Fonts\\FRIZQT__.TTF", px, 0, "Line one\nLine two"),
        ("Fonts\\FZLBJW.TTF", px, 1, "确定连招"), // the glue menu's own CJK face
        ("", px, 1, "fallback face"),
        ("Fonts\\NOSUCH.TTF", 0.0, 1, "zero size refused"),
    ];
    let mut fail = 0;
    for &(font, size, outline, text) in cases {
        let (n, w, h, ox, oy, pw, ph, lit) = raster(world, font, size, outline, text);
        let expect_fail = size <= 0.5;
        let ok = if expect_fail {
            n == 0
        } else {
            n > 0 && lit > 0
        };
        if !ok {
            fail += 1;
        }
        println!(
            "{} font={:?} size={size:.1} outline={outline} text={text:?} -> n={n} {w}x{h} \
             pen=({ox:.1},{oy:.1}) {pw:.1}x{ph:.1} lit={lit}",
            if ok { "ok  " } else { "FAIL" },
            if font.is_empty() { "<default>" } else { font },
        );
    }

    // CJK coverage: the standard text face must map the char to a real glyph,
    // not.notdef (whose box would also light up pixels). A non-BMP codepoint
    // is in no WoW 1.12 face, so its ink is the notdef box.
    {
        let (n1, _, _, _, _, _, _, lit1) = raster(world, "Fonts\\FZLBJW.TTF", px, 0, "确");
        let (n2, _, _, _, _, _, _, lit2) = raster(world, "Fonts\\FZLBJW.TTF", px, 0, "\u{20000}");
        let ok = n1 > 0 && (n2 == 0 || lit1 > lit2 * 2);
        if !ok {
            fail += 1;
        }
        println!(
            "{} CJK glyph present in FZLBJW: 确 lit={lit1}, U+20000 lit={lit2}",
            if ok { "ok  " } else { "FAIL" }
        );
        // FRIZQT is Latin-only (the client has no glyph fallback either):
        // its 确 is the notdef box, which the check documents rather than fails.
        let (_, _, _, _, _, _, _, friz) = raster(world, "Fonts\\FRIZQT__.TTF", px, 0, "确");
        println!("info FRIZQT__.TTF 确 lit={friz} (Latin-only face, expected notdef)");
    }

    // Dump columns: load the glue menu, pull a dump, and check a FontString
    // row now ends in font / size / flags / justifyH / justifyV.
    let frames = bn_ui_load_glue(world as *const _);
    println!("bn_ui_load_glue -> {frames} frames");
    if frames > 0 {
        let mut dump = vec![0u8; 16 * 1024 * 1024];
        let n = bn_ui_dump(dump.as_mut_ptr() as *mut i8, dump.len() as u32);
        let text = String::from_utf8_lossy(&dump[..n as usize]).into_owned();
        let mut checked = 0;
        for line in text.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            if f.first() == Some(&"R") && f.get(3) == Some(&"FontString") {
                checked += 1;
                if f.len() != 19 {
                    println!("FAIL FontString row has {} cols: {line}", f.len());
                    fail += 1;
                    break;
                }
                if checked == 1 {
                    println!(
                        "ok  FontString row cols 14..18: font={:?} size={} flags={:?} jh={} jv={}",
                        f[14], f[15], f[16], f[17], f[18]
                    );
                }
            }
        }
        if checked == 0 {
            println!("FAIL no FontString rows in the glue dump");
            fail += 1;
        } else {
            println!("ok  {checked} FontString rows all have 19 columns");
        }
    } else {
        println!("FAIL bn_ui_load_glue returned 0");
        fail += 1;
    }

    bn_shutdown(world);
    println!("{}", if fail == 0 { "ALL GREEN" } else { "FAILURES" });
    std::process::exit(if fail == 0 { 0 } else { 1 });
}
