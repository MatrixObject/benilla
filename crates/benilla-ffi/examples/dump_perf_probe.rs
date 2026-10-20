// Dump performance decomposition: bn_ui_dump costs 1-1.5s per call in PIE
// (BnPerf line), which stalls the menu's 0.5s refresh. Measure where the
// time goes so the optimisation targets the real hotspot instead of a guess.
//
// Variants (same helpers/rows as UI_DUMP_LUA, only the walk differs):
//   V0 walk  : EnumerateFrames + IsVisible only (frame walk floor)
//   V2 visreg: all F rows, regions only under visible frames
//   V3 visall: rows only for visible frames (F + R)
// Plus bn_ui_dump (the shipped baseline) and region/frame counts.
use benilla::*;

fn eval_ms(code: &str) -> (f64, usize) {
    let Ok(c) = std::ffi::CString::new(code) else {
        return (0.0, 0);
    };
    let mut buf = vec![0u8; 8 * 1024 * 1024];
    let t0 = std::time::Instant::now();
    let n = bn_ui_eval(c.as_ptr(), buf.as_mut_ptr() as *mut i8, buf.len() as u32);
    (t0.elapsed().as_secs_f64() * 1000.0, n as usize)
}

fn dump_ms() -> (f64, u32) {
    let mut buf = vec![0u8; 16 * 1024 * 1024];
    let t0 = std::time::Instant::now();
    let n = bn_ui_dump(buf.as_mut_ptr() as *mut i8, buf.len() as u32);
    (t0.elapsed().as_secs_f64() * 1000.0, n)
}

// One script, three modes: the helpers and dump_one rows are identical to
// UI_DUMP_LUA, only the walk condition on MODE changes. MODE is spliced in
// as a Lua number by the caller.
const SCRIPT: &str = r#"
    local MODE = __MODE__
    local function num(v)
        if v == nil then return "0" end
        return string.format("%.1f", v)
    end
    local function ratio(v)
        if type(v) ~= "number" then return "1.00" end
        return string.format("%.2f", v)
    end
    local function clean(s)
        if s == nil then return "" end
        return (string.gsub(tostring(s), "[\t\r\n]", " "))
    end
    local function colour_str(fn)
        local ok, a, b, c, d = pcall(fn)
        if not ok then return "1.00\t1.00\t1.00\t1.00" end
        return table.concat({ ratio(a), ratio(b), ratio(c), ratio(d) }, "\t")
    end
    local function region_alpha_str(r)
        local ok, ra = pcall(function() return r:GetAlpha() end)
        if not ok then return "1.00" end
        return ratio(ra)
    end
    local FULL = "0.0000\t0.0000\t1.0000\t0.0000\t1.0000\t1.0000\t0.0000\t1.0000"
    local function texcoord_str(r)
        if not r.GetTexCoord then return FULL end
        local ok, ax, ay, bx, by, cx, cy, dx, dy =
            pcall(function() return r:GetTexCoord() end)
        if not ok or type(ax) ~= "number" or type(dx) ~= "number" then
            return FULL
        end
        return table.concat({
            string.format("%.4f", ax), string.format("%.4f", ay),
            string.format("%.4f", cx), string.format("%.4f", cy),
            string.format("%.4f", dx), string.format("%.4f", dy),
            string.format("%.4f", bx), string.format("%.4f", by)
        }, "\t")
    end
    local function layer_str(r)
        local ok, dl = pcall(function() return r:GetDrawLayer() end)
        if not ok or type(dl) ~= "string" then return "ARTWORK" end
        return dl
    end
    local function dump_one(f, vis)
        local mine = {}
        table.insert(mine, table.concat({ "F", clean(f:GetName()),
            vis and "1" or "0", clean(f:GetFrameStrata()),
            tostring(f:GetFrameLevel() or 0), num(f:GetLeft()), num(f:GetTop()),
            num(f:GetWidth()), num(f:GetHeight()),
            ratio(f:GetAlpha()) }, "\t"))
        if vis then
            for _, r in ipairs({ f:GetRegions() }) do
                if r.IsShown and not r:IsShown() then
                    -- skip
                else
                local ty = clean(r:GetObjectType())
                local extra = ""
                local colr = ""
                if ty == "Texture" then
                    extra = clean(r:GetTexture())
                    colr = colour_str(function() return r:GetVertexColor() end)
                elseif ty == "FontString" then
                    extra = clean(r:GetText())
                    colr = colour_str(function() return r:GetTextColor() end)
                end
                local font, size, flags, jh, jv = "", "0.0", "", "LEFT", "CENTER"
                if ty == "FontString" then
                    local okf, fp, fs, ff = pcall(function() return r:GetFont() end)
                    if okf then
                        font = clean(fp)
                        size = num(fs)
                        flags = clean(ff)
                    end
                    local okh, vh = pcall(function() return r:GetJustifyH() end)
                    if okh then jh = clean(vh) end
                    local okv, vv = pcall(function() return r:GetJustifyV() end)
                    if okv then jv = clean(vv) end
                end
                table.insert(mine, table.concat({ "R", clean(f:GetName()),
                    clean(r:GetName()), ty, num(r:GetLeft()), num(r:GetTop()),
                    num(r:GetWidth()), num(r:GetHeight()), extra,
                    colr, region_alpha_str(r), font, size, flags, jh, jv,
                    texcoord_str(r), layer_str(r) }, "\t"))
                end
            end
        end
        return mine
    end
    local out = {}
    local f = EnumerateFrames()
    while f do
        local vis = f:IsVisible()
        if MODE == 0 then
            -- walk only
        elseif MODE == 3 then
            if vis then
                local ok, lines = pcall(dump_one, f, vis)
                if ok and type(lines) == "table" then
                    for i = 1, table.getn(lines) do
                        table.insert(out, lines[i])
                    end
                end
            end
        else
            local ok, lines = pcall(dump_one, f, vis)
            if ok and type(lines) == "table" then
                for i = 1, table.getn(lines) do
                    table.insert(out, lines[i])
                end
            end
        end
        f = EnumerateFrames(f)
    end
    if MODE == 0 then return tostring(-1) end
    return table.concat(out, "\n")
"#;

fn main() {
    let data_dir = std::env::args().nth(1).unwrap_or_else(|| {
        r#"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data"#.to_string()
    });
    let cdir = std::ffi::CString::new(data_dir).unwrap();
    let world = bn_init(cdir.as_ptr());
    if world.is_null() {
        eprintln!("bn_ui_dump failed");
        std::process::exit(1);
    }
    let cworld = world as *const BnWorld;

    // Second arg "full" loads the whole FrameXML toc (in-world UI, 3749
    // frames) instead of the glue menu - that is the dump the HUD runs at
    // 2s intervals after enter_world.
    if std::env::args().nth(2).as_deref() == Some("full") {
        let frames = bn_ui_load(cworld);
        println!("== full FrameXML load: {frames} frames");
    } else {
        bn_ui_load_glue(cworld);
    }
    for _ in 0..3 {
        bn_ui_tick(0.1);
    }

    // Frame / region counts (the denominators for the timings).
    let counts = r#"
        local nf, nv, nr, nrv = 0, 0, 0, 0
        local f = EnumerateFrames()
        while f do
            nf = nf + 1
            local vis = f:IsVisible()
            if vis then nv = nv + 1 end
            local regs = { f:GetRegions() }
            for _, r in ipairs(regs) do
                nr = nr + 1
                if vis and (not r.IsShown or r:IsShown()) then nrv = nrv + 1 end
            end
            f = EnumerateFrames(f)
        end
        return nf .. " " .. nv .. " " .. nr .. " " .. nrv
    "#;
    let (ms, _) = eval_ms(counts);
    let out = {
        let Ok(c) = std::ffi::CString::new(counts) else { return };
        let mut buf = vec![0u8; 4096];
        let n = bn_ui_eval(c.as_ptr(), buf.as_mut_ptr() as *mut i8, buf.len() as u32);
        String::from_utf8_lossy(&buf[..n as usize]).into_owned()
    };
    println!(
        "== counts (frames visible regions visibleRegions): {} (count eval {:.0}ms)",
        out, ms
    );

    // Baseline: the shipped dump, twice (first warms textures/caches).
    for i in 0..2 {
        let (ms, n) = dump_ms();
        println!("== bn_ui_dump #{i}: {ms:.0}ms ({n} bytes)");
    }

    // Variants, twice each; report the second run.
    for mode in [0, 2, 3] {
        let code = SCRIPT.replace("__MODE__", &mode.to_string());
        let mut last = 0.0;
        let mut lastn = 0usize;
        for _ in 0..2 {
            let (ms, n) = eval_ms(&code);
            last = ms;
            lastn = n;
        }
        let name = match mode {
            0 => "V0 walk only    ",
            2 => "V2 vis regions  ",
            _ => "V3 visible rows ",
        };
        println!("== {name}: {last:.0}ms ({lastn} bytes)");
    }

    bn_shutdown(world);
}
