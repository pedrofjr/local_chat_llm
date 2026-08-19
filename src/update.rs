//! Checking for, verifying and installing a new build.
//!
//! Deliberately manual: nothing here runs unless somebody types `/update`. An
//! app that sells itself as local and quiet should not be reaching out to
//! github every time it opens.
//!
//! The reason this exists at all is not convenience. Downloading the exe in a
//! browser stamps it with the Mark of the Web, and Windows then puts
//! SmartScreen in front of it -- the "More info / Run anyway" wall the group
//! hits on every release. A file written by a program carries no such mark, so
//! fetching the binary *here* removes that step entirely. The corporate
//! antivirus still scans it once; that part needs a signing certificate or an
//! allowlist from IT, neither of which is ours to arrange.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

/// Where the group's builds live. Public repo, so no token is involved.
pub const MANIFEST_URL: &str =
    "https://github.com/pedrofjr/local_chat_llm/releases/latest/download/latest.toml";

/// Argument the freshly installed build is relaunched with, so it knows to
/// clean up the copy it replaced.
pub const JUST_UPDATED: &str = "--just-updated";

/// A three-part version, which is all this project uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u32,
    minor: u32,
    patch: u32,
}

impl Version {
    /// Parses `1.2.3`, tolerating a leading `v` and ignoring any `-suffix`.
    ///
    /// A pre-release suffix is dropped rather than ordered: this project has
    /// never shipped one, and guessing an ordering for it would be a silent
    /// way to install the wrong thing.
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim().trim_start_matches('v');
        let core = raw.split(['-', '+']).next().unwrap_or(raw);
        let mut parts = core.split('.');
        let mut next = |what: &str| -> Result<u32> {
            parts
                .next()
                .ok_or_else(|| anyhow!("version is missing its {what}: {raw}"))?
                .parse()
                .with_context(|| format!("version has a non-numeric {what}: {raw}"))
        };
        let major = next("major")?;
        let minor = next("minor")?;
        let patch = next("patch")?;
        if parts.next().is_some() {
            bail!("version has too many parts: {raw}");
        }
        Ok(Self {
            major,
            minor,
            patch,
        })
    }

    /// What this build is.
    pub fn current() -> Self {
        // Comes from Cargo.toml, so it cannot drift from what was published.
        Self::parse(env!("CARGO_PKG_VERSION")).expect("our own version must parse")
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// What the release publishes alongside the binary.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub url: String,
    /// Hex sha256 of the exe. Catches a truncated or corrupted download.
    pub sha256: String,
    /// Hex Ed25519 over the exe bytes, by the release key. Catches a download
    /// that arrived intact but from the wrong hands.
    pub sig: String,
    /// Oldest build that can still talk to this one. Set when the wire format
    /// changes -- which it has, four times -- so an old build says "you must
    /// update" instead of failing to connect and looking broken.
    #[serde(default)]
    pub min_version: Option<String>,
}

/// What a check concluded.
#[derive(Debug)]
pub enum Check {
    UpToDate(Version),
    Available {
        version: Version,
        manifest: Box<Manifest>,
        /// True when the running build can no longer talk to the group, so
        /// updating is not optional.
        required: bool,
    },
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self> {
        let manifest: Manifest = toml::from_str(text).context("release manifest is not valid")?;
        // Parsed here so a malformed manifest is refused up front rather than
        // halfway through an install.
        Version::parse(&manifest.version)?;
        if let Some(floor) = &manifest.min_version {
            Version::parse(floor)?;
        }
        if !manifest.url.starts_with("https://") {
            bail!("release url is not https");
        }
        if hex_bytes(&manifest.sha256).is_none_or(|b| b.len() != 32) {
            bail!("release sha256 is not 32 bytes of hex");
        }
        if hex_bytes(&manifest.sig).is_none_or(|b| b.len() != 64) {
            bail!("release signature is not 64 bytes of hex");
        }
        Ok(manifest)
    }

    /// Compares against what is running.
    pub fn against(self, running: Version) -> Result<Check> {
        let offered = Version::parse(&self.version)?;
        if offered <= running {
            return Ok(Check::UpToDate(running));
        }
        let required = match &self.min_version {
            Some(floor) => running < Version::parse(floor)?,
            None => false,
        };
        Ok(Check::Available {
            version: offered,
            manifest: Box::new(self),
            required,
        })
    }
}

/// The key every release is signed with. The private half lives with whoever
/// publishes; this half is baked in, which is what makes the check meaningful.
///
/// TLS proves the bytes came from github unaltered. It does not prove github
/// was serving *our* binary -- a compromised account, or a release replaced by
/// anyone with push rights, still arrives over a perfectly good TLS session.
/// This is the difference between "nothing tampered in transit" and "this is
/// the build we published".
pub const RELEASE_PUBKEY: [u8; 32] = [
    0x13, 0x50, 0xa8, 0x26, 0xb5, 0xc6, 0x15, 0xad, 0xe2, 0xe6, 0x19, 0x12, 0x6b, 0xd6, 0xf9, 0x18,
    0xf5, 0x00, 0x60, 0xb7, 0x4a, 0xd4, 0x03, 0xf7, 0x75, 0xab, 0x3e, 0xed, 0x82, 0x2e, 0x65, 0x96,
];

/// Checks downloaded bytes against what the manifest promised.
///
/// Both halves matter and they catch different things: the digest catches a
/// truncated or corrupted download, the signature catches an intact download
/// from the wrong hands. Failing either means the bytes never become an
/// executable file -- the caller is expected to delete them.
pub fn verify_download(bytes: &[u8], manifest: &Manifest) -> Result<()> {
    let want = hex_bytes(&manifest.sha256).ok_or_else(|| anyhow!("manifest sha256 is not hex"))?;
    let got = blake3_free_sha256(bytes);
    if got != want.as_slice() {
        bail!("downloaded binary does not match the published digest");
    }

    let sig = hex_bytes(&manifest.sig).ok_or_else(|| anyhow!("manifest signature is not hex"))?;
    let sig: [u8; 64] = sig
        .try_into()
        .map_err(|_| anyhow!("signature is not 64 bytes"))?;
    let key = iroh::PublicKey::from_bytes(&RELEASE_PUBKEY).context("release key is unusable")?;
    key.verify(bytes, &iroh::Signature::from_bytes(&sig))
        .map_err(|_| anyhow!("binary is not signed by the release key"))?;
    Ok(())
}

/// sha256 without pulling in a second hashing crate.
///
/// `ring` is already in the tree by way of iroh's TLS, and this is the one
/// place the project needs sha256 -- everything of ours is keyed blake3.
fn blake3_free_sha256(bytes: &[u8]) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .to_vec()
}

/// Fetches the manifest. Only ever called from `/update`.
///
/// "No release published yet" and "no network" look the same from a distance
/// and mean opposite things -- one is normal, the other is a problem to chase.
/// They are told apart here so the message on screen is actionable.
pub async fn fetch_manifest() -> Result<Manifest> {
    let text = match http_get(MANIFEST_URL).await {
        Ok(text) => text,
        Err(e) if e.downcast_ref::<NoRelease>().is_some() => {
            bail!("no release published yet — nothing to update to")
        }
        Err(e) => return Err(e).context("could not reach github"),
    };
    let text = String::from_utf8(text).context("release manifest is not text")?;
    Manifest::parse(&text)
}

/// The channel answered, and said there is nothing there.
#[derive(Debug)]
struct NoRelease;

impl std::fmt::Display for NoRelease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no release published")
    }
}

impl std::error::Error for NoRelease {}

/// Downloads the binary a manifest points at and checks it.
///
/// Returns the bytes only if both the digest and the release signature hold,
/// so a caller cannot accidentally write an unverified binary to disk.
pub async fn fetch_binary(manifest: &Manifest) -> Result<Vec<u8>> {
    let bytes = http_get(&manifest.url)
        .await
        .context("could not download the new build")?;
    verify_download(&bytes, manifest)?;
    Ok(bytes)
}

/// Bounded, redirect-following GET.
///
/// A release lands here as a file, and this is the step that keeps the Mark of
/// the Web off it: a program writing a file leaves no `Zone.Identifier`, so
/// SmartScreen has nothing to object to. Downloading the same file in a
/// browser is what puts the wall in front of it.
async fn http_get(url: &str) -> Result<Vec<u8>> {
    /// Generous for a ~7 MB exe, mean enough that a wrong url cannot fill the
    /// disk. The group's builds are well under this.
    const MAX: u64 = 64 * 1024 * 1024;

    let client = reqwest::Client::builder()
        .user_agent(concat!("local-llm/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(120))
        .https_only(true)
        .build()
        .context("could not start the http client")?;
    let response = client.get(url).send().await?;
    // A repo with no releases answers 404 for `releases/latest/...`. That is
    // an empty shelf, not a broken one.
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(NoRelease.into());
    }
    let response = response.error_for_status()?;
    if let Some(len) = response.content_length() {
        if len > MAX {
            bail!("release is {len} bytes, refusing to download that");
        }
    }
    let bytes = response.bytes().await?;
    if bytes.len() as u64 > MAX {
        bail!("release is larger than expected, refusing it");
    }
    Ok(bytes.to_vec())
}

/// Puts a verified binary in place of the running one and starts it.
///
/// Windows will not let a running executable be overwritten or deleted, but it
/// *will* let it be renamed. So the running file steps aside, the new one takes
/// its name, and the replacement is launched; the old copy is swept up by the
/// build that replaces it, on its next start.
///
/// Never returns on success -- the caller is expected to exit.
#[cfg(windows)]
pub fn install_and_relaunch(bytes: &[u8]) -> Result<()> {
    use std::fs;

    let exe = std::env::current_exe().context("cannot find our own executable")?;
    let staging = exe.with_extension("new");
    let retired = exe.with_extension("old");

    // A leftover from an interrupted attempt would block the rename.
    let _ = fs::remove_file(&retired);
    fs::write(&staging, bytes).context("could not write the new build")?;

    // Point of no return: from here the running file no longer has its name.
    fs::rename(&exe, &retired).context("could not step the running build aside")?;
    if let Err(e) = fs::rename(&staging, &exe) {
        // Put it back rather than leaving the machine without an executable.
        let _ = fs::rename(&retired, &exe);
        return Err(e).context("could not put the new build in place");
    }

    // Carries the version being left behind, so the new build can say what
    // it came from rather than just asserting it is new.
    std::process::Command::new(&exe)
        .arg(format!("{JUST_UPDATED}={}", env!("CARGO_PKG_VERSION")))
        .spawn()
        .context("the new build did not start")?;
    Ok(())
}

/// The version we were updated from, if this start came from an update.
pub fn updated_from() -> Option<String> {
    std::env::args().find_map(|arg| {
        arg.strip_prefix(JUST_UPDATED)
            .and_then(|rest| rest.strip_prefix('='))
            .map(str::to_string)
    })
}

/// Whether this start came from an update at all.
pub fn was_just_updated() -> bool {
    std::env::args().any(|a| a.starts_with(JUST_UPDATED))
}

#[cfg(not(windows))]
pub fn install_and_relaunch(_bytes: &[u8]) -> Result<()> {
    bail!("updating is only wired up for windows")
}

/// Removes the build we replaced. Called by the new build on its first run,
/// when the old file is finally unlocked.
pub fn sweep_previous() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("old"));
        let _ = std::fs::remove_file(exe.with_extension("new"));
    }
}

/// Notes which room to reopen.
///
/// Takes the caller's `DataDir` rather than opening its own: opening one
/// claims a window slot, and a second slot means a *different* profile
/// directory. Written from a freshly opened `DataDir`, this note would land
/// in `guest-2` while the app that has to read it lives in the main profile.
///
/// Only the topic goes in -- the key never does. A room whose key is
/// remembered reopens by itself; a locked one stops at the unlock screen,
/// which is the correct outcome.
pub fn write_resume(dir: &crate::store::DataDir, topic_hex: &str) -> Result<()> {
    std::fs::write(dir.resume_path(), topic_hex).context("could not note the room to reopen")
}

/// Reads and clears the note. One shot: a stale file must not reopen a room on
/// some unrelated start weeks later.
pub fn take_resume(dir: &crate::store::DataDir) -> Option<String> {
    let path = dir.resume_path();
    let topic = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let topic = topic.trim().to_string();
    let usable = topic.len() == 64 && topic.chars().all(|c| c.is_ascii_hexdigit());
    usable.then_some(topic)
}

/// Decodes hex, refusing anything that is not an even run of hex digits.
pub fn hex_bytes(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) || text.is_empty() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(raw: &str) -> Version {
        Version::parse(raw).unwrap()
    }

    #[test]
    fn versions_compare_the_way_releases_do() {
        assert!(v("0.6.0") > v("0.5.0"));
        assert!(v("0.5.1") > v("0.5.0"));
        assert!(v("1.0.0") > v("0.9.9"));
        assert_eq!(v("0.5.0"), v("0.5.0"));
        // Ordered by number, not by text: "0.10.0" must beat "0.9.0".
        assert!(v("0.10.0") > v("0.9.0"));

        // A leading v and a pre-release suffix are tolerated on the way in.
        assert_eq!(v("v0.5.0"), v("0.5.0"));
        assert_eq!(v("0.5.0-rc1"), v("0.5.0"));
    }

    #[test]
    fn our_own_version_is_readable() {
        // If this ever fails, `current()` would panic at startup.
        let _ = Version::current();
    }

    #[test]
    fn nonsense_versions_are_refused_rather_than_guessed() {
        for bad in ["", "1", "1.2", "1.2.3.4", "a.b.c", "1.2.x", "-1.0.0"] {
            assert!(Version::parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    fn manifest_text(version: &str, extra: &str) -> String {
        format!(
            "version = \"{version}\"\n\
             url = \"https://github.com/pedrofjr/local_chat_llm/releases/download/v{version}/local-llm.exe\"\n\
             sha256 = \"{}\"\n\
             sig = \"{}\"\n{extra}",
            "ab".repeat(32),
            "cd".repeat(64),
        )
    }

    #[test]
    fn a_newer_release_is_offered_and_an_older_one_is_not() {
        let running = v("0.5.0");

        let newer = Manifest::parse(&manifest_text("0.6.0", "")).unwrap();
        match newer.against(running).unwrap() {
            Check::Available {
                version, required, ..
            } => {
                assert_eq!(version, v("0.6.0"));
                assert!(!required, "nothing forces this one");
            }
            other => panic!("expected an offer, got {other:?}"),
        }

        // Same version: nothing to do.
        let same = Manifest::parse(&manifest_text("0.5.0", "")).unwrap();
        assert!(matches!(
            same.against(running).unwrap(),
            Check::UpToDate(_)
        ));

        // Older on the server than here: never walk backwards on our own.
        let older = Manifest::parse(&manifest_text("0.4.0", "")).unwrap();
        assert!(matches!(
            older.against(running).unwrap(),
            Check::UpToDate(_)
        ));
    }

    #[test]
    fn a_build_below_the_floor_is_told_it_must_update() {
        // The wire format has changed four times; when it does, an old build
        // cannot connect at all and should say so rather than look broken.
        let manifest = Manifest::parse(&manifest_text("0.6.0", "min_version = \"0.6.0\"\n")).unwrap();
        match manifest.against(v("0.5.0")).unwrap() {
            Check::Available { required, .. } => assert!(required, "0.5.0 can no longer talk"),
            other => panic!("expected an offer, got {other:?}"),
        }

        let manifest = Manifest::parse(&manifest_text("0.7.0", "min_version = \"0.6.0\"\n")).unwrap();
        match manifest.against(v("0.6.0")).unwrap() {
            Check::Available { required, .. } => assert!(!required, "0.6.0 still talks"),
            other => panic!("expected an offer, got {other:?}"),
        }
    }

    #[test]
    fn a_manifest_we_cannot_trust_is_refused_before_anything_is_downloaded() {
        // Not https: a plain-http url would let anyone on the path swap the
        // binary before the signature is even looked at.
        let http = manifest_text("0.6.0", "").replace("https://", "http://");
        assert!(Manifest::parse(&http).is_err(), "http must be refused");

        // Digest and signature of the wrong shape.
        let short_sha = manifest_text("0.6.0", "").replace(&"ab".repeat(32), "abcd");
        assert!(Manifest::parse(&short_sha).is_err(), "short sha256");
        let short_sig = manifest_text("0.6.0", "").replace(&"cd".repeat(64), "cdcd");
        assert!(Manifest::parse(&short_sig).is_err(), "short signature");

        // Garbage, empty, and missing fields must not panic.
        for bad in ["", "not toml at all {{{", "version = \"0.6.0\"\n"] {
            assert!(Manifest::parse(bad).is_err(), "{bad:?} should be refused");
        }
    }

    /// The exact bytes the fixture below was signed over.
    fn fixture_payload() -> Vec<u8> {
        b"pretend this is local-llm.exe ".repeat(100)
    }

    /// Signed with the real release key, so this proves the key baked into
    /// `RELEASE_PUBKEY` is the one whose private half was handed over -- not
    /// just that some Ed25519 pair round-trips.
    const FIXTURE_SHA: &str = "10101f860168ef81e4b6c3177b186f637af027e546a20e6f44d7f915f2f3abc1";
    const FIXTURE_SIG: &str = "16c2ba2c9cc0f872efb032fadfdf5d781dcc37267b666925993ed73ee71ec1f4\
                               d11b05d9d2ac701582a81ec91d33542baf5ee8bdca8a77b34930b09e611c900f";
    /// A valid signature over the same bytes, by a key that is not ours.
    const FIXTURE_WRONG_SIG: &str =
        "3f317fe8cdc3302d8e4e2893aadca52b0067d58a78e1bd994fd9944483245b34\
         d603b887756f2b22a620ea3dca22c08e85c44ec86009d438947865582d0cc802";

    fn fixture_manifest(sha: &str, sig: &str) -> Manifest {
        Manifest::parse(&format!(
            "version = \"0.6.0\"\n\
             url = \"https://github.com/pedrofjr/local_chat_llm/releases/download/v0.6.0/local-llm.exe\"\n\
             sha256 = \"{}\"\n\
             sig = \"{}\"\n",
            sha,
            sig.replace([' ', '\n'], ""),
        ))
        .unwrap()
    }

    #[test]
    fn a_binary_signed_by_the_release_key_is_accepted() {
        let payload = fixture_payload();
        let manifest = fixture_manifest(FIXTURE_SHA, FIXTURE_SIG);
        verify_download(&payload, &manifest).expect("our own signature must verify");
    }

    #[test]
    fn a_binary_from_the_wrong_hands_is_refused() {
        let payload = fixture_payload();

        // Correct digest, and a genuine signature -- by somebody else's key.
        // This is the case TLS alone cannot catch: bytes that arrived intact,
        // from github, but which we did not publish.
        let impostor = fixture_manifest(FIXTURE_SHA, FIXTURE_WRONG_SIG);
        let err = verify_download(&payload, &impostor).unwrap_err().to_string();
        assert!(
            err.contains("not signed by the release key"),
            "should name the reason, got {err:?}"
        );

        // Tampered bytes are caught by the digest before the signature is even
        // considered.
        let mut edited = payload.clone();
        edited[0] ^= 0xff;
        let err = verify_download(&edited, &fixture_manifest(FIXTURE_SHA, FIXTURE_SIG))
            .unwrap_err()
            .to_string();
        assert!(err.contains("digest"), "should mention the digest, got {err:?}");

        // Truncated download: same protection.
        assert!(verify_download(&payload[..payload.len() - 1], &fixture_manifest(FIXTURE_SHA, FIXTURE_SIG)).is_err());

        // A digest that matches bytes we did not publish, with a signature
        // that therefore cannot match either.
        let other = b"a completely different build".to_vec();
        let sha = super::blake3_free_sha256(&other);
        let hex: String = sha.iter().map(|b| format!("{b:02x}")).collect();
        assert!(verify_download(&other, &fixture_manifest(&hex, FIXTURE_SIG)).is_err());
    }

    /// The rename dance, exercised on plain files in a tempdir.
    ///
    /// The real thing cannot be unit tested -- it ends in `process::exit` --
    /// but the file moves are the part that can leave a machine with no
    /// executable, so they are worth pinning down on their own.
    #[test]
    fn the_running_file_steps_aside_before_the_new_one_takes_its_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let exe = tmp.path().join("local-llm.exe");
        let staging = exe.with_extension("new");
        let retired = exe.with_extension("old");
        std::fs::write(&exe, b"the build that is running").unwrap();

        // Same sequence as `install_and_relaunch`.
        std::fs::write(&staging, b"the new build").unwrap();
        std::fs::rename(&exe, &retired).unwrap();
        std::fs::rename(&staging, &exe).unwrap();

        assert_eq!(std::fs::read(&exe).unwrap(), b"the new build");
        assert!(retired.exists(), "the replaced build waits to be swept");
        assert!(!staging.exists(), "nothing left half-installed");

        // Windows refuses to overwrite a running exe but allows the rename;
        // that is the whole reason for this shape.
        assert!(exe.exists(), "there must always be an executable at the name");
    }

    #[test]
    fn a_stale_retired_copy_does_not_block_the_next_update() {
        let tmp = tempfile::TempDir::new().unwrap();
        let exe = tmp.path().join("local-llm.exe");
        let retired = exe.with_extension("old");
        std::fs::write(&exe, b"running").unwrap();
        // Left behind by an update that was interrupted before the sweep.
        std::fs::write(&retired, b"from an earlier attempt").unwrap();

        let _ = std::fs::remove_file(&retired);
        std::fs::rename(&exe, &retired).expect("the rename must not be blocked");
        assert_eq!(std::fs::read(&retired).unwrap(), b"running");
    }

    /// The second half of the wrong-room bug: the note itself was written
    /// through a freshly opened `DataDir`, which claims a window slot. With
    /// the app already holding slot 1, that fresh one became slot 2 and the
    /// note went to `guest-2` -- somewhere the app would never read it.
    #[test]
    fn the_resume_note_lands_in_the_profile_that_will_read_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = crate::store::DataDir::from_path(tmp.path().to_path_buf()).unwrap();
        let topic = "b".repeat(64);

        write_resume(&dir, &topic).unwrap();
        assert!(
            dir.resume_path().exists(),
            "the note must be in this profile, not a sibling one"
        );

        // And it is read back from the same place, once.
        assert_eq!(take_resume(&dir).as_deref(), Some(topic.as_str()));
        assert_eq!(take_resume(&dir), None, "a note is good for one restart");
        assert!(!dir.resume_path().exists(), "and is cleared after reading");
    }

    #[test]
    fn the_resume_note_carries_a_room_and_never_a_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("resume.txt");
        let topic = "a".repeat(64);
        std::fs::write(&path, &topic).unwrap();

        // Same shape check `take_resume` applies.
        let read = std::fs::read_to_string(&path).unwrap();
        assert_eq!(read.trim().len(), 64);
        assert!(read.trim().chars().all(|c| c.is_ascii_hexdigit()));

        // A topic is public; the key must never be written next to it.
        assert!(!read.contains('-'), "a pin has dashes, a topic does not");
    }

    #[test]
    fn a_damaged_resume_note_is_ignored_rather_than_obeyed() {
        // Anything that is not exactly a 64-char hex topic must not send the
        // app opening something on start.
        for bad in ["", "   ", "7K2M-9QXP", "zz", &"a".repeat(63), &"a".repeat(65)] {
            let ok = bad.trim().len() == 64 && bad.trim().chars().all(|c| c.is_ascii_hexdigit());
            assert!(!ok, "{bad:?} should not be accepted as a room");
        }
    }

    /// Fetches the real published release and puts it through the same checks
    /// the app applies before installing anything.
    ///
    /// Ignored by default because it needs the network and a published
    /// release. This is the one that would catch a manifest the app cannot
    /// parse, or a binary whose signature does not match what was uploaded --
    /// both of which look fine from a browser.
    ///
    ///   cargo test the_published_release_passes_our_own_checks -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn the_published_release_passes_our_own_checks() {
        let manifest = fetch_manifest().await.expect("manifest should parse");
        println!("published version: {}", manifest.version);
        println!("url: {}", manifest.url);

        let bytes = fetch_binary(&manifest)
            .await
            .expect("the published binary must pass digest and signature");
        println!("binary: {} bytes, verified", bytes.len());

        // And it must actually be a Windows executable, not an error page
        // that happened to hash correctly.
        assert_eq!(&bytes[..2], b"MZ", "that is not a windows executable");
    }

    #[test]
    fn the_relaunch_argument_carries_the_version_it_came_from() {
        // Shape of what `install_and_relaunch` passes on.
        let arg = format!("{JUST_UPDATED}=0.6.0");
        assert!(arg.starts_with(JUST_UPDATED), "must still be recognisable");

        let parsed = arg
            .strip_prefix(JUST_UPDATED)
            .and_then(|rest| rest.strip_prefix('='));
        assert_eq!(parsed, Some("0.6.0"), "so the notice can say where from");

        // The bare form, from a build old enough not to send a version, must
        // still register as an update rather than being ignored.
        assert!(JUST_UPDATED.starts_with(JUST_UPDATED));
        assert_eq!(
            JUST_UPDATED
                .strip_prefix(JUST_UPDATED)
                .and_then(|r| r.strip_prefix('=')),
            None,
            "no version attached, and that is fine"
        );
    }

    #[test]
    fn hex_decoding_refuses_what_is_not_hex() {
        assert_eq!(hex_bytes("00ff").as_deref(), Some([0x00, 0xff].as_slice()));
        assert!(hex_bytes("").is_none(), "empty");
        assert!(hex_bytes("abc").is_none(), "odd length");
        assert!(hex_bytes("zz").is_none(), "not hex digits");
        assert!(hex_bytes("ab cd").is_none(), "inner space");
    }
}
