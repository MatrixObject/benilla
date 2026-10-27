// Probe: the UE host's real input path against the glue login screen --
// pointer move + button press (bn_ui_mouse_move / bn_ui_mouse_button) instead
// of `:Click`, plus the caret/label questions the render host cannot answer
// by itself:
// 1. does a pointer press on the login button hit-test and run its handler?
// 2. does a pointer press on the account editbox take focus (caret region)?
// 3. what does the button's label look like in the dump (text / font / shown)?
use benilla::*;

fn eval_str(code: &str) -> String {
    let Ok(c) = std::ffi::CString::new(code) else {
        return "<nul>".into();
    };
    let mut buf = vec![0u8; 64 * 1024];
    let n = bn_ui_eval(c.as_ptr(), buf.as_mut_ptr() as *mut i8, buf.len() as u32);
    String::from_utf8_lossy(&buf[..n as usize]).to_string()
}

fn dump_text() -> String {
    let mut buf = vec![0u8; 16 * 1024 * 1024];
    let n = bn_ui_dump(buf.as_mut_ptr() as *mut i8, buf.len() as u32) as usize;
    eprintln!("== dump bytes: {n}");
    String::from_utf8_lossy(&buf[..n]).to_string()
}

/// Every visible dump row owned by `frame` (frame|region|type|rect|texture|text).
fn rows_for(frame: &str) -> Vec<String> {
    dump_text()
        .lines()
        .filter(|l| l.starts_with("R\t"))
        .filter(|l| l.split('\t').nth(1) == Some(frame))
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            format!(
                "  rg={:28} ty={:10} l={:>7} t={:>7} w={:>7} h={:>7} layer={:9} tex='{}' text='{}'",
                f[2],
                f[3],
                f[4],
                f[5],
                f[6],
                f[7],
                f.get(27).copied().unwrap_or("?"),
                f.get(8).copied().unwrap_or(""),
                f.get(14).copied().unwrap_or(""),
            )
        })
        .collect()
}

/// Centre of `frame` in the VM's y-up screen units (what the host sends).
fn centre(frame: &str) -> Option<(f32, f32)> {
    let code = format!(
        "local f = {frame} return string.format('%.3f %.3f', \
         f:GetLeft() + f:GetWidth() / 2, f:GetTop() - f:GetHeight() / 2)"
    );
    let s = eval_str(&code);
    let mut it = s.split_whitespace();
    let x = it.next()?.parse().ok()?;
    let y = it.next()?.parse().ok()?;
    Some((x, y))
}

fn press(x: f32, y: f32) -> (u32, u32) {
    let mv = bn_ui_mouse_move(x, y);
    let dn = bn_ui_mouse_button(x, y, c"LeftButton".as_ptr(), 1);
    bn_ui_tick(0.05);
    let up = bn_ui_mouse_button(x, y, c"LeftButton".as_ptr(), 0);
    bn_ui_tick(0.05);
    println!("== press ({x:.1},{y:.1}): move={mv} down={dn} up={up}");
    (dn, up)
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
    let cworld = world as *const BnWorld;

    let frames = bn_ui_load_glue(cworld);
    println!("== load: {frames} frames");
    for _ in 0..3 {
        bn_ui_tick(0.1);
    }
    // The UE host hands the VM the viewport in seam units (px / (h/768)),
    // so a 1280x720 window is 1365.33 x 768. Pointer coords share that space.
    println!(
        "== set_screen_size(1365.33, 768) -> {}",
        bn_ui_set_screen_size(1365.33, 768.0)
    );
    for _ in 0..3 {
        bn_ui_tick(0.1);
    }

    // --- 1. button label: text / regions the render host would draw ---
    println!("== AccountLoginLoginButton:");
    for r in rows_for("AccountLoginLoginButton") {
        println!("{r}");
    }
    println!(
        "  frame text = {}",
        eval_str(
            "return type(AccountLoginLoginButton.GetText) == 'function' \
             and tostring(AccountLoginLoginButton:GetText()) or '<no GetText>'"
        )
    );

    // --- 2. pointer press on the account editbox FIRST: focus + caret row.
    // (Pressing the login button first submits the login and tears the glue
    // screen down, so the caret questions must come before it.)
    eval_str(r#"AccountLoginAccountEdit:SetText("acc") return "seeded""#);
    if let Some((x, y)) = centre("AccountLoginAccountEdit") {
        press(x, y);
    }
    println!("== editbox rows after click:");
    for r in rows_for("AccountLoginAccountEdit") {
        println!("{r}");
    }
    // Typing only reaches the box when it holds focus -- the UE host's keyboard
    // path (bn_ui_key/bn_ui_char) is the same entry point.
    bn_ui_key(c"Z".as_ptr(), 0, 1);
    bn_ui_key(c"Z".as_ptr(), 0, 0);
    bn_ui_char(c"Q".as_ptr());
    bn_ui_tick(0.1);
    let text_after = eval_str("return AccountLoginAccountEdit:GetText()");
    println!("== editbox text after typing: {text_after:?}");

    // --- 4. caret: the dump row the render host must paint itself ---
    println!("== editbox rows after typing (caret expected):");
    for r in rows_for("AccountLoginAccountEdit") {
        println!("{r}");
    }

    // --- 5. login button press LAST (it tears the screen down) ---
    println!(
        "== patch: {}",
        eval_str(
            r#"local orig = DefaultServerLogin _G.CAP = "not-called"
               DefaultServerLogin = function(a, p) _G.CAP = tostring(a) return orig(a, p) end
               return "wrapped""#
        )
    );
    if let Some((x, y)) = centre("AccountLoginLoginButton") {
        press(x, y);
    }
    println!(
        "== CAP after pointer press = {}",
        eval_str("return tostring(_G.CAP)")
    );

    bn_shutdown(world);
}
