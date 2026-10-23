//! Incrusta el icono de TeatroCue en el .exe de Windows (el Explorador y el acceso directo del Escritorio lo muestran).
//! Usa `windres` de mingw; si no está, el programa se compila igual, solo que sin icono.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let ico = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR")).join("../../assets/icons/teatrocue.ico");
    println!("cargo:rerun-if-changed={}", ico.display());
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let ico = ico.canonicalize().unwrap_or(ico);
    let rc = out.join("icon.rc");
    let obj = out.join("icon.o");
    let ico_text = ico.to_string_lossy().replace('\\', "/");
    if std::fs::write(&rc, format!("1 ICON \"{ico_text}\"\n")).is_err() {
        println!("cargo:warning=no se pudo escribir icon.rc: el .exe queda sin icono");
        return;
    }
    let windres = std::env::var("WINDRES").unwrap_or_else(|_| "x86_64-w64-mingw32-windres".into());
    match Command::new(&windres).arg("-i").arg(&rc).arg("-o").arg(&obj).status() {
        Ok(s) if s.success() => println!("cargo:rustc-link-arg-bins={}", obj.display()),
        _ => println!("cargo:warning=`{windres}` no está disponible: el .exe queda sin icono"),
    }
}
