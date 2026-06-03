use std::env;
use std::path::Path;

fn main() {
    let target = env::var("TARGET").unwrap_or_default();
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");
    
    // Emit check-cfg for has_c_avx2 to suppress warnings
    println!("cargo::rustc-check-cfg=cfg(has_c_avx2)");
    
    // Skip C AVX2 compilation when cross-compiling under QEMU
    if env::var("RINHA_SKIP_C_AVX2").is_ok() {
        println!("cargo:warning=Skipping C AVX2 compilation (RINHA_SKIP_C_AVX2 set)");
        return;
    }
    
    // Auto-detect x86_64 with AVX2 support
    if target.contains("x86_64") {
        let c_file = if Path::new("native/distance_avx2.c").exists() {
            "native/distance_avx2.c"
        } else if Path::new("../native/distance_avx2.c").exists() {
            "../native/distance_avx2.c"
        } else {
            println!("cargo:warning=C AVX2 file not found, using scalar fallback");
            return;
        };
        
        // Use cc crate for reliable compilation
        let mut build = cc::Build::new();
        build.file(c_file)
            .out_dir(&out_dir)
            .flag("-mavx2")
            .flag("-mfma")
            .flag("-O3");
        build.compile("distance_avx2");
        
        println!("cargo:rustc-link-lib=static=distance_avx2");
        println!("cargo:rustc-link-search=native={}", out_dir);
        println!("cargo:rustc-cfg=has_c_avx2");
        println!("cargo:warning=C AVX2 distance compiled successfully");
    }
    
    println!("cargo:rerun-if-changed={}", 
        if Path::new("native/distance_avx2.c").exists() {
            "native/distance_avx2.c"
        } else {
            "../native/distance_avx2.c"
        });
}
