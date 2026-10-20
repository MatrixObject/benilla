// Probe: every visible text (FontString) row on the glue login screen, raw,
// with all 28 dump columns -- to compare the label that DOES render in the
// render host ("帐号名称") against the login button's label that does not,
// and to capture the editbox's own regions (text / selection / caret) without
// clicking anything.
use benilla::*;

fn eval_str(code: &str) -> String {
    let Ok(c) = std::ffi::CString::new(code) else {
        return "<nul>".into();
    };
    let mut buf = vec![0u8; 64 * 1024];
    let n = bn_ui_eval(c.as_ptr(), buf.as_mut_ptr() as *mut i8, buf.len() as u32);
    String::from_utf8_lossy(&buf[..n as usize]).to_string()
}

fn main() {
    let data_dir = std::env::args().nth(1).unwrap_or_else(|| {
        r#"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data"#.to_string()
    });
    let cdir = std::ffi::CString::new(data_dir).unwrap();
    let world = bn_init(cdir.as_ptr());
    if world.is_null() {
        eprintln!("bn_init failed");
        std::process::exit(1);
    }
    let cworld = world as *const BnWorld;

    println!("== load: {} frames", bn_ui_load_glue(cworld));
    for _ in 0..3 {
        bn_ui_tick(0.1);
    }
    println!("== set_screen_size(1365.33,768) -> {}", bn_ui_set_screen_size(1365.33, 768.0));
    for _ in 0..3 {
        bn_ui_tick(0.1);
    }

    // Which editbox holds the keyboard focus before any click, and does the
    // caret region exist (hidden until focused / blinking)?
    println!(
        "== focused: {} | editbox shown: {}",
        eval_str("return tostring(AccountLoginAccountEdit and AccountLoginAccountEdit:HasFocus())"),
        eval_str("return tostring(AccountLoginAccountEdit:IsShown())"),
    );

    let mut buf = vec![0u8; 16 * 1024 * 1024];
    let n = bn_ui_dump(buf.as_mut_ptr() as *mut i8, buf.len() as u32) as usize;
    let text = String::from_utf8_lossy(&buf[..n]).to_string();

    println!("== visible F rows:");
    for l in text.lines().filter(|l| l.starts_with("F\t")) {
        let f: Vec<&str> = l.split('\t').collect();
        println!("  name={:32} visible={} strata={} level={} rect=({},{},{},{})",
            f[1], f[2], f[3], f[4], f[5], f[6], f[7], f[8]);
    }

    println!("== every visible FontString row (28 cols, raw):");
    for l in text.lines().filter(|l| l.starts_with("R\t")) {
        let f: Vec<&str> = l.split('\t').collect();
        if f[3] == "FontString" {
            println!("  {l}");
            let _ = f;
        }
    }

    println!("== rows of the account/password editbox frames:");
    for l in text.lines().filter(|l| l.starts_with("R\t")) {
        let f: Vec<&str> = l.split('\t').collect();
        let name = f[1];
        if name.contains("AccountEdit") || name.contains("PasswordEdit") {
            println!("  {l}");
        }
    }

    bn_shutdown(world);
}
