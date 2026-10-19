// Probe: the offline main-menu flow - load GlueXML, dump the login screen,
// "log in" through the stub CGlueMgr host, load the fake roster, then press
// Enter World and collect the queued event. Mirrors what the UE host will do.
use benilla::*;

fn eval_str(code: &str) -> String {
    let Ok(c) = std::ffi::CString::new(code) else {
        return "<nul>".into();
    };
    let mut buf = vec![0u8; 4096];
    let n = bn_ui_eval(c.as_ptr(), buf.as_mut_ptr() as *mut i8, buf.len() as u32);
    String::from_utf8_lossy(&buf[..n as usize]).to_string()
}

fn dump_stats() -> (usize, usize, usize, bool) {
    let mut buf = vec![0u8; 16 * 1024 * 1024];
    let n = bn_ui_dump(buf.as_mut_ptr() as *mut i8, buf.len() as u32) as usize;
    let text = String::from_utf8_lossy(&buf[..n]).to_string();
    let frames = text.lines().filter(|l| l.starts_with("F\t")).count();
    let shown = text
        .lines()
        .filter(|l| l.starts_with("F\t") && l.split('\t').nth(2) == Some("1"))
        .count();
    (n, frames, shown, text.contains("Nezha"))
}

/// The names of every shown frame right now.
fn shown_names() -> std::collections::HashSet<String> {
    let mut buf = vec![0u8; 16 * 1024 * 1024];
    let n = bn_ui_dump(buf.as_mut_ptr() as *mut i8, buf.len() as u32) as usize;
    let text = String::from_utf8_lossy(&buf[..n]).to_string();
    text.lines()
        .filter(|l| l.starts_with("F\t"))
        .filter(|l| l.split('\t').nth(2) == Some("1"))
        .filter_map(|l| l.split('\t').nth(1).map(String::from))
        .collect()
}

fn main() {
    let data_dir = std::env::args().nth(1).unwrap_or_else(|| {
        r#"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data"#.to_string()
    });
    let cdir = std::ffi::CString::new(data_dir.clone()).unwrap();
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
    println!(
        "== at load: TOSAccepted={} EULAAccepted={} TOSDeclineShown={} AccountLoginShown={} CharSelectShown={}",
        eval_str("return tostring(TOSAccepted())"),
        eval_str("return tostring(EULAAccepted())"),
        eval_str("return tostring(TOSDecline:IsShown())"),
        eval_str("return tostring(AccountLogin:IsShown())"),
        eval_str("return tostring(CharacterSelect:IsShown())"),
    );
    let (bytes, nf, shown, _) = dump_stats();
    println!("== login dump: {bytes} bytes, {nf} frames, {shown} shown");
    println!(
        "== screen root: {}x{}",
        eval_str("return tostring(GetScreenWidth())"),
        eval_str("return tostring(GetScreenHeight())")
    );
    // Actual coordinate bounds the login screen lays out to.
    {
        let mut buf = vec![0u8; 16 * 1024 * 1024];
        let n = bn_ui_dump(buf.as_mut_ptr() as *mut i8, buf.len() as u32) as usize;
        let text = String::from_utf8_lossy(&buf[..n]).to_string();
        let (mut max_r, mut max_t) = (0.0f32, 0.0f32);
        for line in text.lines().filter(|l| l.starts_with("R\t")) {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() >= 8 {
                if let (Ok(l), Ok(w), Ok(t)) = (
                    f[4].parse::<f32>(),
                    f[6].parse::<f32>(),
                    f[5].parse::<f32>(),
                ) {
                    max_r = max_r.max(l + w);
                    max_t = max_t.max(t);
                }
            }
        }
        println!("== login bounds: right={max_r} top={max_t}");
    }

    // Texcoord columns (19..22): every R row must now be 23 fields wide, and
    // Texture rows should carry sub-rects (9-slice boxes) where set.
    {
        let mut buf = vec![0u8; 16 * 1024 * 1024];
        let n = bn_ui_dump(buf.as_mut_ptr() as *mut i8, buf.len() as u32) as usize;
        let text = String::from_utf8_lossy(&buf[..n]).to_string();
        let (mut rows, mut wide, mut sub) = (0usize, 0usize, 0usize);
        let (mut out_of_range, mut login_rows) = (0usize, 0usize);
        let mut samples: Vec<String> = Vec::new();
        for line in text.lines().filter(|l| l.starts_with("R\t")) {
            let f: Vec<&str> = line.split('\t').collect();
            rows += 1;
            if f.len() >= 23 {
                wide += 1;
                let uv = &f[19..23];
                let full = uv == ["0.0000", "0.0000", "1.0000", "1.0000"];
                if f[3] == "Texture" && !full {
                    sub += 1;
                }
                let vals: Vec<f32> = uv.iter().map(|s| s.parse().unwrap_or(0.0)).collect();
                if vals.iter().any(|v| *v < -0.001 || *v > 1.001) {
                    out_of_range += 1;
                    if out_of_range <= 4 {
                        samples.push(format!("OOR {} {} uv={:?}", f[1], f[2], uv));
                    }
                }
                if f[1].contains("AccountLogin") && f[3] == "Texture" && login_rows < 12 {
                    samples.push(format!("LOGIN {} {} uv={:?}", f[1], f[2], uv));
                    login_rows += 1;
                }
            }
        }
        println!(
            "== texcoord: {rows} R rows, {wide} with 23 cols, {sub} texture sub-rects, \
             {out_of_range} out-of-[0,1], {login_rows} login samples"
        );
        for s in &samples {
            println!("   {s}");
        }
    }

    // Frame names the login screen actually uses (1.12 spelling).
    {
        let mut buf = vec![0u8; 16 * 1024 * 1024];
        let n = bn_ui_dump(buf.as_mut_ptr() as *mut i8, buf.len() as u32) as usize;
        let text = String::from_utf8_lossy(&buf[..n]).to_string();
        for line in text.lines().filter(|l| l.starts_with("F\t")) {
            let name = line.split('\t').nth(1).unwrap_or("");
            let lower = name.to_ascii_lowercase();
            if (lower.contains("login") || lower.contains("account") || lower.contains("password")
                || lower.contains("edit"))
                && line.split('\t').nth(2) == Some("1")
            {
                println!("   frame {name}");
            }
        }
    }

    // Record whether the login button's handler reaches DefaultServerLogin,
    // then click. (The glue events fired by the host land in script.errors() /
    // the dump, so watch visibility changes too.)
    let before = shown_names();
    println!(
        "== patch: {}",
        eval_str(r#"local orig = DefaultServerLogin _G.CAP = "not-called" DefaultServerLogin = function(a, p) _G.CAP = tostring(a) return orig(a, p) end return "wrapped""#)
    );
    let r = eval_str(r#"AccountLoginAccountEdit:SetText("acc") AccountLoginPasswordEdit:SetText("pw") AccountLoginLoginButton:Click() return "clicked""#);
    println!("== login eval: {r:?}");
    println!("== CAP={}", eval_str("return tostring(_G.CAP)"));
    // login -> charselect runs through GlueFrameFadeOut(AccountLoginUI, LOGIN_FADE_OUT=0.5s)
    // whose completion calls SetGlueScreen; GlueParent's OnUpdate (GlueFrameFadeUpdate)
    // is what advances the fade, so tick past it instead of once.
    let mut switched = false;
    for i in 0..50 {
        bn_ui_tick(0.1);
        if eval_str("return tostring(CharacterSelect:IsShown())") == "1" {
            println!("== charselect shown after {} ticks ({:.1}s)", i + 1, (i + 1) as f32 * 0.1);
            switched = true;
            break;
        }
    }
    if !switched {
        println!(
            "== charselect NEVER shown; AccountLogin={} GlueScreen={}",
            eval_str("return tostring(AccountLogin:IsShown())"),
            eval_str("return tostring(GetCurrentGlueScreenName and GetCurrentGlueScreenName() or '?')")
        );
    }
    // CharacterSelect_OnShow asks for the roster; the stub host queues it and bn_ui_tick
    // loads it a tick later.
    for _ in 0..5 {
        bn_ui_tick(0.1);
    }
    let after = shown_names();
    let newly: Vec<_> = after.difference(&before).collect();
    let hidden: Vec<_> = before.difference(&after).collect();
    println!("== newly shown: {newly:?}");
    println!("== newly hidden: {hidden:?}");
    let (bytes, nf, shown, has_nezha) = dump_stats();
    println!("== charselect dump: {bytes} bytes, {nf} frames, {shown} shown, roster={has_nezha}");

    // Press Enter World (the reference's own press path).
    let r = eval_str(r#"CharacterSelect_SelectCharacter(0) CharSelectEnterWorldButton:Click() return "clicked""#);
    println!("== enter world eval: {r:?}");
    bn_ui_tick(0.1);

    let mut ev = vec![0u8; 256];
    let n = bn_ui_poll_event(ev.as_mut_ptr() as *mut i8, ev.len() as u32);
    println!(
        "== event: {:?}",
        String::from_utf8_lossy(&ev[..n as usize]).to_string()
    );

    // Resize the root to a 16:9 viewport: dump coordinates must reflow to it
    // so the render host can draw and point 1:1 in viewport pixels.
    println!("== set_screen_size(1920,1080) -> {}", bn_ui_set_screen_size(1920.0, 1080.0));
    println!(
        "== screen root now: {}x{}",
        eval_str("return tostring(GetScreenWidth())"),
        eval_str("return tostring(GetScreenHeight())")
    );
    println!(
        "== Button1 rect after reflow: L={} T={} W={} H={}",
        eval_str("return tostring(CharSelectCharacterButton1:GetLeft())"),
        eval_str("return tostring(CharSelectCharacterButton1:GetTop())"),
        eval_str("return tostring(CharSelectCharacterButton1:GetWidth())"),
        eval_str("return tostring(CharSelectCharacterButton1:GetHeight())"),
    );

    bn_shutdown(world);
}
