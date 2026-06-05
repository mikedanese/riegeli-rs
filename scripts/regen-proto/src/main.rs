//! Regenerates riegeli/src/proto_generated/ from riegeli/proto/.
//!
//! Uses protobuf_codegen::CodeGen — the same invocation the build script
//! used before the generated code was checked in — so this tool cannot
//! drift from what protoc actually emits. protobuf-codegen hard-asserts
//! that `protoc --version` matches its own version (34.1) and fails
//! loudly on any other. protoc is found via $PROTOC, else `protoc` on
//! PATH. Run via scripts/regen-proto.sh from the repository root.

fn main() {
    let repo_root = std::env::current_dir().expect("cwd");
    let proto_dir = repo_root.join("riegeli/proto");
    let out_dir = repo_root.join("riegeli/src/proto_generated");
    assert!(
        proto_dir.is_dir(),
        "riegeli/proto not found — run from the repository root"
    );

    // CodeGen::new() reads OUT_DIR eagerly (it is written for build
    // scripts); satisfy it with a throwaway value, then override.
    std::env::set_var("OUT_DIR", std::env::temp_dir());
    std::env::set_current_dir(&proto_dir).expect("chdir to riegeli/proto");

    protobuf_codegen::CodeGen::new()
        // Input order matters: protoc places the generated.rs module shim
        // next to the FIRST input's output, and lib.rs includes it from
        // proto_generated/google/protobuf/generated.rs.
        .inputs(["google/protobuf/descriptor.proto", "records_metadata.proto"])
        .include(".")
        .output_dir(&out_dir)
        .generate_and_compile()
        .expect("protoc codegen failed");

    // crate_mapping.txt is an input file protoc reads (empty here — no
    // cross-crate deps), not generated code; don't leave it in the tree.
    let _ = std::fs::remove_file(out_dir.join("crate_mapping.txt"));
    println!("regenerated {}", out_dir.display());
}
