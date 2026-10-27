// One-off probe: is Interface\ChatFrame\ChatFrameBackground a white texture
// (so the chat panel's dark tint must come from the region's vertex colour) or
// a black translucent one? Prints the mean RGBA of the decoded BLP.
use benilla::*;

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
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    for p in [
        r#"Interface\ChatFrame\ChatFrameBackground"#,
        r#"Interface\ChatFrame\ChatFrameBackground.BLP"#,
    ] {
        let cv = std::ffi::CString::new(p).unwrap();
        let (mut w, mut h) = (0u32, 0u32);
        let n = bn_texture(
            cworld,
            cv.as_ptr(),
            &mut w,
            &mut h,
            buf.as_mut_ptr(),
            buf.len() as u32,
        );
        println!("{p} -> {n} bytes ({w}x{h})");
        if n > 0 {
            let px = &buf[..n as usize];
            let count = (n / 4).max(1) as u64;
            let (mut r, mut g, mut b, mut a) = (0u64, 0u64, 0u64, 0u64);
            for c in px.chunks_exact(4) {
                r += c[0] as u64;
                g += c[1] as u64;
                b += c[2] as u64;
                a += c[3] as u64;
            }
            println!(
                "  mean RGBA = ({}, {}, {}, {})",
                r / count,
                g / count,
                b / count,
                a / count
            );
            println!("  first px = {:?}", &px[..4.min(px.len())]);
            break;
        }
    }
    bn_shutdown(world);
}
