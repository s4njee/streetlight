fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-link-lib=va");
    println!("cargo:rustc-link-lib=va-drm");
    println!("cargo:rustc-link-lib=EGL");
    println!("cargo:rustc-link-lib=gbm");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindgen::Builder::default()
        .header_contents("va_wrap.h", "#include <va/va.h>\n#include <va/va_drmcommon.h>\n#include <va/va_vpp.h>\n")
        .allowlist_function("va(CreateSurfaces|DestroySurfaces|CreateConfig|DestroyConfig|CreateContext|DestroyContext|CreateBuffer|DestroyBuffer|BeginPicture|RenderPicture|EndPicture|SyncSurface|ErrorStr|ExportSurfaceHandle|QueryVendorString)")
        .allowlist_type("VA.*|_VA.*")
        .allowlist_var("VA_.*")
        .derive_default(true)
        .layout_tests(false)
        .generate()
        .expect("bindgen libva")
        .write_to_file(out.join("va.rs"))
        .unwrap();
}
