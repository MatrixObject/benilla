// FFI smoke test: directly call the benilla crate's C ABI functions.
// They are `pub` in the crate, so no DLL linking is needed.
use benilla::{BnWorld, bn_init, bn_shutdown, bn_num_archives, bn_load_tile};

fn c_char_ptr(s: &str) -> *const std::ffi::c_char {
    s.as_ptr() as *const std::ffi::c_char
}

fn main() {
    let data = "I:\\WOW\\World of Warcraft 1.12.1.5875 zhCN\\Data";
    let w: *mut BnWorld = bn_init(c_char_ptr(data));
    if w.is_null() {
        eprintln!("FAIL: bn_init returned null for {data}");
        std::process::exit(1);
    }
    let n = bn_num_archives(w as *const BnWorld);
    println!("OK: {n} MPQ archives registered");

    let chunks = bn_load_tile(w, c_char_ptr("Durotar"), 1, 1);
    println!("OK: Durotar_1_1.adt parsed {chunks} MCNK chunks");

    bn_shutdown(w);
    println!("FFI SMOKE TEST PASSED");
}
