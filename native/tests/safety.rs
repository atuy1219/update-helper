use sha2::{Digest, Sha256};
use std::fs;
use std::io::Cursor;
use tb376_ota_helper_native::{
    ensure_write_target, inspect_image, patch_image, partition_name, stream_copy_exact,
    stream_hash, validate_partition_size, Region,
};

#[test]
fn row_to_prc_changes_exactly_nine_bytes_and_preserves_size() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("stock.img");
    let output = temp.path().join("prc.img");
    let bytes = vendor_boot(&[
        ("qcom,other", None),
        ("qcom,tuna", Some(Region::Row)),
        ("qcom,tuna", Some(Region::Row)),
        ("qcom,tunap", Some(Region::Row)),
    ]);
    fs::write(&input, &bytes).unwrap();
    let report = patch_image(&input, &output).unwrap();
    assert_eq!(report.changed_byte_count, 9);
    assert_eq!(report.changed_offsets.len(), 9);
    assert_eq!(report.input_size, report.output_size);
    assert_eq!(inspect_image(&output).unwrap().region, Region::Prc);
    let patched = fs::read(output).unwrap();
    assert_eq!(patched.len(), bytes.len());
    assert!(patched.windows(17).any(|w| w == b"unrelated_ROW_txt"));
}

#[test]
fn rejects_supported_fdt_count_other_than_three() {
    assert_invalid(&[
        ("qcom,tuna", Some(Region::Row)),
        ("qcom,tunap", Some(Region::Row)),
    ]);
}

#[test]
fn rejects_wrong_tuna_tunap_distribution() {
    assert_invalid(&[
        ("qcom,tuna", Some(Region::Row)),
        ("qcom,tunap", Some(Region::Row)),
        ("qcom,tunap", Some(Region::Row)),
    ]);
}

#[test]
fn already_prc_is_distinct_success_without_changes() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("prc-stock.img");
    let output = temp.path().join("prc-copy.img");
    fs::write(
        &input,
        vendor_boot(&[
            ("qcom,tuna", Some(Region::Prc)),
            ("qcom,tuna", Some(Region::Prc)),
            ("qcom,tunap", Some(Region::Prc)),
        ]),
    )
    .unwrap();
    let report = patch_image(&input, &output).unwrap();
    assert!(report.already_prc);
    assert_eq!(report.changed_byte_count, 0);
    assert_eq!(fs::read(input).unwrap(), fs::read(output).unwrap());
}

#[test]
fn rejects_mixed_regions() {
    assert_invalid(&[
        ("qcom,tuna", Some(Region::Row)),
        ("qcom,tuna", Some(Region::Prc)),
        ("qcom,tunap", Some(Region::Row)),
    ]);
}

#[test]
fn streaming_sha_and_copy_cover_the_entire_input() {
    let bytes: Vec<u8> = (0..3_000_017).map(|n| (n % 251) as u8).collect();
    let expected = format!("{:X}", Sha256::digest(&bytes));
    let actual = stream_hash(Cursor::new(&bytes), bytes.len() as u64).unwrap();
    assert_eq!(actual, expected);
    let mut output = Vec::new();
    let copied =
        stream_copy_exact(Cursor::new(&bytes), &mut output, bytes.len() as u64).unwrap();
    assert_eq!(copied, expected);
    assert_eq!(output, bytes);
}

#[test]
fn rejects_block_size_mismatch() {
    assert!(validate_partition_size(100_663_296, 100_663_295).is_err());
    assert!(validate_partition_size(4_096, 4_096).is_err());
}

#[test]
fn active_slot_and_non_allowlisted_partition_are_rejected() {
    assert!(ensure_write_target('a', 'a', "vendor_boot_a").is_err());
    assert!(ensure_write_target('a', 'b', "vbmeta_b").is_err());
    assert_eq!(partition_name('b').unwrap(), "vendor_boot_b");
    assert!(partition_name('x').is_err());
}

#[test]
fn known_335_fixture_matches_device_verified_digest_when_available() {
    let Ok(fixture) = std::env::var("TB376_VENDOR_BOOT_335_ROW") else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("vendor_boot-prc.img");
    let report = patch_image(std::path::Path::new(&fixture), &output).unwrap();
    assert_eq!(
        report.output_sha256,
        tb376_ota_helper_native::KNOWN_335_PRC_SHA256
    );
    assert_eq!(report.output_size, 100_663_296);
}

fn assert_invalid(specs: &[(&str, Option<Region>)]) {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("bad.img");
    fs::write(&input, vendor_boot(specs)).unwrap();
    assert!(inspect_image(&input).is_err());
}

fn vendor_boot(specs: &[(&str, Option<Region>)]) -> Vec<u8> {
    let mut bytes = b"prefix unrelated_ROW_txt\0".to_vec();
    for (compatible, region) in specs {
        bytes.extend_from_slice(&fdt(compatible, *region));
    }
    bytes
}

fn fdt(compatible: &str, region: Option<Region>) -> Vec<u8> {
    const BEGIN: u32 = 1;
    const END_NODE: u32 = 2;
    const END: u32 = 9;
    let strings = b"compatible\0region,country\0";
    let mut structure = Vec::new();
    push32(&mut structure, BEGIN);
    structure.extend_from_slice(&[0, 0, 0, 0]);
    property(&mut structure, 0, compatible.as_bytes());
    if let Some(region) = region {
        property(
            &mut structure,
            11,
            match region {
                Region::Row => b"ROW\0",
                Region::Prc => b"PRC\0",
            },
        );
    }
    push32(&mut structure, END_NODE);
    push32(&mut structure, END);

    let reserve_offset = 40_u32;
    let reserve_size = 16_u32;
    let structure_offset = reserve_offset + reserve_size;
    let strings_offset = structure_offset + structure.len() as u32;
    let total_size = strings_offset + strings.len() as u32;
    let mut output = Vec::new();
    for value in [
        0xD00D_FEED,
        total_size,
        structure_offset,
        strings_offset,
        reserve_offset,
        17,
        16,
        0,
        strings.len() as u32,
        structure.len() as u32,
    ] {
        push32(&mut output, value);
    }
    output.extend_from_slice(&[0; 16]);
    output.extend_from_slice(&structure);
    output.extend_from_slice(strings);
    output
}

fn property(structure: &mut Vec<u8>, name_offset: u32, value: &[u8]) {
    push32(structure, 3);
    push32(structure, value.len() as u32);
    push32(structure, name_offset);
    structure.extend_from_slice(value);
    while !structure.len().is_multiple_of(4) {
        structure.push(0);
    }
}

fn push32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_be_bytes());
}
