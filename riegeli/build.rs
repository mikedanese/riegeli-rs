use protobuf_codegen::CodeGen;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let proto_dir = manifest_dir.join("proto");

    std::env::set_current_dir(&proto_dir).unwrap();

    CodeGen::new()
        .inputs(["google/protobuf/descriptor.proto", "records_metadata.proto"])
        .include(".")
        .generate_and_compile()
        .unwrap();
}
