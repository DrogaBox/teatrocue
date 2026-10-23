use std::process::ExitCode;

use installer_tool::{find_key, load_key, parse_args, sign};

fn password() -> Result<zeroize::Zeroizing<String>, String> {
    if let Ok(p) = std::env::var("TEATROCUE_MASTER_PASSWORD") {
        return Ok(zeroize::Zeroizing::new(p));
    }
    rpassword::prompt_password("Contraseña maestra: ").map(zeroize::Zeroizing::new).map_err(|e| format!("no se pudo leer la contraseña: {e}"))
}

fn run() -> Result<(), String> {
    let args = parse_args(&std::env::args().skip(1).collect::<Vec<_>>())?;
    let path = find_key(args.key_path.as_deref())?;
    // Se valida el código antes de pedir la contraseña: un error de copia se ve enseguida.
    args.code.parse::<qlab_license::activation::ChallengeCode>().map_err(|e| e.to_string())?;
    let key = load_key(&path, &password()?)?;
    println!("{}", sign(&args, &key, qlab_license::now_unix())?);
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
