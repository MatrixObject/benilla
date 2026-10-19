// Probe: the keyboard FFI on the login screen - key names in, WoW keymap
// chords applied middle-side, EditBox text verified through the script env.
// Walks: autofocus typing -> backspace -> caret move+insert -> Ctrl+A select
// -> Delete clear -> paste -> click-to-focus path.

use benilla::*;

fn eval_str(code: &str) -> String {
    let Ok(c) = std::ffi::CString::new(code) else {
        return "<nul>".into();
    };
    let mut buf = vec![0u8; 4096];
    let n = bn_ui_eval(c.as_ptr(), buf.as_mut_ptr() as *mut i8, buf.len() as u32);
    String::from_utf8_lossy(&buf[..n as usize]).to_string()
}

fn cstr(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).unwrap()
}

/// Reference event order: key-down, the typed character, then key-up.
fn type_char(ch: &str, key: &str) -> (u32, u32) {
    let ck = cstr(key);
    let cc = cstr(ch);
    let walk = bn_ui_key(ck.as_ptr(), 0, 1);
    let consumed = bn_ui_char(cc.as_ptr());
    let up = bn_ui_key(ck.as_ptr(), 0, 0);
    let _ = up;
    (walk, consumed)
}

fn key_down(key: &str, mods: u32) -> u32 {
    let ck = cstr(key);
    let r = bn_ui_key(ck.as_ptr(), mods, 1);
    bn_ui_key(ck.as_ptr(), mods, 0);
    r
}

fn acct() -> String {
    eval_str("return tostring(AccountLoginAccountEdit:GetText())")
}

fn main() {
    let data_dir = std::env::args().nth(1).unwrap_or_else(|| {
        r#"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data"#.to_string()
    });
    let world = bn_init(cstr(&data_dir).as_ptr());
    if world.is_null() {
        eprintln!("bn_init failed");
        std::process::exit(1);
    }
    let cworld = world as *const BnWorld;

    let frames = bn_ui_load_glue(cworld);
    bn_ui_set_screen_size(800.0, 600.0);
    for _ in 0..3 {
        bn_ui_tick(0.1);
    }
    println!("== load: {frames} frames, screen={}", eval_str("return GetCurrentGlueScreenName()"));
    let mut shown = eval_str("return tostring(AccountLogin:IsShown())") == "1";
    if !shown {
        // Land on the login screen whatever the glue flow opened on.
        println!("== forcing login screen: {}", eval_str(r#"SetGlueScreen("login") return GetCurrentGlueScreenName()"#));
        for _ in 0..5 {
            bn_ui_tick(0.1);
        }
        shown = eval_str("return tostring(AccountLogin:IsShown())") == "1";
    }
    println!("== AccountLogin shown={shown}");
    if !shown {
        println!("== abort: no login screen to type into");
        bn_shutdown(world);
        return;
    }

    // (1) Typing with no click: the account box should autoFocus and take it.
    for (ch, key) in [("N", "N"), ("e", "E"), ("z", "Z"), ("h", "H"), ("a", "A"),
                      ("1", "1"), ("2", "2"), ("3", "3")] {
        let (walk, consumed) = type_char(ch, key);
        println!("   type {ch:?}: keywalk={walk} (2=box) char_consumed={consumed}");
    }
    println!("== after autofocus typing: account={:?}", acct());

    // (2) Backspace drops the last character.
    let b = key_down("BACKSPACE", 0);
    println!("== backspace (walk={b}) -> {:?}", acct());

    // (3) Caret to the start, insert "X" at position 0.
    let l = key_down("LEFT", 0);
    key_down("LEFT", 0);
    key_down("LEFT", 0);
    key_down("LEFT", 0);
    key_down("LEFT", 0);
    key_down("LEFT", 0);
    key_down("LEFT", 0);
    let (_, _) = type_char("X", "X");
    println!("== LEFTx7+insert (walk={l}) -> {:?}", acct());

    // (4) Ctrl+A selects all, Delete clears the box.
    let _ = key_down("A", 2); // Ctrl held
    let d = key_down("DELETE", 0);
    println!("== ctrl+A then DELETE (walk={d}) -> {:?}", acct());

    // (5) Paste types the text in.
    let p = bn_ui_paste(cstr("paste77").as_ptr());
    println!("== paste -> consumed={p} text={:?}", acct());

    // (6) Click-to-focus path: focus the password box by clicking it, type.
    let center = eval_str(
        "local x,y = AccountLoginPasswordEdit:GetCenter() return tostring(x)..','..tostring(y)",
    );
    println!("== password center: {center}");
    if let Some((xs, ys)) = center.split_once(',') {
        if let (Ok(x), Ok(y)) = (xs.parse::<f32>(), ys.parse::<f32>()) {
            bn_ui_mouse_move(x, y);
            let c2 = cstr("LeftButton");
            bn_ui_mouse_button(x, y, c2.as_ptr(), 1);
            bn_ui_mouse_button(x, y, c2.as_ptr(), 0);
            for (ch, key) in [("p", "P"), ("w", "W")] {
                let (walk, _) = type_char(ch, key);
                println!("   type {ch:?} into password: keywalk={walk}");
            }
            println!(
                "== password text (visible via region) focus={}",
                eval_str("return tostring(AccountLoginPasswordEdit:HasFocus())")
            );
        }
    }
    println!("== account after password click: {:?}", acct());

    // (7) ENTER on the password box fires OnEnterPressed (login attempt).
    println!("== ENTER walk={}", key_down("ENTER", 0));
    for _ in 0..5 {
        bn_ui_tick(0.1);
    }
    println!("== after ENTER: screen={} CAP={}",
        eval_str("return GetCurrentGlueScreenName()"),
        eval_str("return tostring(_G.CAP)"));

    bn_shutdown(world);
}
