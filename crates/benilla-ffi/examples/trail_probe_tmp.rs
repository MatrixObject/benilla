// Temp diagnostic: dump UI_MainMenu particle quads at one time, grouped by texture.
use benilla::*;
use std::io::Write;

fn main() {
    let data_dir = r#"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data"#;
    let cdir = std::ffi::CString::new(data_dir).unwrap();
    let world = bn_init(cdir.as_ptr());
    assert!(!world.is_null(), "bn_init");
    let cworld = world as *const BnWorld;
    let model = r"Interface\Glues\Models\UI_MainMenu\UI_MainMenu.m2";
    let cpath = std::ffi::CString::new(model).unwrap();
    let groups = model_particles::bn_model_particle_group_count(cworld, cpath.as_ptr());
    println!("groups={groups}");
    let mut names = Vec::new();
    for g in 0..groups {
        let mut out = vec![0i8; 512];
        let mut blend = 0u32;
        let n = model_particles::bn_model_particle_group(
            cworld,
            cpath.as_ptr(),
            g,
            &mut blend,
            out.as_mut_ptr(),
            out.len() as u32,
        );
        let s = String::from_utf8_lossy(
            &out[..n.saturating_sub(1) as usize]
                .iter()
                .map(|&c| c as u8)
                .collect::<Vec<u8>>(),
        )
        .to_string();
        names.push(s.rsplit(['\\', '/']).next().unwrap_or(&s).to_string());
    }
    let mut buf = vec![model_particles::BnParticleQuad::default(); 8192];
    for t in [1000.0f32, 1300.0, 1600.0] {
        let n = model_particles::bn_model_particles(
            cworld,
            cpath.as_ptr(),
            t,
            buf.as_mut_ptr(),
            buf.len() as u32,
        ) as usize;
        let n = n.min(buf.len());
        let fname = format!("D:/Unreal Projects/nezha/Saved/Shots/quads_{t:.0}.csv");
        let mut f = std::fs::File::create(&fname).unwrap();
        writeln!(f, "group,x,y,z,size,r,g,b,a").unwrap();
        for q in &buf[..n] {
            writeln!(
                f,
                "{},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4}",
                q.group, q.x, q.y, q.z, q.size, q.r, q.g, q.b, q.a
            )
            .unwrap();
        }
        println!("== t={t} n={n} -> {fname}");
        for g in 0..groups as usize {
            let mut c = [f32::MAX; 4];
            let mut d = [f32::MIN; 4];
            let mut cnt = 0;
            for q in buf[..n].iter().filter(|q| q.group as usize == g) {
                for (k, v) in [q.r, q.g, q.b, q.a].iter().enumerate() {
                    c[k] = c[k].min(*v);
                    d[k] = d[k].max(*v);
                }
                cnt += 1;
            }
            if cnt > 0 {
                println!(
                    "  g{g} {:<16} n={cnt:4} r={:.3}..{:.3} g={:.3}..{:.3} b={:.3}..{:.3} a={:.3}..{:.3}",
                    names[g], c[0], d[0], c[1], d[1], c[2], d[2], c[3], d[3]
                );
            }
        }
    }
}
