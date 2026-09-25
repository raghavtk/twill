fn main() {
    println!("cargo:rerun-if-changed=assets/JSONC.sublime-syntax");
    let mut builder = two_face::syntax::extra_newlines().into_builder();
    let jsonc = syntect::parsing::SyntaxDefinition::load_from_str(
        include_str!("assets/JSONC.sublime-syntax"),
        true,
        None,
    )
    .expect("bundled JSONC grammar must parse");
    builder.add(jsonc);
    let output =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("syntaxes.packdump");
    syntect::dumps::dump_to_uncompressed_file(&builder.build(), output).expect("write syntax pack");
}
