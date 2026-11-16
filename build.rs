use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/vtdrop.rc");
    println!("cargo:rerun-if-changed=assets/vtdrop-gnu.rc");
    println!("cargo:rerun-if-changed=assets/vtdrop.manifest");
    println!("cargo:rerun-if-changed=assets/vtdrop.ico");

    // embed-resource elige su backend (RC.EXE o windres+ar) según el *host*,
    // porque los build scripts se compilan para el host. En un runner MSVC
    // que compila para `*-windows-gnu` eso genera un `.lib` de MSVC que el
    // `ld` de MinGW rechaza. Para el target GNU compilamos con windres aquí.
    //
    // MinGW-W64 14 enlaza además su propio manifiesto `longPathAware`, así que
    // en GNU solo incrustamos icono + manifiesto propio como RT_MANIFEST/2,
    // que la app activa en tiempo de ejecución (src/ui/styles.rs). El DPI se
    // fija con SetProcessDpiAwarenessContext antes de crear la ventana.
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_env == "gnu" {
        compile_with_windres();
    } else {
        embed_resource::compile("assets/vtdrop.rc", embed_resource::NONE)
            .manifest_required()
            .expect("no se pudieron incrustar los recursos de Windows");
    }
}

fn compile_with_windres() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let object = out_dir.join("vtdrop-resource.o");
    let windres = find_windres();
    let machine = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86") => "pe-i386",
        Ok("aarch64") => "pe-aarch64-little",
        _ => "pe-x86-64",
    };

    let status = Command::new(&windres)
        .arg("--input")
        .arg("assets/vtdrop-gnu.rc")
        .arg("--output")
        .arg(&object)
        .arg("--output-format=coff")
        .arg("--target")
        .arg(machine)
        .args(["-c", "65001"])
        .arg("--include-dir")
        .arg(&out_dir)
        .status()
        .unwrap_or_else(|error| panic!("no se pudo ejecutar {windres}: {error}"));

    assert!(
        status.success(),
        "{windres} no pudo compilar assets/vtdrop-gnu.rc"
    );
    println!("cargo:rustc-link-arg-bins={}", object.display());
}

fn find_windres() -> String {
    for candidate in ["windres", "x86_64-w64-mingw32-windres"] {
        if Command::new(candidate)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return candidate.to_owned();
        }
    }
    panic!(
        "windres no está en el PATH: instala MinGW-W64 (GCC + binutils) y añade su bin al PATH, \
         o compila con la toolchain MSVC"
    );
}
