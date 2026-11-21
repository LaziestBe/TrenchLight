//! Live negative control for the Horizon installer.
//!
//! The unit tests build their own archives, which proves the refusal logic but
//! not that it holds for the real release: a 74 MB deflate stream with a
//! wrapped folder, DLLs and a wasm blob. This test takes a **real** archive,
//! flips one byte, and checks that nothing is installed.
//!
//! It is ignored by default because it needs a file on disk:
//!
//! ```text
//! set MCL_TEST_ARCHIVE=%TEMP%\horizon-windows-x64.zip
//! cargo test -p mcl --test horizon_negative_control -- --ignored --nocapture
//! ```
//!
//! The archive is copied before it is corrupted, so the original stays usable.

use std::path::Path;
use std::sync::atomic::AtomicBool;

#[test]
#[ignore = "needs MCL_TEST_ARCHIVE pointing at a real release archive"]
fn a_corrupted_real_archive_is_refused() {
    let Ok(source) = std::env::var("MCL_TEST_ARCHIVE") else {
        panic!("set MCL_TEST_ARCHIVE to a real horizon-windows-x64.zip");
    };
    let source = Path::new(&source);
    assert!(source.is_file(), "no archive at {}", source.display());

    let dir = std::env::temp_dir().join("mcl-horizon-negative-control");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // The published checksum of the untouched copy.
    let expected = mcl::horizon::sha256_file(source).unwrap();
    println!("archive:   {}", source.display());
    println!("sha256:    {expected}");

    // A real copy with one bit flipped in the middle of the compressed stream.
    let broken = dir.join("horizon-windows-x64.zip");
    let mut bytes = std::fs::read(source).unwrap();
    let at = bytes.len() / 2;
    bytes[at] ^= 0xFF;
    std::fs::write(&broken, &bytes).unwrap();
    let actual = mcl::horizon::sha256_file(&broken).unwrap();
    println!("corrupted: {actual} (byte {at} flipped)");
    assert_ne!(actual, expected, "the corruption must change the hash");

    let dest = dir.join("builds").join("nightly-corrupted");
    let cancel = AtomicBool::new(false);
    let error = mcl::horizon::unpack_verified(&broken, &expected, &dest, &cancel)
        .expect_err("a corrupted archive must never install");
    println!("refused:   {error:#}");

    assert!(
        !dest.exists(),
        "a refused archive must leave no build folder at {}",
        dest.display()
    );
    assert!(
        !dest.with_extension("tmp-install").exists(),
        "no half-unpacked folder may survive a refusal"
    );

    // And the untouched archive with the same hash does install, so the refusal
    // was the corruption and not a broken unpacker.
    let good_dest = dir.join("builds").join("nightly-good");
    mcl::horizon::unpack_verified(source, &expected, &good_dest, &cancel)
        .expect("the untouched archive must install");
    assert!(
        good_dest.join("horizon-client.exe").is_file(),
        "the game executable must land directly in the build folder"
    );
    println!("installed: {}", good_dest.display());

    let _ = std::fs::remove_dir_all(&dir);
}
