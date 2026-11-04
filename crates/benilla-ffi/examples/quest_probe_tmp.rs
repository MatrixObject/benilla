// Scratch: load the stock UI headlessly and inspect the QuestFrame text colour.
fn eval(code: &str) -> String {
    let c = std::ffi::CString::new(code).unwrap();
    let mut buf = vec![0u8; 4096];
    let n = benilla::bn_ui_eval(c.as_ptr(), buf.as_mut_ptr() as *mut i8, buf.len() as u32);
    String::from_utf8_lossy(&buf[..n as usize]).into_owned()
}

fn main() {
    let dir = std::ffi::CString::new("I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data").unwrap();
    let w = benilla::bn_init(dir.as_ptr());
    assert!(!w.is_null(), "bn_init failed");
    let frames = benilla::bn_ui_load(w);
    println!("frames = {frames}");

    println!(
        "table = {}",
        eval("return type(MATERIAL_TEXT_COLOR_TABLE)")
    );
    println!(
        "parchment = {}",
        eval("local t = MATERIAL_TEXT_COLOR_TABLE and MATERIAL_TEXT_COLOR_TABLE['Parchment']; \
              if not t then return 'NIL' end; return table.concat({t[1],t[2],t[3]}, ',')")
    );
    println!(
        "questDesc exists = {}",
        eval("local q = QuestDescription; if not q then return 'NIL' end; return q:GetObjectType()")
    );
    println!(
        "set/dump colour = {}",
        eval("QuestDescription:SetText('The quick brown fox jumps over the lazy dog. 0123456789'); \
              QuestFrame_SetTextColor(QuestDescription, 'Parchment'); \
              return 'ok'")
    );
    println!(
        "get = {}",
        eval("return QuestDescription:GetTextColor()")
    );
    println!(
        "grad = {}",
        eval("QuestDescription:SetAlphaGradient(12, 30); return 'set'")
    );

    println!("expr call = {}", eval("string.len('abc')"));
    println!("let+return = {}", eval("local s=1 return 's='..s"));
    println!(
        "iife = {}",
        eval("(function() local s='q' local t='r' return 'i='..s..t end)()")
    );

    let mut buf = vec![0u8; 16 * 1024 * 1024];
    let n = benilla::bn_ui_dump(buf.as_mut_ptr() as *mut i8, buf.len() as u32);
    let text = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
    for line in text.lines() {
        if line.split('\t').nth(2) == Some("QuestDescription") {
            println!("ROW {line}");
        }
    }
    benilla::bn_shutdown(w);
}
