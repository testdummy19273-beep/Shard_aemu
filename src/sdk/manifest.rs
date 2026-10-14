//! Google's SDK repository manifests: discovery, parsing and package selection.
//!
//! - `repository2-N.xml` lists the emulator and platform-tools.
//! - `addons_list-N.xml` lists the system-image sites; the Play image site is
//!   `sys-img/google_apis_playstore/sys-img2-N.xml`.
//!
//! N changes over time, so discovery starts from the last known value and probes upward until a
//! 404 (and downward if the known value disappeared). Nothing here hardcodes archive file names.

use std::cmp::Ordering;

use anyhow::{anyhow, bail, Context, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use reqwest::Url;

pub const REPO_BASE: &str = "https://dl.google.com/android/repository/";
/// Manifest versions current when this was written (2026-10); discovery probes from here.
pub const REPOSITORY_BASELINE: u32 = 4;
pub const ADDONS_LIST_BASELINE: u32 = 7;
pub const IMAGE_PATH: &str = "system-images;android-33;google_apis_playstore;x86_64";
const PLAYSTORE_SITE_DIR: &str = "sys-img/google_apis_playstore/";

// ---------------------------------------------------------------------------------------------
// A minimal DOM over quick-xml events (local names only; namespaces are irrelevant here).

#[derive(Debug, Default, Clone)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Element>,
    pub text: String,
}

impl Element {
    fn from_start(e: &BytesStart<'_>) -> Result<Self> {
        let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
        let mut attrs = Vec::new();
        for a in e.attributes() {
            let a = a?;
            attrs.push((
                String::from_utf8_lossy(a.key.local_name().as_ref()).into_owned(),
                a.unescape_value()?.into_owned(),
            ));
        }
        Ok(Self {
            name,
            attrs,
            ..Self::default()
        })
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> {
        self.children.iter().filter(move |c| c.name == name)
    }
    pub fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.trim())
    }
}

pub fn parse_xml(text: &str) -> Result<Element> {
    let mut reader = Reader::from_str(text);
    let mut stack = vec![Element::default()];
    loop {
        match reader.read_event()? {
            Event::Start(e) => stack.push(Element::from_start(&e)?),
            Event::Empty(e) => {
                let el = Element::from_start(&e)?;
                stack.last_mut().expect("root").children.push(el);
            }
            Event::End(_) => {
                let el = stack.pop().expect("open element");
                stack
                    .last_mut()
                    .ok_or_else(|| anyhow!("unbalanced XML"))?
                    .children
                    .push(el);
            }
            Event::Text(t) => stack.last_mut().expect("root").text.push_str(&t.decode()?),
            Event::CData(c) => stack
                .last_mut()
                .expect("root")
                .text
                .push_str(&String::from_utf8_lossy(&c)),
            Event::GeneralRef(r) => {
                let resolved = if r.is_char_ref() {
                    r.resolve_char_ref()?.map(String::from)
                } else {
                    match r.decode()?.as_ref() {
                        "amp" => Some("&".into()),
                        "lt" => Some("<".into()),
                        "gt" => Some(">".into()),
                        "quot" => Some("\"".into()),
                        "apos" => Some("'".into()),
                        _ => None,
                    }
                };
                if let Some(s) = resolved {
                    stack.last_mut().expect("root").text.push_str(&s);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if stack.len() != 1 {
        bail!("unbalanced XML");
    }
    stack
        .pop()
        .and_then(|root| root.children.into_iter().next())
        .ok_or_else(|| anyhow!("empty XML document"))
}

// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Revision {
    pub major: u32,
    pub minor: u32,
    pub micro: u32,
    /// 0 = final release; previews sort below the matching final release.
    pub preview: u32,
}

impl Revision {
    fn from_element(e: &Element) -> Self {
        let n = |k: &str| e.child_text(k).and_then(|t| t.parse().ok()).unwrap_or(0);
        Self {
            major: n("major"),
            minor: n("minor"),
            micro: n("micro"),
            preview: n("preview"),
        }
    }
    /// Release-first ordering: 37.2.12 > 37.2.12-rc1.
    pub fn cmp_release(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.micro)
            .cmp(&(other.major, other.minor, other.micro))
            .then_with(|| match (self.preview, other.preview) {
                (0, 0) => Ordering::Equal,
                (0, _) => Ordering::Greater,
                (_, 0) => Ordering::Less,
                (a, b) => a.cmp(&b),
            })
    }
}

impl std::fmt::Display for Revision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // System images only have a major revision ("r9"); show "9", not "9.0.0".
        if self.minor == 0 && self.micro == 0 {
            write!(f, "{}", self.major)?;
        } else {
            write!(f, "{}.{}.{}", self.major, self.minor, self.micro)?;
        }
        if self.preview > 0 {
            write!(f, "-rc{}", self.preview)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumKind {
    Sha1,
    Sha256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checksum {
    pub kind: ChecksumKind,
    pub hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archive {
    pub url: String,
    pub size: u64,
    pub checksum: Checksum,
    pub host_os: Option<String>,
    pub host_arch: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RemotePackage {
    pub path: String,
    pub display_name: String,
    pub revision: Revision,
    /// Channel *name* ("stable", "beta", "dev", "canary"), resolved from `channelRef`.
    pub channel: String,
    pub archives: Vec<Archive>,
}

/// Parses every `<remotePackage>` of a repository or sys-img manifest.
pub fn parse_packages(xml: &str) -> Result<Vec<RemotePackage>> {
    let root = parse_xml(xml)?;
    let channels: Vec<(String, String)> = root
        .children_named("channel")
        .filter_map(|c| Some((c.attr("id")?.to_string(), c.text.trim().to_string())))
        .collect();
    let mut out = Vec::new();
    for pkg in root.children_named("remotePackage") {
        let Some(path) = pkg.attr("path") else {
            continue;
        };
        // A package without channelRef is on the default (stable) channel.
        let channel = match pkg.child("channelRef").and_then(|c| c.attr("ref")) {
            Some(id) => channels
                .iter()
                .find(|(cid, _)| cid == id)
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| id.to_string()),
            None => "stable".to_string(),
        };
        let mut archives = Vec::new();
        if let Some(list) = pkg.child("archives") {
            for a in list.children_named("archive") {
                let Some(complete) = a.child("complete") else {
                    continue;
                };
                let Some(ck) = complete.child("checksum") else {
                    continue;
                };
                let kind = match ck.attr("type").unwrap_or("sha1") {
                    "sha1" => ChecksumKind::Sha1,
                    "sha-256" | "sha256" => ChecksumKind::Sha256,
                    _ => continue,
                };
                archives.push(Archive {
                    url: complete.child_text("url").unwrap_or_default().to_string(),
                    size: complete
                        .child_text("size")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0),
                    checksum: Checksum {
                        kind,
                        hex: ck.text.trim().to_ascii_lowercase(),
                    },
                    host_os: a.child_text("host-os").map(str::to_string),
                    host_arch: a.child_text("host-arch").map(str::to_string),
                });
            }
        }
        out.push(RemotePackage {
            path: path.to_string(),
            display_name: pkg.child_text("display-name").unwrap_or(path).to_string(),
            revision: pkg
                .child("revision")
                .map(Revision::from_element)
                .unwrap_or_default(),
            channel,
            archives,
        });
    }
    Ok(out)
}

/// Site URLs listed in an `addons_list-N.xml`.
pub fn parse_site_urls(xml: &str) -> Result<Vec<String>> {
    let root = parse_xml(xml)?;
    Ok(root
        .children_named("site")
        .filter_map(|s| s.child_text("url").map(str::to_string))
        .collect())
}

/// The Play (google_apis_playstore) system-image site from an addons list.
pub fn playstore_site(urls: &[String]) -> Option<&str> {
    urls.iter()
        .map(String::as_str)
        .find(|u| u.starts_with(PLAYSTORE_SITE_DIR) && u.ends_with(".xml"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageId {
    Emulator,
    PlatformTools,
    SystemImage,
}

impl PackageId {
    pub const ALL: [PackageId; 3] = [Self::PlatformTools, Self::Emulator, Self::SystemImage];
    pub fn key(self) -> &'static str {
        match self {
            Self::Emulator => "emulator",
            Self::PlatformTools => "platform-tools",
            Self::SystemImage => "system-image",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Emulator => "Android Emulator",
            Self::PlatformTools => "Platform tools (adb)",
            Self::SystemImage => "Android 13 Google Play image (x86_64)",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Selected {
    pub id: PackageId,
    pub path: String,
    pub revision: Revision,
    pub archive: Archive,
    /// Absolute download URL.
    pub url: Url,
}

impl Selected {
    pub fn file_name(&self) -> String {
        self.url
            .path_segments()
            .and_then(|mut s| s.next_back())
            .unwrap_or("download.zip")
            .to_string()
    }
}

/// Latest stable package at exactly `path` with an archive for `host_os` (None = host-neutral).
pub fn select(
    packages: &[RemotePackage],
    path: &str,
    host_os: Option<&str>,
) -> Option<(RemotePackage, Archive)> {
    packages
        .iter()
        .filter(|p| p.path == path && p.channel == "stable")
        .filter_map(|p| {
            let archive = p.archives.iter().find(|a| match host_os {
                Some(os) => {
                    a.host_os.as_deref() == Some(os)
                        && matches!(a.host_arch.as_deref(), None | Some("x64"))
                }
                None => a.host_os.is_none(),
            })?;
            Some((p.clone(), archive.clone()))
        })
        .max_by(|(a, _), (b, _)| a.revision.cmp_release(&b.revision))
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub emulator: Selected,
    pub platform_tools: Selected,
    pub image: Selected,
}

impl Plan {
    pub fn get(&self, id: PackageId) -> &Selected {
        match id {
            PackageId::Emulator => &self.emulator,
            PackageId::PlatformTools => &self.platform_tools,
            PackageId::SystemImage => &self.image,
        }
    }
    pub fn total_bytes(&self) -> u64 {
        PackageId::ALL
            .iter()
            .map(|id| self.get(*id).archive.size)
            .sum()
    }
}

/// Pure selection from already-downloaded manifest texts (unit-tested with fixtures).
pub fn plan_from_manifests(
    repo_xml: &str,
    repo_url: &Url,
    sysimg_xml: &str,
    sysimg_url: &Url,
) -> Result<Plan> {
    let repo = parse_packages(repo_xml).context("parsing the SDK repository manifest")?;
    let images = parse_packages(sysimg_xml).context("parsing the system image manifest")?;
    let pick = |id: PackageId,
                pkgs: &[RemotePackage],
                path: &str,
                os: Option<&str>,
                base: &Url|
     -> Result<Selected> {
        let (pkg, archive) = select(pkgs, path, os)
            .ok_or_else(|| anyhow!("no stable Windows package \"{path}\" in Google's manifest"))?;
        let url = base
            .join(&archive.url)
            .with_context(|| format!("bad archive URL {}", archive.url))?;
        Ok(Selected {
            id,
            path: pkg.path,
            revision: pkg.revision,
            archive,
            url,
        })
    };
    Ok(Plan {
        emulator: pick(
            PackageId::Emulator,
            &repo,
            "emulator",
            Some("windows"),
            repo_url,
        )?,
        platform_tools: pick(
            PackageId::PlatformTools,
            &repo,
            "platform-tools",
            Some("windows"),
            repo_url,
        )?,
        image: pick(
            PackageId::SystemImage,
            &images,
            IMAGE_PATH,
            None,
            sysimg_url,
        )?,
    })
}

// ---------------------------------------------------------------------------------------------
// Network side.

/// Finds the newest `<prefix><N>.xml` that exists, starting at `baseline`.
pub fn discover(
    client: &reqwest::blocking::Client,
    prefix: &str,
    baseline: u32,
) -> Result<(Url, String)> {
    let base = Url::parse(REPO_BASE)?;
    let url_for = |n: u32| base.join(&format!("{prefix}{n}.xml"));
    let exists = |n: u32| -> Result<bool> {
        let resp = client.head(url_for(n)?).send()?;
        Ok(resp.status().is_success())
    };
    let mut found = None;
    if exists(baseline)? {
        let mut n = baseline;
        while n < baseline + 10 && exists(n + 1)? {
            n += 1;
        }
        found = Some(n);
    } else {
        for n in (1..baseline).rev() {
            if exists(n)? {
                found = Some(n);
                break;
            }
        }
    }
    let n = found.ok_or_else(|| anyhow!("no {prefix}N.xml manifest found at {REPO_BASE}"))?;
    let url = url_for(n)?;
    let text = fetch_text(client, &url)?;
    Ok((url, text))
}

pub fn fetch_text(client: &reqwest::blocking::Client, url: &Url) -> Result<String> {
    let resp = client
        .get(url.clone())
        .send()
        .with_context(|| format!("downloading {url}"))?;
    if !resp.status().is_success() {
        bail!("{url} returned HTTP {}", resp.status());
    }
    resp.text().with_context(|| format!("reading {url}"))
}

/// Discovers the current manifests and selects the three packages Shard needs.
pub fn resolve(client: &reqwest::blocking::Client) -> Result<Plan> {
    let (repo_url, repo_xml) = discover(client, "repository2-", REPOSITORY_BASELINE)?;
    let (_, addons_xml) = discover(client, "addons_list-", ADDONS_LIST_BASELINE)?;
    let sites = parse_site_urls(&addons_xml)?;
    let site = playstore_site(&sites)
        .ok_or_else(|| anyhow!("Google's addons list has no google_apis_playstore image site"))?;
    let sysimg_url = Url::parse(REPO_BASE)?.join(site)?;
    let sysimg_xml = fetch_text(client, &sysimg_url)?;
    plan_from_manifests(&repo_xml, &repo_url, &sysimg_xml, &sysimg_url)
}
