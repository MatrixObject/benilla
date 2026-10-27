// Stage A feasibility probe for the login-screen glue booth (the missing
// background): prove the four links the UE host will need hold before any FFI
// is written.
// 1. Lua: the shown Model frame exposes GetModel -> the booth MDX path.
// 2. MPQ: the path is readable (both.mdx and.m2 spellings checked).
// 3. M2 : camera table index 0 parses (eye/target/roll/fov/near/far).
// 4. FFI: bn_model_fill's loader returns vertices for the same path.
use benilla::*;
use benilla_mpq::Archive;
use std::io::Cursor;

fn eval_str(code: &str) -> String {
    let Ok(c) = std::ffi::CString::new(code) else {
        return "<nul>".into();
    };
    let mut buf = vec![0u8; 16 * 1024];
    let n = bn_ui_eval(c.as_ptr(), buf.as_mut_ptr() as *mut i8, buf.len() as u32);
    String::from_utf8_lossy(&buf[..n as usize]).to_string()
}

fn main() {
    let data_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| r#"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data"#.to_string());
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
        "== glue screen: {}",
        eval_str("return tostring(GetCurrentGlueScreenName and GetCurrentGlueScreenName() or '?')")
    );

    // 1. Model frames the login screen shows (1.12 GlueXML names a Model
    // frame and pushes the MDX through SetModel / the file= attribute).
    println!("== model frames:");
    println!(
        "{}",
        eval_str(
            r#"local out = {}
               local n = 0
               local f = EnumerateFrames()
               while f do
                   if f.GetModel and type(f.GetModel) == "function" then
                       local ok, m = pcall(function() return f:GetModel() end)
                       local ok2, sh = pcall(function() return f:IsShown() end)
                       local ok3, l = pcall(function() return f:GetLeft() end)
                       local ok4, t = pcall(function() return f:GetTop() end)
                       local ok5, w = pcall(function() return f:GetWidth() end)
                       local ok6, h = pcall(function() return f:GetHeight() end)
                       local ok7, fc = pcall(function() return f:GetFacing() end)
                       local ok8, sc = pcall(function() return f:GetModelScale() end)
                       local ok9, px = pcall(function() return f:GetPosition() end)
                       n = n + 1
                       out[n] = string.format(
                           "%s shown=%s model=%s rect=[%s,%s,%s,%s] facing=%s scale=%s pos=%s",
                           tostring(f:GetName()), tostring(ok2 and sh), tostring(ok and m),
                           tostring(ok3 and l), tostring(ok4 and t),
                           tostring(ok5 and w), tostring(ok6 and h),
                           tostring(ok7 and fc), tostring(ok8 and sc),
                           tostring(ok9 and px))
                   end
                   f = EnumerateFrames(f)
               end
               return table.concat(out, "\n")"#
        )
    );

    // The path the booth builder uses (glue_booth: token -> UI_<token> MDX).
    let path_mdx = r"Interface\Glues\Models\UI_MainMenu\UI_MainMenu.mdx";
    let path_m2 = r"Interface\Glues\Models\UI_MainMenu\UI_MainMenu.m2";

    // 2. MPQ readability: which spelling the 1.12 chain actually stores.
    let mut bytes: Option<Vec<u8>> = None;
    let files: Vec<String> = std::fs::read_dir(&data_dir)
        .expect("read Data")
        .filter_map(|e| e.ok())
        .map(|e| e.path().to_string_lossy().into_owned())
        .filter(|p| p.to_lowercase().ends_with(".mpq"))
        .collect();
    for mpq in &files {
        let Ok(a) = Archive::open(mpq) else { continue };
        for cand in [path_mdx, path_m2] {
            if a.contains(cand) {
                match a.read_file(cand) {
                    Ok(b) => {
                        println!(
                            "== mpq: {cand} readable ({} bytes) from {}",
                            b.len(),
                            std::path::Path::new(mpq)
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                        );
                        if bytes.is_none() {
                            bytes = Some(b);
                        }
                    }
                    Err(e) => println!("== mpq: {cand} contains but read failed: {e}"),
                }
            }
        }
    }
    let Some(bytes) = bytes else {
        println!("== mpq: BOOTH MODEL NOT FOUND in any archive");
        bn_shutdown(world);
        std::process::exit(2);
    };

    // 3. Camera table record 0 = the glue pane's camera (glue_booth: scene.cam
    // = model.camera0). Values are WoW/model space, radians.
    match benilla_formats::parse_m2_camera(&bytes, 0) {
        Some(c) => println!(
            "== camera0: eye=({:.3},{:.3},{:.3}) target=({:.3},{:.3},{:.3}) \
             roll={:.4} fov={:.4} rad ({} deg) near={:.3} far={:.1}",
            c.position[0],
            c.position[1],
            c.position[2],
            c.target[0],
            c.target[1],
            c.target[2],
            c.roll,
            c.fov,
            c.fov.to_degrees(),
            c.near_clip,
            c.far_clip
        ),
        None => println!("== camera0: PARSE FAILED (no camera table)"),
    }
    // Header bounds give the booth's world extent (fog/pane framing sanity).
    if let Ok(fmt) = benilla_m2::parse_m2(&mut Cursor::new(bytes.as_slice())) {
        let m = fmt.model();
        println!(
            "== bounds: min=({:.2},{:.2},{:.2}) max=({:.2},{:.2},{:.2}) verts={} bones={} \
             camera1={}",
            m.header.bounding_box_min[0],
            m.header.bounding_box_min[1],
            m.header.bounding_box_min[2],
            m.header.bounding_box_max[0],
            m.header.bounding_box_max[1],
            m.header.bounding_box_max[2],
            m.vertices.len(),
            m.bones.len(),
            benilla_formats::parse_m2_camera(&bytes, 1).is_some()
        );
    }

    // 4. The FFI loader UE would call (BnMapTile::RenderModel path) must agree.
    for cand in [path_mdx, path_m2] {
        let c = std::ffi::CString::new(cand).unwrap();
        let nv = bn_model_vertex_count(cworld, c.as_ptr());
        let nt = bn_model_triangle_count(cworld, c.as_ptr());
        println!("== bn_model_fill loader: {cand} -> {nv} verts, {nt} tris");
    }

    // 5. The FFI itself: one call must hand back the whole booth payload
    // (path normalized, camera raw-WoW, stock fog) the UE host consumes.
    let mut booth: BnBooth = unsafe { std::mem::zeroed() };
    let has = bn_ui_booth(cworld, &mut booth);
    let path = booth
        .path
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8 as char)
        .collect::<String>();
    println!("== bn_ui_booth: has={has} path={path}");
    if has != 0 {
        println!(
            "   rect={:?} facing={:.4} scale={} pos={:?}",
            booth.rect, booth.facing, booth.scale, booth.pos
        );
        println!(
            "   cam eye={:?} target={:?} roll={:.4} fov={:.4} near={:.3} far={:.1}",
            booth.cam_eye,
            booth.cam_target,
            booth.cam_roll,
            booth.cam_fov,
            booth.cam_near,
            booth.cam_far
        );
        println!(
            "   fog rgb={:?} near={} far={:.1}",
            booth.fog_rgb, booth.fog_near, booth.fog_far
        );
    }

    bn_shutdown(world);
}
