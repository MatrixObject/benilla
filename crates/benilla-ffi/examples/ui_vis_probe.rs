// Probe: visibility audit of the UI dump. The dump script marks an F row by
// `IsShown()` alone, which (like the 1.12 API) ignores hidden ancestors, and
// it never asks a region whether it is itself shown. Both leak rows the
// renderer must not draw. Counts, at the login screen and after reflow:
//   shown frames, shown-but-not-visible frames (ancestor leak),
//   regions under shown frames, regions hidden on their own.
use benilla::*;

fn eval_str(code: &str) -> String {
    let Ok(c) = std::ffi::CString::new(code) else {
        return "<nul>".into();
    };
    let mut buf = vec![0u8; 4096];
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

    bn_ui_load_glue(cworld);
    for _ in 0..3 {
        bn_ui_tick(0.1);
    }

    // Lua 5.0: no `#`, no `while ... do` with break-free oddities needed here.
    let audit = r#"
        local ns, nleak, nreg, nreghid, nregvisleak = 0, 0, 0, 0, 0
        local f = EnumerateFrames()
        while f do
            if f:IsShown() then
                ns = ns + 1
                if not f:IsVisible() then nleak = nleak + 1 end
                local regs = { f:GetRegions() }
                local i = 1
                while regs[i] do
                    local r = regs[i]
                    nreg = nreg + 1
                    if r.IsShown and not r:IsShown() then
                        nreghid = nreghid + 1
                    end
                    if r.IsVisible and not r:IsVisible() then
                        nregvisleak = nregvisleak + 1
                    end
                    i = i + 1
                end
            end
            f = EnumerateFrames(f)
        end
        return ns .. " " .. nleak .. " " .. nreg .. " " .. nreghid .. " " .. nregvisleak
    "#;
    let counts = eval_str(audit);
    println!(
        "== login audit: shown={} shownNotVisible={} regions={} regionHidden={} regionNotVisible={}",
        {
            let v: Vec<&str> = counts.split(' ').collect();
            v.first().copied().unwrap_or("?")
        },
        {
            let v: Vec<&str> = counts.split(' ').collect();
            v.get(1).copied().unwrap_or("?")
        },
        {
            let v: Vec<&str> = counts.split(' ').collect();
            v.get(2).copied().unwrap_or("?")
        },
        {
            let v: Vec<&str> = counts.split(' ').collect();
            v.get(3).copied().unwrap_or("?")
        },
        {
            let v: Vec<&str> = counts.split(' ').collect();
            v.get(4).copied().unwrap_or("?")
        },
    );

    // Which shown frames are the ancestor-leak ones? Sample their names.
    let leak_names = r#"
        local out = {}
        local f = EnumerateFrames()
        while f do
            if f:IsShown() and not f:IsVisible() then
                table.insert(out, tostring(f:GetName()))
            end
            f = EnumerateFrames(f)
        end
        return table.concat(out, ",")
    "#;
    let names = eval_str(leak_names);
    println!("== leak frames: {}", &names[..names.len().min(1200)]);

    // The frames that ARE effectively visible (the set the renderer should draw).
    let vis_names = r#"
        local out = {}
        local f = EnumerateFrames()
        while f do
            if f:IsVisible() then
                table.insert(out, tostring(f:GetName()))
            end
            f = EnumerateFrames(f)
        end
        return table.concat(out, ",")
    "#;
    println!("== visible frames: {}", eval_str(vis_names));

    // Sample regions hidden on their own under shown frames (name/type/texture).
    let hid_reg = r#"
        local out = {}
        local f = EnumerateFrames()
        while f do
            if f:IsShown() then
                local regs = { f:GetRegions() }
                local i = 1
                while regs[i] do
                    local r = regs[i]
                    if r.IsShown and not r:IsShown() then
                        local tex = ""
                        if r.GetTexture then local ok, t = pcall(function() return r:GetTexture() end) if ok then tex = tostring(t or "") end end
                        table.insert(out, tostring(f:GetName()) .. "/" .. tostring(r:GetName()) .. ":" .. tostring(r:GetObjectType()) .. ":" .. tex)
                    end
                    i = i + 1
                end
            end
            f = EnumerateFrames(f)
        end
        return table.concat(out, " | ")
    "#;
    let hr = eval_str(hid_reg);
    println!("== hidden regions ({} chars): {}", hr.len(), &hr[..hr.len().min(1500)]);

    bn_shutdown(world);
}
