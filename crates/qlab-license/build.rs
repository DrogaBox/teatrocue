//! Incrusta la clave pública del desarrollador en el binario.
//!
//! La clave se toma de `TEATROCUE_PUBLIC_KEY_FILE` (un archivo con 64 caracteres hexadecimales; lo genera
//! `scripts/build_usb_installer.sh`). Sin ella se compila una clave vacía: el crate funciona (las pruebas usan sus
//! propias claves) pero ninguna licencia real se acepta.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=TEATROCUE_PUBLIC_KEY_FILE");
    // Fecha de compilación: cota inferior del reloj (un reloj anterior a ella es falso). El script del pendrive la fija.
    println!("cargo:rerun-if-env-changed=TEATROCUE_BUILD_UNIX");
    let build_unix = std::env::var("TEATROCUE_BUILD_UNIX").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or_else(|| {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
    });
    println!("cargo:rustc-env=TEATROCUE_BUILD_UNIX_VALUE={build_unix}");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("public_key.hex");
    let hex = match std::env::var("TEATROCUE_PUBLIC_KEY_FILE") {
        Ok(path) if !path.is_empty() => {
            println!("cargo:rerun-if-changed={path}");
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("TEATROCUE_PUBLIC_KEY_FILE={path}: {e}"));
            let hex: String = text.split_whitespace().collect();
            assert!(hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()), "la clave pública debe ser 64 caracteres hexadecimales");
            hex.to_ascii_lowercase()
        }
        _ => String::new(),
    };
    std::fs::write(out, hex).expect("escribir la clave pública");
}
