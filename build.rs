//! Ensambla el guest (`guest/guest.S`) a una imagen plana con binutils y
//! exporta la dirección de `branch_entry` para las pruebas.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn run(cmd: &mut Command) -> Vec<u8> {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("no se pudo ejecutar {cmd:?}: {e} (¿falta binutils?)"));
    assert!(
        out.status.success(),
        "{cmd:?} terminó con {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn tool(var: &str, default: &str) -> String {
    env::var(var).unwrap_or_else(|_| default.into())
}

fn main() {
    println!("cargo:rerun-if-changed=guest/guest.S");
    println!("cargo:rerun-if-changed=guest/guest.ld");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let obj = out.join("guest.o");
    let elf = out.join("guest.elf");
    let bin = out.join("guest.bin");
    run(Command::new(tool("CORTEX9_AS", "as"))
        .args(["--64", "-o"])
        .arg(&obj)
        .arg("guest/guest.S"));
    run(Command::new(tool("CORTEX9_LD", "ld"))
        .args([
            "-m",
            "elf_x86_64",
            "-nostdlib",
            "-static",
            "-T",
            "guest/guest.ld",
            "-o",
        ])
        .arg(&elf)
        .arg(&obj));
    run(Command::new(tool("CORTEX9_OBJCOPY", "objcopy"))
        .args(["-O", "binary"])
        .arg(&elf)
        .arg(&bin));

    let syms = String::from_utf8(run(Command::new(tool("CORTEX9_NM", "nm")).arg(&elf)))
        .expect("salida de nm en UTF-8");
    let addr = syms
        .lines()
        .find_map(|l| {
            let mut it = l.split_whitespace();
            let (a, _, name) = (it.next()?, it.next()?, it.next()?);
            (name == "branch_entry").then(|| a.to_string())
        })
        .expect("branch_entry no aparece en la tabla de símbolos");
    std::fs::write(
        out.join("guest_syms.rs"),
        format!(
            "/// Dirección de `branch_entry` en el guest (sacada de `nm`).\n\
             pub const BRANCH_ENTRY: u64 = 0x{addr};\n"
        ),
    )
    .expect("escribir guest_syms.rs");
}
