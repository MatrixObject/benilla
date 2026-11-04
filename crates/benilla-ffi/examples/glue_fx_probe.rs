// What the login backdrop's M2 actually authors, and what the FFI answers for it.
//
// The reference draws the glue booth's particles from the MDX's emitter tables;
// this probe lists those tables (blob, ribbon, recursion, tail) next to the quad
// list `bn_model_particles` answers, so a visual gap traces to a missing table.
use benilla::*;
use benilla_formats::{parse_m2_particle_emitters, parse_m2_ribbon_emitters};
use benilla_mpq::Archive;

fn base(p: &Option<String>) -> String {
    p.as_deref()
        .map(|s| s.rsplit(['\\', '/']).next().unwrap_or(s).to_string())
        .unwrap_or_else(|| "<none>".into())
}

fn main() {
    let data_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| r#"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data"#.to_string());
    let token = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "UI_MainMenu".to_string());
    let rel = format!("Interface\\Glues\\Models\\{token}\\{token}.mdx");

    let mut bytes: Option<Vec<u8>> = None;
    let mut found = rel.clone();
    let mut from = String::new();
    for e in std::fs::read_dir(&data_dir).expect("read Data") {
        let Ok(e) = e else { continue };
        let mpq = e.path();
        if !mpq.to_string_lossy().to_lowercase().ends_with(".mpq") {
            continue;
        }
        let Ok(a) = Archive::open(&mpq) else {
            continue;
        };
        for cand in [&rel, &rel.replace(".mdx", ".m2")] {
            if a.contains(cand) {
                if let Ok(b) = a.read_file(cand) {
                    bytes = Some(b);
                    found = cand.to_string();
                    from = format!("{cand} in {}", mpq.display());
                }
                break;
            }
        }
        if bytes.is_some() {
            break;
        }
    }
    let Some(bytes) = bytes else {
        eprintln!("no {token} under {data_dir}");
        std::process::exit(1);
    };
    println!("== file: {from} ({} bytes)", bytes.len());

    let blobs = parse_m2_particle_emitters(&bytes).expect("blob parse");
    println!("== blob emitters: {}", blobs.len());
    let mut authored_steady = 0u32;
    for (i, d) in blobs.iter().enumerate() {
        let now = d.params.sample(Some(0), 0.0, 0.0);
        let rate = d.timing.rate(Some(0), 0.0, 0.0);
        let steady = (rate * now.lifespan.max(0.01)).ceil() as u32;
        authored_steady += steady;
        println!(
            "[{i:2}] tex={:<20} {:?} lit={} ht={} tail={:.2} tile={}x{} {:?} bone={} \
             drag={:.2} spin={:.2} twk(s={:.1} p={:.2} {:.2}-{:.2}) mspace={} \
             rate={:.1} life={:.2} steady={steady} area={:.2}/{:.2} vr={:.2} hr={:.2} \
             spd={:.2} grav={:.2} geom={} recur={}",
            base(&d.texture),
            d.blend,
            d.lit,
            d.head_tail,
            d.tail_time,
            d.tile_rows,
            d.tile_cols,
            d.shape,
            d.bone,
            d.drag,
            d.spin,
            d.twinkle_speed,
            d.twinkle_percent,
            d.twinkle_min,
            d.twinkle_max,
            d.model_space(),
            rate,
            now.lifespan,
            now.area_length,
            now.area_width,
            now.vertical_range,
            now.horizontal_range,
            now.emission_speed,
            now.gravity,
            base(&d.geometry_model),
            base(&d.recursion_model),
        );
    }
    println!("== authored steady-state particles: {authored_steady}");

    let ribbons = parse_m2_ribbon_emitters(&bytes).unwrap_or_default();
    println!("== ribbon emitters: {}", ribbons.len());
    for (i, r) in ribbons.iter().enumerate() {
        println!(
            "[{i:2}] tex={:<20} {:?} bone={} edges/s={:.1} life={:.2} above={:.2} below={:.2} \
             tile={}x{} grav={:.2}",
            base(&r.texture),
            r.blend,
            r.bone,
            r.edges_per_second,
            r.edge_lifetime,
            r.height_above.sample_ms(0.0),
            r.height_below.sample_ms(0.0),
            r.tile_rows,
            r.tile_cols,
            r.gravity,
        );
    }

    let cdir = std::ffi::CString::new(data_dir).unwrap();
    let world = bn_init(cdir.as_ptr());
    assert!(!world.is_null(), "bn_init");
    let cworld = world as *const BnWorld;
    let cpath = std::ffi::CString::new(found).unwrap();

    let groups = model_particles::bn_model_particle_group_count(cworld, cpath.as_ptr());
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
        names.push(format!(
            "g{g}={} blend{blend}",
            s.rsplit(['\\', '/']).next().unwrap_or(&s)
        ));
    }
    println!("== ffi groups ({groups}): {}", names.join(" "));

    let buf = vec![model_particles::BnParticleQuad::default(); 8192];
    let mut buf = buf;
    for t in [200.0f32, 1000.0, 3000.0, 6000.0] {
        let n = model_particles::bn_model_particles(
            cworld,
            cpath.as_ptr(),
            t,
            buf.as_mut_ptr(),
            buf.len() as u32,
        );
        let drawn = n.min(buf.len() as u32) as usize;
        let mut per = vec![0u32; groups.max(1) as usize];
        let (mut smin, mut smax) = (f32::MAX, 0.0f32);
        let (mut amin, mut amax) = (f32::MAX, 0.0f32);
        let mut bb = [[f32::MAX, f32::MIN]; 3];
        for q in &buf[..drawn] {
            per[q.group as usize] += 1;
            smin = smin.min(q.size);
            smax = smax.max(q.size);
            amin = amin.min(q.a);
            amax = amax.max(q.a);
            for (k, v) in [q.x, q.y, q.z].iter().enumerate() {
                bb[k][0] = bb[k][0].min(*v);
                bb[k][1] = bb[k][1].max(*v);
            }
        }
        println!(
            "== t={t:.0}: answer={n} drawn={drawn} per={:?} size={smin:.1}..{smax:.1} \
             alpha={amin:.2}..{amax:.2} x[{:.0},{:.0}] y[{:.0},{:.0}] z[{:.0},{:.0}]",
            per, bb[0][0], bb[0][1], bb[1][0], bb[1][1], bb[2][0], bb[2][1],
        );
    }
}
