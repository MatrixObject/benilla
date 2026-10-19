// One-off probe: why do UI texture paths from bn_ui_dump fail bn_texture?
// Tries the raw path, then +".blp"/+".BLP" variants, prints which resolves.
use benilla::*;

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

    let paths = [
        r#"Interface\Buttons\UI-Panel-Button-Up"#,
        r#"Interface\TargetingFrame\UI-StatusBar"#,
        r#"Interface\Minimap\UI-Minimap-Border"#,
        r#"Interface\ChatFrame\ChatFrameBackground"#,
        r#"Interface\Buttons\UI-Quickslot2"#,
        r#"Interface\Tooltips\UI-Tooltip-Border"#,
        r#"Interface\Glues\Common\Glue-Tooltip-Border"#,
        r#"Interface\Glues\Common\Glue-Panel-Button-Up"#,
        r#"Interface\Tooltips\UI-Tooltip-Background"#,
    ];
    // Raw RGBA dumps for visual inspection (Python/PIL renders them to PNG).
    const DUMP: &[(&str, &str)] = &[
        (
            r#"Interface\Glues\Common\Glue-Tooltip-Border"#,
            r#"C:\Users\Administrator\AppData\Local\Temp\glue_tooltip_border.rgba"#,
        ),
        (
            r#"Interface\Glues\Common\Glue-Panel-Button-Up"#,
            r#"C:\Users\Administrator\AppData\Local\Temp\glue_panel_button_up.rgba"#,
        ),
        (
            r#"Interface\Tooltips\UI-Tooltip-Background"#,
            r#"C:\Users\Administrator\AppData\Local\Temp\ui_tooltip_background.rgba"#,
        ),
    ];
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    for p in paths {
        let mut variants: Vec<String> = vec![p.to_string()];
        variants.push(format!("{p}.BLP"));
        variants.push(format!("{p}.blp"));
        // also try with the blp hidden by the path separator case variants
        for v in &variants {
            let cv = std::ffi::CString::new(v.as_str()).unwrap();
            let (mut w, mut h) = (0u32, 0u32);
            let n = bn_texture(cworld, cv.as_ptr(), &mut w, &mut h, buf.as_mut_ptr(), buf.len() as u32);
            println!("{:<62} -> {} bytes ({}x{})", v, n, w, h);
            if n > 0 {
                for (dp, out) in DUMP {
                    if *dp == p {
                        std::fs::write(*out, &buf[..n as usize]).expect("write rgba dump");
                        println!("    dumped {n} bytes ({w}x{h}) -> {out}");
                    }
                }
                break;
            }
        }
        println!();
    }
    bn_shutdown(world);
}
