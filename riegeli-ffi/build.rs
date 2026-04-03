use blake2::{Blake2b512, Digest};
use flate2::read::GzDecoder;
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use tar::Archive;

// Pinned dependency versions and BLAKE2b-512 checksums of the .tar.gz downloads.
// To update: set expected_hash to "" and rebuild — the build will print the hash.
struct Dep {
    name: &'static str,
    url: String,
    blake2b_512: &'static str,
}

const RIEGELI_COMMIT: &str = "fc37128e99f75fe94e027ca4542d0c07a9a0b312";
const ABSEIL_TAG: &str = "20260107.0";
const HIGHWAYHASH_COMMIT: &str = "f8381f3331d9c56a9792f9b4a35f61c41108c39e";
const PROTOBUF_TAG: &str = "v33.2";
const BROTLI_TAG: &str = "v1.1.0";
const ZSTD_TAG: &str = "v1.5.6";
const SNAPPY_TAG: &str = "1.2.0";

fn deps() -> Vec<Dep> {
    vec![
        Dep {
            name: "riegeli",
            url: format!("https://github.com/google/riegeli/archive/{RIEGELI_COMMIT}.tar.gz"),
            blake2b_512: "ff55c5845742e1fc8a3885220bd27bd0a2958b1cb5867fff354b73e3fc0385f392f9d971ebad821ff27e85fed3732d1eaeac3b0db9a2111a2598a249b66b399c",
        },
        Dep {
            name: "abseil-cpp",
            url: format!(
                "https://github.com/abseil/abseil-cpp/archive/refs/tags/{ABSEIL_TAG}.tar.gz"
            ),
            blake2b_512: "7a7ca8c1e1cf9097f0c63f371c5b4470c6d8d36305f519c868d570e1d0e62c5e26382044451bb92227e7e68c3e7b40c69db1406ad59537be368e78173a90c341",
        },
        Dep {
            name: "highwayhash",
            url: format!(
                "https://github.com/google/highwayhash/archive/{HIGHWAYHASH_COMMIT}.tar.gz"
            ),
            blake2b_512: "543676811b396b22c681ba52742037ed35155ffb7b3e9d2f2ca63b9493f40c47bf78f3a227cddd0a0ae3ac47c0952ebac569ca44cc2b99311ad7e05e6a0c5fa6",
        },
        Dep {
            name: "protobuf",
            url: format!(
                "https://github.com/protocolbuffers/protobuf/archive/refs/tags/{PROTOBUF_TAG}.tar.gz"
            ),
            blake2b_512: "74e09134d5a8c524f1c2c11244d41c7b26a075872fbf99a95759878cf8e6a3effaf946b7e1994f3cbdfd8a465218bd1f226278e9e6ac6b1f9a1f7d54a0a366cf",
        },
        Dep {
            name: "brotli",
            url: format!("https://github.com/google/brotli/archive/refs/tags/{BROTLI_TAG}.tar.gz"),
            blake2b_512: "7ac767fd6dafaabfb4e3834d690f71abceb4d4e7f131849d6c328a04f3a16c54d0a9463a37f03663a4158c35e970a089512c8a5bc43eda79fb43c1f61223379e",
        },
        Dep {
            name: "zstd",
            url: format!("https://github.com/facebook/zstd/archive/refs/tags/{ZSTD_TAG}.tar.gz"),
            blake2b_512: "3cd4027441815915621d76d95fcd3822c6382930524ee78c40d30928ed1c3907c57a39c7331c94f7203207f97116283ec300aa9b392206ac0fc8d644ed0c87e3",
        },
        Dep {
            name: "snappy",
            url: format!("https://github.com/google/snappy/archive/refs/tags/{SNAPPY_TAG}.tar.gz"),
            blake2b_512: "327b60ea032ceb004c5f5e36a0013dc2a44258ec303d0701cf23446904b83e72a66b7e59866a7331c7751a08ce6ec6b871bc056efa6eeb86733d4640569d8072",
        },
    ]
}

fn download_and_extract(name: &str, url: &str, expected_hash: &str, deps_dir: &Path) -> PathBuf {
    let marker = deps_dir.join(format!(".{name}.done"));
    if marker.exists() {
        // Find the extracted directory
        for entry in fs::read_dir(deps_dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir()
                && entry.file_name().to_str().unwrap_or("").starts_with(name)
            {
                return entry.path();
            }
        }
    }

    eprintln!("Downloading {name} from {url}...");
    let resp = ureq::get(url)
        .call()
        .unwrap_or_else(|e| panic!("Failed to download {name}: {e}"));
    let mut reader = resp.into_reader();

    // Download to a temp file so we can hash before extracting
    let tarball = deps_dir.join(format!("{name}.tar.gz"));
    let mut buf = Vec::new();
    reader
        .read_to_end(&mut buf)
        .unwrap_or_else(|e| panic!("Failed to read {name} download: {e}"));

    // BLAKE2b-512 verification
    let mut hasher = Blake2b512::new();
    hasher.update(&buf);
    let hash = format!("{:x}", hasher.finalize());

    if expected_hash.is_empty() {
        println!("cargo:warning=BLAKE2b-512({name}): {hash}");
        println!("cargo:warning=  Pin this hash in build.rs to enable verification.");
    } else if hash != expected_hash {
        panic!(
            "Checksum mismatch for {name}!\n  expected: {expected_hash}\n  got:      {hash}\n  \
             The download may be corrupted or the upstream release may have changed."
        );
    }

    fs::write(&tarball, &buf).unwrap();

    // Extract
    let decoder = GzDecoder::new(&buf[..]);
    let mut archive = Archive::new(decoder);
    archive
        .unpack(deps_dir)
        .unwrap_or_else(|e| panic!("Failed to extract {name}: {e}"));

    fs::remove_file(&tarball).ok();
    fs::write(&marker, "").unwrap();

    // Find the extracted directory (GitHub tarballs extract to {repo}-{commit}/)
    for entry in fs::read_dir(deps_dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            let fname = entry.file_name();
            let fname = fname.to_str().unwrap_or("");
            if fname.starts_with(name) || fname.starts_with(&name.replace('-', "_")) {
                return entry.path();
            }
        }
    }
    panic!(
        "Could not find extracted directory for {name} in {}",
        deps_dir.display()
    );
}

fn collect_cc_files(dir: &Path, exclude_patterns: &[&str]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if !dir.exists() {
        return files;
    }
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("cc")
            || path.extension().and_then(|e| e.to_str()) == Some("cpp")
        {
            let fname = path.file_name().unwrap().to_str().unwrap();
            let excluded = exclude_patterns
                .iter()
                .any(|pat| matches_pattern(fname, pat));
            if !excluded {
                files.push(path);
            }
        }
    }
    files
}

fn collect_c_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if !dir.exists() {
        return files;
    }
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("c") {
            files.push(path);
        }
    }
    files
}

fn walkdir(dir: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    if !dir.exists() {
        return result;
    }
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            result.extend(walkdir(&path));
        } else {
            result.push(path);
        }
    }
    result
}

/// Simple glob pattern matching: supports `*` as wildcard for any chars.
fn matches_pattern(name: &str, pattern: &str) -> bool {
    if !pattern.contains('*') {
        return name == pattern;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 2 {
        // single wildcard: prefix*suffix
        let prefix = parts[0];
        let suffix = parts[1];
        return name.starts_with(prefix)
            && name.ends_with(suffix)
            && name.len() >= prefix.len() + suffix.len();
    }
    // fallback: check all parts appear in order
    let mut remaining = name;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !remaining.starts_with(part) {
                return false;
            }
            remaining = &remaining[part.len()..];
        } else if let Some(pos) = remaining.find(part) {
            remaining = &remaining[pos + part.len()..];
        } else {
            return false;
        }
    }
    true
}

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let deps_dir = out_dir.join("deps");
    fs::create_dir_all(&deps_dir).unwrap();

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // Download all dependencies with BLAKE2b-512 verification
    let all_deps = deps();
    let mut dep_paths: Vec<PathBuf> = Vec::new();
    for dep in &all_deps {
        dep_paths.push(download_and_extract(
            dep.name,
            &dep.url,
            dep.blake2b_512,
            &deps_dir,
        ));
    }
    let [
        ref riegeli_src,
        ref abseil_src,
        ref highwayhash_src,
        ref protobuf_src,
        ref brotli_src,
        ref zstd_src,
        ref snappy_src,
    ] = dep_paths[..]
    else {
        unreachable!()
    };

    // Generate snappy-stubs-public.h from .in template if needed
    generate_snappy_config(snappy_src);

    // Generate protobuf config if needed
    generate_protobuf_config(protobuf_src);

    // Common exclude patterns for C++ test/benchmark files
    let test_excludes: &[&str] = &[
        "*_test.cc",
        "*_testing.cc",
        "*_test_common.cc",
        "*_benchmark.cc",
        "*_bench.cc",
        "*_unittest.cc",
        "*_fuzzer.cc",
        "*_fuzz.cc",
        "*test_util*.cc",
        "*test_helpers*.cc",
        "*test_matchers.cc",
        "*mock*.cc",
        "*matcher*.cc",
        "*benchmark*.cc",
        "*tester*.cc",
        "*_win.cc",
        "*_win32.cc",
        "*_windows.cc",
    ];

    // =========================================================
    // Build brotli (C library)
    // =========================================================
    let mut brotli_build = cc::Build::new();
    brotli_build
        .include(brotli_src.join("c/include"))
        .files(collect_c_files(&brotli_src.join("c/common")))
        .files(collect_c_files(&brotli_src.join("c/enc")))
        .files(collect_c_files(&brotli_src.join("c/dec")));
    brotli_build.compile("brotli");

    // =========================================================
    // Build zstd (C library)
    // =========================================================
    let mut zstd_build = cc::Build::new();
    zstd_build
        .define("ZSTD_DISABLE_ASM", None)
        .include(zstd_src.join("lib"))
        .include(zstd_src.join("lib/common"))
        .files(collect_c_files(&zstd_src.join("lib/common")))
        .files(collect_c_files(&zstd_src.join("lib/compress")))
        .files(collect_c_files(&zstd_src.join("lib/decompress")));
    zstd_build.compile("zstd");

    // =========================================================
    // SIMD-specific builds (need different compiler flags)
    // =========================================================

    // Highwayhash SSE4.1 / AVX2
    if cfg!(target_arch = "x86_64") || cfg!(target_arch = "x86") {
        let mut hh_sse41 = cc::Build::new();
        hh_sse41
            .cpp(true)
            .std("c++17")
            .include(highwayhash_src)
            .flag("-msse4.1")
            .file(highwayhash_src.join("highwayhash/hh_sse41.cc"));
        hh_sse41.compile("highwayhash_sse41");

        let mut hh_avx2 = cc::Build::new();
        hh_avx2
            .cpp(true)
            .std("c++17")
            .include(highwayhash_src)
            .flag("-mavx2")
            .file(highwayhash_src.join("highwayhash/hh_avx2.cc"));
        hh_avx2.compile("highwayhash_avx2");
    }

    // utf8_range SIMD variants
    let utf8_dir = protobuf_src.join("third_party/utf8_range");
    if utf8_dir.exists() {
        let sse_file = utf8_dir.join("range-sse.c");
        if sse_file.exists() {
            let mut sse_build = cc::Build::new();
            sse_build
                .define("NDEBUG", None)
                .include(&utf8_dir)
                .flag("-msse4.1")
                .file(&sse_file);
            sse_build.compile("utf8_range_sse");
        }
        let avx_file = utf8_dir.join("range-avx2.c");
        if avx_file.exists() {
            let mut avx_build = cc::Build::new();
            avx_build
                .define("NDEBUG", None)
                .include(&utf8_dir)
                .flag("-mavx2")
                .file(&avx_file);
            avx_build.compile("utf8_range_avx2");
        }
    }

    // =========================================================
    // Shared include paths for all C++ builds
    // =========================================================
    let cpp_includes: Vec<PathBuf> = vec![
        riegeli_src.clone(),
        abseil_src.clone(),
        highwayhash_src.clone(),
        protobuf_src.join("src"),
        brotli_src.join("c/include"),
        zstd_src.join("lib"),
        snappy_src.clone(),
        protobuf_src.join("third_party/utf8_range"),
        manifest_dir.clone(),
        manifest_dir.join("cpp/generated"),
    ];

    // =========================================================
    // Build 1: deps_cpp — all third-party C++ (abseil, protobuf, snappy,
    // highwayhash, riegeli C++ source, pre-generated pb). This only
    // recompiles when dependency sources change (i.e. version bump),
    // NOT when wrapper.h/wrapper.cc/lib.rs change.
    //
    // We skip this entirely if the static library already exists, since
    // these sources never change between wrapper edits.
    // =========================================================
    let deps_lib = out_dir.join("libdeps_cpp.a");
    let deps_needed = !deps_lib.exists();

    if !deps_needed {
        // Still emit the link search path so the linker finds it
        println!(
            "cargo:rustc-link-search=native={}",
            out_dir.to_str().unwrap()
        );
    }

    if deps_needed {
        let mut deps_build = cc::Build::new();
        deps_build
            .cpp(true)
            .std("c++17")
            .define("NDEBUG", None)
            .define("HAVE_PTHREAD", None);
        for inc in &cpp_includes {
            deps_build.include(inc);
        }

        // --- snappy ---
        deps_build
            .file(snappy_src.join("snappy.cc"))
            .file(snappy_src.join("snappy-sinksource.cc"))
            .file(snappy_src.join("snappy-stubs-internal.cc"));

        // --- highwayhash (portable) ---
        deps_build
            .file(highwayhash_src.join("highwayhash/arch_specific.cc"))
            .file(highwayhash_src.join("highwayhash/instruction_sets.cc"))
            .file(highwayhash_src.join("highwayhash/os_specific.cc"))
            .file(highwayhash_src.join("highwayhash/hh_portable.cc"));

        // --- abseil ---
        {
            let abseil_subdirs = [
                "absl/base",
                "absl/base/internal",
                "absl/container/internal",
                "absl/crc",
                "absl/crc/internal",
                "absl/debugging",
                "absl/debugging/internal",
                "absl/flags",
                "absl/flags/internal",
                "absl/hash",
                "absl/hash/internal",
                "absl/log",
                "absl/log/internal",
                "absl/numeric",
                "absl/profiling/internal",
                "absl/random",
                "absl/random/internal",
                "absl/status",
                "absl/status/internal",
                "absl/strings",
                "absl/strings/internal",
                "absl/strings/internal/str_format",
                "absl/synchronization",
                "absl/synchronization/internal",
                "absl/time",
                "absl/time/internal/cctz/src",
                "absl/types",
            ];
            for subdir in &abseil_subdirs {
                deps_build.files(collect_cc_files(&abseil_src.join(subdir), test_excludes));
            }
        }

        // --- protobuf ---
        {
            let proto_subdirs = [
                "google/protobuf",
                "google/protobuf/io",
                "google/protobuf/stubs",
                "google/protobuf/json",
                "google/protobuf/json/internal",
            ];
            let proto_excludes: Vec<&str> = {
                let mut v: Vec<&str> = test_excludes.to_vec();
                v.extend_from_slice(&["io_win32.cc", "*_win.cc"]);
                v
            };
            let src_dir = protobuf_src.join("src");
            for subdir in &proto_subdirs {
                deps_build.files(collect_cc_files(&src_dir.join(subdir), &proto_excludes));
            }
        }

        // --- utf8_range (non-SIMD C files, compiled as C++ here) ---
        let utf8_dir = protobuf_src.join("third_party/utf8_range");
        if utf8_dir.exists() {
            for entry in fs::read_dir(&utf8_dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("c") {
                    let fname = path.file_name().unwrap().to_str().unwrap();
                    if fname == "main.c" || fname.contains("sse") || fname.contains("avx") {
                        continue;
                    }
                    deps_build.file(&path);
                }
            }
        }

        // --- riegeli C++ source ---
        deps_build.files(collect_cc_files(
            &riegeli_src.join("riegeli/base"),
            test_excludes,
        ));

        let bytes_excludes: Vec<&str> = {
            let mut v: Vec<&str> = test_excludes.to_vec();
            v.extend_from_slice(&[
                "fd_reader.cc",
                "fd_writer.cc",
                "fd_mmap_reader.cc",
                "fd_internal.cc",
                "fd_dependency.cc",
                "cfile_reader.cc",
                "cfile_writer.cc",
                "cfile_internal.cc",
                "cfile_dependency.cc",
                "istream_reader.cc",
                "ostream_writer.cc",
                "reader_istream.cc",
                "writer_ostream.cc",
                "std_io.cc",
                "file_mode_string.cc",
                "reader_factory.cc",
            ]);
            v
        };
        deps_build.files(collect_cc_files(
            &riegeli_src.join("riegeli/bytes"),
            &bytes_excludes,
        ));

        deps_build.files(collect_cc_files(
            &riegeli_src.join("riegeli/chunk_encoding"),
            test_excludes,
        ));
        deps_build.files(collect_cc_files(
            &riegeli_src.join("riegeli/records"),
            test_excludes,
        ));

        let messages_excludes: Vec<&str> = {
            let mut v: Vec<&str> = test_excludes.to_vec();
            v.extend_from_slice(&[
                "text_parse_message.cc",
                "text_print_message.cc",
                "field_handlers.cc",
                // These headers use repeated default template arguments that GCC
                // rejects (CWG 2082). They are not needed by our FFI bridge.
                "serialized_message_writer.cc",
                "serialized_message_backward_writer.cc",
                "serialized_message_assembler.cc",
            ]);
            v
        };
        deps_build.files(collect_cc_files(
            &riegeli_src.join("riegeli/messages"),
            &messages_excludes,
        ));
        deps_build.files(collect_cc_files(
            &riegeli_src.join("riegeli/varint"),
            test_excludes,
        ));
        deps_build.files(collect_cc_files(
            &riegeli_src.join("riegeli/ordered_varint"),
            test_excludes,
        ));
        deps_build.files(collect_cc_files(
            &riegeli_src.join("riegeli/brotli"),
            test_excludes,
        ));
        deps_build.files(collect_cc_files(
            &riegeli_src.join("riegeli/zstd"),
            test_excludes,
        ));
        deps_build.files(collect_cc_files(
            &riegeli_src.join("riegeli/snappy"),
            test_excludes,
        ));
        deps_build.files(collect_cc_files(
            &riegeli_src.join("riegeli/digests"),
            test_excludes,
        ));
        deps_build.files(collect_cc_files(
            &riegeli_src.join("riegeli/endian"),
            test_excludes,
        ));

        // --- pre-generated protobuf code ---
        let gen_dir = manifest_dir.join("cpp/generated");
        for path in walkdir(&gen_dir) {
            if path.extension().and_then(|e| e.to_str()) == Some("cc") {
                deps_build.file(&path);
            }
        }

        deps_build.compile("deps_cpp");
    } // if deps_needed

    // =========================================================
    // Build 2: bridge — only the cxx bridge + our wrapper.
    // This is the only part that recompiles when the FFI interface changes.
    // =========================================================
    let mut bridge_build = cxx_build::bridge("src/lib.rs");
    bridge_build
        .cpp(true)
        .std("c++17")
        .define("NDEBUG", None)
        .define("HAVE_PTHREAD", None);
    for inc in &cpp_includes {
        bridge_build.include(inc);
    }
    bridge_build.file(manifest_dir.join("cpp/wrapper.cc"));
    bridge_build.compile("riegeli_bridge");

    // Link everything
    println!("cargo:rustc-link-lib=static=brotli");
    println!("cargo:rustc-link-lib=static=zstd");
    if cfg!(target_arch = "x86_64") || cfg!(target_arch = "x86") {
        println!("cargo:rustc-link-lib=static=highwayhash_sse41");
        println!("cargo:rustc-link-lib=static=highwayhash_avx2");
        println!("cargo:rustc-link-lib=static=utf8_range_sse");
        println!("cargo:rustc-link-lib=static=utf8_range_avx2");
    }
    println!("cargo:rustc-link-lib=static=deps_cpp");
    println!("cargo:rustc-link-lib=static=riegeli_bridge");
    println!("cargo:rustc-link-lib=stdc++");
    println!("cargo:rustc-link-lib=pthread");

    // Rerun if wrapper changes
    println!("cargo:rerun-if-changed=cpp/wrapper.h");
    println!("cargo:rerun-if-changed=cpp/wrapper.cc");
    println!("cargo:rerun-if-changed=src/lib.rs");
}

fn generate_snappy_config(snappy_src: &Path) {
    // snappy needs snappy-stubs-public.h generated from the .in template
    let stubs_in = snappy_src.join("snappy-stubs-public.h.in");
    let stubs_out = snappy_src.join("snappy-stubs-public.h");
    if stubs_out.exists() {
        return;
    }
    if stubs_in.exists() {
        let content = fs::read_to_string(&stubs_in).unwrap();
        // Replace cmake variables with appropriate values
        let content = content
            .replace("${HAVE_SYS_UIO_H_01}", "1")
            .replace("${PROJECT_VERSION_MAJOR}", "1")
            .replace("${PROJECT_VERSION_MINOR}", "2")
            .replace("${PROJECT_VERSION_PATCH}", "0");
        fs::write(&stubs_out, content).unwrap();
    }

    // Also generate config.h
    let config_h = snappy_src.join("config.h");
    if !config_h.exists() {
        fs::write(
            &config_h,
            r#"
#ifndef THIRD_PARTY_SNAPPY_CONFIG_H_
#define THIRD_PARTY_SNAPPY_CONFIG_H_

#define HAVE_SYS_UIO_H 1
#define HAVE_SYS_MMAN_H 1
#define HAVE_UNISTD_H 1

#ifdef __has_builtin
#  if __has_builtin(__builtin_expect)
#    define SNAPPY_HAVE_BUILTIN_EXPECT 1
#  endif
#  if __has_builtin(__builtin_ctz)
#    define SNAPPY_HAVE_BUILTIN_CTZ 1
#  endif
#endif

#endif  // THIRD_PARTY_SNAPPY_CONFIG_H_
"#,
        )
        .unwrap();
    }
}

fn generate_protobuf_config(protobuf_src: &Path) {
    // Some protobuf versions need a config.h or similar generated header
    // For v29.x, check if we need anything
    let src_dir = protobuf_src.join("src");
    if !src_dir.exists() {}
}

// build_abseil and build_protobuf_lite are merged into the main riegeli_build above.
