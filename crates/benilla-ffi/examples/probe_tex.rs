// Probe: dump embedded M2 texture array
use benilla::*;

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
    let models = [
        r#"Creature\RidingHorse\RidingHorse.m2"#,
        r#"Creature\Wolf\Wolf.m2"#,
        r#"Interface\Buttons\TalkToMe.m2"#,
    ];
    let mut out = vec![0u8; 4096];
    let mut buf: Vec<u8> = vec![0u8; (4u32 * 1024 * 1024) as usize];
    for m in &models {
        let cm = std::ffi::CString::new(*m).unwrap();
        let count = bn_model_texture_count(cworld, cm.as_ptr());
        println!("{:<48} embedded_textures={count}", m);
        for i in 0..count.min(8) {
            let n = bn_model_texture_name(
                cworld,
                cm.as_ptr(),
                i as u32,
                out.as_mut_ptr(),
                out.len() as u32,
            );
            let name = if n > 1 {
                String::from_utf8_lossy(&out[..(n - 1) as usize]).to_string()
            } else {
                String::from("<empty>")
            };
            let mut w: u32 = 0;
            let mut h: u32 = 0;
            let cap = 4u32 * 1024 * 1024;
            let cbn = std::ffi::CString::new(name.clone()).unwrap_or_default();
            let mut w2: u32 = 0;
            let mut h2: u32 = 0;
            let tok = bn_texture(
                cworld,
                cbn.as_ptr(),
                &mut w2,
                &mut h2,
                buf.as_mut_ptr(),
                cap,
            );
            println!(
                "   [{i}] name={name:?}  via_name_bytes={tok}  ({}x{})",
                w2, h2
            );
            // Pixel stats: a bound-but-black texture shows up here as a near-0
            // mean with a sane alpha; alpha 0 everywhere means the mask kills it.
            if tok > 0 && w2 > 0 && h2 > 0 && (tok as usize) == (w2 * h2 * 4) as usize {
                let n = (w2 * h2) as usize;
                let (mut sr, mut sg, mut sb, mut sa, mut opaque) = (0u64, 0u64, 0u64, 0u64, 0u64);
                for px in buf[..n * 4].chunks_exact(4) {
                    sr += px[0] as u64;
                    sg += px[1] as u64;
                    sb += px[2] as u64;
                    sa += px[3] as u64;
                    if px[3] > 8 {
                        opaque += 1;
                    }
                }
                println!(
                    "        mean RGBA = ({:.0},{:.0},{:.0},{:.0})  opaque={:.1}%",
                    sr as f64 / n as f64,
                    sg as f64 / n as f64,
                    sb as f64 / n as f64,
                    sa as f64 / n as f64,
                    opaque as f64 * 100.0 / n as f64
                );
            }
        }
    }
    bn_shutdown(world);
}
