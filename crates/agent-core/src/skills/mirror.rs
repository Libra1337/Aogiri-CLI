//! The LynShen skills mirror on software.lynshen.org, for computers that
//! cannot reach GitHub (much of China). `deploy/skills-mirror` publishes it as
//! the software library's `skills` product: a manifest signed with the
//! library's key (docs/software-library.md) that lists `catalog.json` and one
//! `.tar.gz` per redistributable skill, each with its sha256. The catalog
//! holds the marketplace's sources in [`super::source_json`]'s shape; a skill
//! the mirror carries names its archive in `archive`.

use super::{
    fetch, install_package_bytes, normalize_repository_url, parse_source, validate_skill_id,
    verify_sha256, SkillSource, Timeouts, MAX_PACKAGE_BYTES,
};
use crate::update;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

pub const MIRROR_URL: &str = "https://software.lynshen.org/skills/latest.json";
/// The manifest and the catalog: a mirror that does not answer at once is
/// passed over for GitHub.
pub const MIRROR_TIMEOUTS: Timeouts = Timeouts {
    connect: Duration::from_secs(5),
    total: Duration::from_secs(10),
};
/// One skill's archive.
const ARCHIVE_TIMEOUTS: Timeouts = Timeouts {
    connect: Duration::from_secs(5),
    total: Duration::from_secs(60),
};
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;

/// The manifest to read: `LYNSHEN_SKILLS_MIRROR` (a `latest.json` URL, or
/// empty for no mirror), else the software library's.
pub fn mirror_url() -> Option<String> {
    match std::env::var("LYNSHEN_SKILLS_MIRROR") {
        Ok(url) => Some(url.trim().to_string()).filter(|url| !url.is_empty()),
        Err(_) => Some(MIRROR_URL.to_string()),
    }
}

#[derive(Debug, Clone)]
pub struct Mirror {
    /// The published version, which says when the mirror was built.
    pub version: String,
    pub sources: Vec<SkillSource>,
    /// (repository, skill id) → the archive's URL and sha256.
    archives: BTreeMap<(String, String), (String, String)>,
}

impl Mirror {
    pub fn source(&self, repository: &str) -> Option<&SkillSource> {
        let repository = normalize_repository_url(repository);
        self.sources
            .iter()
            .find(|source| source.repository == repository)
    }

    pub fn has_archive(&self, repository: &str, id: &str) -> bool {
        self.archive(repository, id).is_some()
    }

    fn archive(&self, repository: &str, id: &str) -> Option<&(String, String)> {
        self.archives
            .get(&(normalize_repository_url(repository), id.to_string()))
    }
}

/// The mirror's catalog, once its manifest verifies under the software
/// library's key and the catalog matches the manifest's sha256.
pub fn fetch_mirror(manifest_url: &str) -> Result<Mirror, String> {
    fetch_signed(manifest_url, update::MANIFEST_PUBLIC_KEY)
}

fn fetch_signed(manifest_url: &str, public_key: &str) -> Result<Mirror, String> {
    let manifest = fetch(manifest_url, MAX_MANIFEST_BYTES, MIRROR_TIMEOUTS)
        .map_err(|error| error.to_string())?;
    let release =
        update::verified_release(&String::from_utf8_lossy(&manifest), public_key, "skills")?;
    let release = update::parse_release(&release, update::manifest_dir(manifest_url))?;
    let files: BTreeMap<&str, &update::Asset> = release
        .assets
        .iter()
        .map(|asset| (asset.name.as_str(), asset))
        .collect();
    let catalog = files
        .get("catalog.json")
        .ok_or("the mirror lists no catalog.json")?;
    let bytes = fetch(&catalog.url, MAX_CATALOG_BYTES, MIRROR_TIMEOUTS)
        .map_err(|error| error.to_string())?;
    verify_sha256(&bytes, &catalog.sha256).map_err(|error| error.to_string())?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|error| format!("mirror catalog: {error}"))?;
    let mut mirror = Mirror {
        version: release.version.clone(),
        sources: Vec::new(),
        archives: BTreeMap::new(),
    };
    for item in value["sources"]
        .as_array()
        .ok_or("the mirror catalog has no sources")?
    {
        let source = parse_source(item)?;
        for skill in item["skills"].as_array().into_iter().flatten() {
            let (Some(id), Some(archive)) = (skill["id"].as_str(), skill["archive"].as_str())
            else {
                continue;
            };
            if skill["redistributable"] != true {
                continue;
            }
            let asset = files
                .get(archive)
                .ok_or_else(|| format!("the mirror's manifest does not list {archive}"))?;
            mirror.archives.insert(
                (source.repository.clone(), id.to_string()),
                (asset.url.clone(), asset.sha256.clone()),
            );
        }
        mirror.sources.push(source);
    }
    Ok(mirror)
}

/// Installs skill `id` of `repository` from its archive on the mirror as
/// `skills_dir/<id>`, once the archive matches the signed sha256. An earlier
/// install is replaced only once the archive is unpacked.
pub fn install_from_mirror(
    mirror: &Mirror,
    skills_dir: &Path,
    repository: &str,
    id: &str,
) -> io::Result<PathBuf> {
    validate_skill_id(id)?;
    super::check_source_free(skills_dir, id, repository)?;
    let (url, sha256) = mirror.archive(repository, id).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("the mirror has no archive of {id}"),
        )
    })?;
    let bytes = fetch(url, MAX_PACKAGE_BYTES, ARCHIVE_TIMEOUTS)?;
    verify_sha256(&bytes, sha256)?;
    let dir = skills_dir.join(id);
    install_package_bytes(&dir, &bytes, "tar.gz")?;
    super::record_source(&dir, repository)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::fs;

    const REPOSITORY: &str = "https://github.com/example/skills";

    fn temp(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lynshen-mirror-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `<id>/<files>` as deploy/skills-mirror packs a skill.
    fn archive(id: &str, files: &[(&str, &str, u32)]) -> Vec<u8> {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        for (name, content, mode) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(*mode);
            header.set_cksum();
            tar.append_data(&mut header, format!("{id}/{name}"), content.as_bytes())
                .unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    fn sha(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    /// A published mirror under `root/skills`, signed with a throwaway key;
    /// returns the manifest URL and the public key.
    fn publish(root: &Path, catalog: &Value, files: &[(&str, Vec<u8>)]) -> (String, String) {
        let version = root.join("skills/20261013.930.0");
        fs::create_dir_all(&version).unwrap();
        let catalog = catalog.to_string().into_bytes();
        let mut listed =
            vec![json!({ "name": "catalog.json", "sha256": sha(&catalog), "size": catalog.len() })];
        fs::write(version.join("catalog.json"), &catalog).unwrap();
        for (name, bytes) in files {
            fs::write(version.join(name), bytes).unwrap();
            listed.push(json!({ "name": name, "sha256": sha(bytes), "size": bytes.len() }));
        }
        let key = SigningKey::from_bytes(&[5u8; 32]);
        let release = json!({ "product": "skills", "version": "20261013.930.0", "files": listed })
            .to_string();
        let manifest = json!({
            "release": release,
            "signature": STANDARD.encode(key.sign(release.as_bytes()).to_bytes()),
        });
        fs::write(root.join("skills/latest.json"), manifest.to_string()).unwrap();
        (
            format!("file://{}", root.join("skills/latest.json").display()),
            STANDARD.encode(key.verifying_key().as_bytes()),
        )
    }

    fn catalog() -> Value {
        json!({ "sources": [{
            "name": "Example",
            "repository": REPOSITORY,
            "revision": "0123456789abcdef0123456789abcdef01234567",
            "dir": "skills",
            "skills": [
                { "id": "demo", "name": "Demo", "description": "Demo skill", "license": "MIT",
                  "redistributable": true, "archive": "example_skills__demo.tar.gz" },
                { "id": "closed", "name": "Closed", "description": "Listed only",
                  "license": "Proprietary", "redistributable": false }
            ]
        }]})
    }

    fn demo_archive() -> Vec<u8> {
        archive(
            "demo",
            &[
                (
                    "SKILL.md",
                    "---\nname: demo\ndescription: Demo skill\n---\n",
                    0o644,
                ),
                ("LICENSE", "MIT License\n", 0o644),
                ("scripts/run.sh", "echo ok\n", 0o755),
            ],
        )
    }

    #[test]
    fn a_signed_mirror_lists_its_sources_and_installs_a_checked_archive() {
        let root = temp("signed");
        let (url, key) = publish(
            &root,
            &catalog(),
            &[("example_skills__demo.tar.gz", demo_archive())],
        );

        let mirror = fetch_signed(&url, &key).unwrap();

        assert_eq!(mirror.version, "20261013.930.0");
        let source = mirror.source(&format!("{REPOSITORY}/")).unwrap();
        assert_eq!(source.name, "Example");
        assert_eq!(source.revision.len(), 40);
        assert_eq!(source.skills.len(), 2);
        assert!(source.skills[0].redistributable);
        assert!(!source.skills[1].redistributable);
        assert!(mirror.has_archive(REPOSITORY, "demo"));
        assert!(!mirror.has_archive(REPOSITORY, "closed"));

        let skills = root.join("installed");
        let dir = install_from_mirror(&mirror, &skills, REPOSITORY, "demo").unwrap();
        assert_eq!(dir, skills.join("demo"));
        assert!(super::super::skill_installed(&skills, "demo"));
        // It came from that repository; another one's `demo` is not it.
        assert!(super::super::installed_from(&skills, "demo", REPOSITORY));
        assert!(!super::super::installed_from(
            &skills,
            "demo",
            "https://github.com/someone/else"
        ));
        assert_eq!(
            fs::read_to_string(dir.join("LICENSE")).unwrap(),
            "MIT License\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join("scripts/run.sh")).unwrap(),
            "echo ok\n"
        );
        let error = install_from_mirror(&mirror, &skills, REPOSITORY, "closed").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_mirror_under_another_key_or_with_changed_files_is_refused() {
        let root = temp("refused");
        let (url, key) = publish(
            &root,
            &catalog(),
            &[("example_skills__demo.tar.gz", demo_archive())],
        );
        // Only the software library's own key is built in.
        let error = fetch_mirror(&url).unwrap_err();
        assert!(error.contains("signature"), "{error}");

        // A changed archive is refused, and the installed skill stays.
        let mirror = fetch_signed(&url, &key).unwrap();
        let skills = root.join("installed");
        fs::create_dir_all(skills.join("demo")).unwrap();
        fs::write(skills.join("demo/SKILL.md"), "old").unwrap();
        let version = root.join("skills/20261013.930.0");
        fs::write(
            version.join("example_skills__demo.tar.gz"),
            archive("demo", &[("SKILL.md", "evil", 0o644)]),
        )
        .unwrap();
        let error = install_from_mirror(&mirror, &skills, REPOSITORY, "demo").unwrap_err();
        assert!(error.to_string().contains("sha256"), "{error}");
        assert_eq!(
            fs::read_to_string(skills.join("demo/SKILL.md")).unwrap(),
            "old"
        );

        // So is a changed catalog.
        fs::write(
            version.join("catalog.json"),
            json!({ "sources": [] }).to_string(),
        )
        .unwrap();
        let error = fetch_signed(&url, &key).unwrap_err();
        assert!(error.contains("sha256"), "{error}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_catalog_naming_an_unlisted_archive_or_an_unsafe_skill_is_refused() {
        let root = temp("unlisted");
        let (url, key) = publish(&root, &catalog(), &[]);
        let error = fetch_signed(&url, &key).unwrap_err();
        assert!(error.contains("example_skills__demo.tar.gz"), "{error}");

        let root = temp("unsafe");
        let mut unsafe_catalog = catalog();
        unsafe_catalog["sources"][0]["skills"][0]["id"] = json!("../escape");
        let (url, key) = publish(
            &root,
            &unsafe_catalog,
            &[("example_skills__demo.tar.gz", demo_archive())],
        );
        assert!(fetch_signed(&url, &key).is_err());
        let mut unsafe_dir = catalog();
        unsafe_dir["sources"][0]["dir"] = json!("../x");
        let (url, key) = publish(
            &root,
            &unsafe_dir,
            &[("example_skills__demo.tar.gz", demo_archive())],
        );
        assert!(fetch_signed(&url, &key).is_err());
        let _ = fs::remove_dir_all(root);
    }

    /// A mirror `deploy/skills-mirror/build.py` built and signed, served
    /// locally: `LYNSHEN_TEST_MIRROR=http://127.0.0.1:8000/skills/latest.json
    /// LYNSHEN_TEST_MIRROR_KEY=<public key> cargo test -p lynshen-agent-core
    /// built_mirror -- --ignored`. Installs every archive it lists.
    #[test]
    #[ignore]
    fn built_mirror_installs_every_archive() {
        let url = std::env::var("LYNSHEN_TEST_MIRROR").unwrap();
        let key = std::env::var("LYNSHEN_TEST_MIRROR_KEY").unwrap();
        let mirror = fetch_signed(&url, &key).unwrap();
        let root = temp("built");
        let mut installed = 0;
        for source in &mirror.sources {
            for skill in &source.skills {
                if mirror.has_archive(&source.repository, &skill.id) {
                    let dir = install_from_mirror(&mirror, &root, &source.repository, &skill.id)
                        .unwrap_or_else(|error| panic!("{}: {error}", skill.id));
                    assert!(dir.join("SKILL.md").is_file(), "{}", skill.id);
                    installed += 1;
                }
            }
            eprintln!("{}: {} skills", source.name, source.skills.len());
        }
        eprintln!("installed {installed} archives from {}", mirror.version);
        assert!(installed > 0);
        let _ = fs::remove_dir_all(root);
    }
}
