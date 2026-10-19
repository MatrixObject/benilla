// Print GlueXML.toc and any GlueXML lua that handles SET_GLUE_SCREEN,
// read straight out of the interface/patch MPQs, to see how a screen switch
// shows and hides frames.
fn main() {
    let data = r"I:\WOW\World of Warcraft 1.12.1.5875 zhCN\Data";
    let archives = [
        format!("{data}\\patch-2.MPQ"),
        format!("{data}\\patch.MPQ"),
        format!("{data}\\interface.MPQ"),
        format!("{data}\\base.MPQ"),
    ];
    let want: Vec<String> = std::env::args().skip(1).collect();
    let want: Vec<String> = if want.is_empty() {
        vec![
            "Interface\\GlueXML\\GlueXML.toc".into(),
            "Interface\\GlueXML\\GlueParent.lua".into(),
            "Interface\\GlueXML\\AccountLogin.lua".into(),
        ]
    } else {
        want
    };

    for path in &want {
        let mut found = false;
        for arch in &archives {
            let Ok(a) = benilla_mpq::Archive::open(arch) else {
                continue;
            };
            if !a.contains(path) {
                continue;
            }
            found = true;
            match a.read_file(path) {
                Ok(bytes) => {
                    println!("== {path} ({arch}, {} bytes)", bytes.len());
                    println!("{}", String::from_utf8_lossy(&bytes));
                }
                Err(e) => println!("== {path}: read error {e}"),
            }
            break;
        }
        if !found {
            println!("== {path}: not found in any archive");
        }
    }
}
