//! Manifest parsing and package selection against synthetic fixtures that follow the schema of
//! the SDK repository manifests (all revisions, sizes, checksums and file names are made up). No
//! network.

use reqwest::Url;
use shard::sdk::manifest::{
    self, parse_packages, parse_site_urls, playstore_site, select, ChecksumKind, PackageId,
    IMAGE_PATH,
};

const REPO: &str = include_str!("fixtures/repository2-4.xml");
const ADDONS: &str = include_str!("fixtures/addons_list-7.xml");
const SYSIMG: &str = include_str!("fixtures/sys-img2-5.xml");

fn repo_url() -> Url {
    Url::parse("https://dl.google.com/android/repository/repository2-4.xml").unwrap()
}
fn sysimg_url() -> Url {
    Url::parse(
        "https://dl.google.com/android/repository/sys-img/google_apis_playstore/sys-img2-5.xml",
    )
    .unwrap()
}

#[test]
fn stable_emulator_wins_over_newer_dev_channel() {
    let pkgs = parse_packages(REPO).unwrap();
    // The fixture lists dev 31.3.7 first, then "emulators;1000305", and stable 31.2.9 last.
    let dev = pkgs
        .iter()
        .find(|p| p.path == "emulator" && p.revision.to_string() == "31.3.7")
        .unwrap();
    assert_eq!(dev.channel, "dev");
    let (pkg, archive) = select(&pkgs, "emulator", Some("windows")).unwrap();
    assert_eq!(pkg.revision.to_string(), "31.2.9");
    assert_eq!(pkg.channel, "stable");
    assert_eq!(archive.url, "emulator-windows_x64-1000209.zip");
    assert_eq!(archive.size, 320_000_209);
    assert_eq!(archive.checksum.kind, ChecksumKind::Sha1);
    assert_eq!(
        archive.checksum.hex,
        "90d9a8d5a99b766ed94b512e5ddc047cdbc0b1be"
    );
}

#[test]
fn platform_tools_windows_archive() {
    let pkgs = parse_packages(REPO).unwrap();
    let (pkg, archive) = select(&pkgs, "platform-tools", Some("windows")).unwrap();
    assert_eq!(pkg.revision.to_string(), "31.0.2");
    assert_eq!(archive.url, "platform-tools_r31.0.2-win.zip");
}

#[test]
fn image_is_matched_by_exact_path_not_ext_variants() {
    let pkgs = parse_packages(SYSIMG).unwrap();
    // ext4/ext5 variants share the tag and abi; only the exact path may be picked.
    assert!(pkgs
        .iter()
        .any(|p| p.path == "system-images;android-33-ext5;google_apis_playstore;x86_64"));
    let (pkg, archive) = select(&pkgs, IMAGE_PATH, None).unwrap();
    assert_eq!(pkg.path, IMAGE_PATH);
    // A newer r04 on the dev channel is skipped.
    assert_eq!(pkg.revision.major, 3);
    assert_eq!(archive.url, "x86_64-33_r03.zip");
    assert_eq!(archive.size, 1_200_000_303);
    assert_eq!(
        archive.checksum.hex,
        "4d7957fb12bda39744fba43134bf94df154a6b5b"
    );
}

#[test]
fn playstore_site_comes_from_the_addons_list() {
    let sites = parse_site_urls(ADDONS).unwrap();
    assert!(sites.len() > 10);
    assert_eq!(
        playstore_site(&sites),
        Some("sys-img/google_apis_playstore/sys-img2-5.xml")
    );
}

#[test]
fn plan_resolves_absolute_urls() {
    let plan = manifest::plan_from_manifests(REPO, &repo_url(), SYSIMG, &sysimg_url()).unwrap();
    assert_eq!(
        plan.get(PackageId::Emulator).url.as_str(),
        "https://dl.google.com/android/repository/emulator-windows_x64-1000209.zip"
    );
    assert_eq!(
        plan.get(PackageId::PlatformTools).url.as_str(),
        "https://dl.google.com/android/repository/platform-tools_r31.0.2-win.zip"
    );
    assert_eq!(
        plan.get(PackageId::SystemImage).url.as_str(),
        "https://dl.google.com/android/repository/sys-img/google_apis_playstore/x86_64-33_r03.zip"
    );
    assert_eq!(
        plan.get(PackageId::SystemImage).file_name(),
        "x86_64-33_r03.zip"
    );
    assert_eq!(
        plan.total_bytes(),
        320_000_209 + 1_200_000_303 + plan.get(PackageId::PlatformTools).archive.size
    );
}

#[test]
fn missing_package_is_a_clear_error() {
    let err = manifest::plan_from_manifests(REPO, &repo_url(), REPO, &sysimg_url()).unwrap_err();
    assert!(format!("{err:#}").contains(IMAGE_PATH), "{err:#}");
}

#[test]
fn garbage_is_rejected() {
    assert!(parse_packages("<not-closed>").is_err());
}
