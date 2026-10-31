//! #543: an arm64 Linux host downloads the x64 asset, hash-verifies it, and dies at `exec`.
//!
//! THE DEFECT, precisely. The release publishes Linux for x64 only, and the README's Linux install
//! line names `iris-interop-dev-linux-x64` unconditionally — no `uname -m` anywhere, unlike the
//! macOS line right above it. So on an arm64 Linux host (Graviton, an arm64 runner, Docker
//! Desktop's arm64 Linux VM on an Apple-silicon Mac) the download succeeds, the SHA-256 GitHub
//! reports matches the bytes that arrived, and the first thing that fails is `exec`. A checksum
//! that passes on a binary for another instruction set verified nothing.
//!
//! This is the third instance of one shape: #174 (GLIBC floor) and #423 (macOS arm64-only) both
//! downloaded, hash-verified, and then failed at exec. #423 added a per-asset architecture guard,
//! but that guard proves an asset is built for the arch its NAME promises — it cannot report an
//! arch for which no asset exists at all. The repair for THAT is a named refusal at selection
//! time, which is what this file pins:
//!
//!   * `scripts/select-release-asset.sh` is the one place a host maps to an asset name, and it has
//!     three outcomes, not two — selected, no asset for this architecture (exit 3), unrecognised
//!     platform (exit 4). A failure is never answered with a plausible-looking asset name.
//!   * the README's Linux block is EXECUTED here against a stubbed `uname` and `curl`, so what is
//!     asserted is which URL an arm64 host actually fetches — not whether the prose mentions arm64.
//!   * the selector's catalogue and `release.yml`'s build matrix are compared in both directions,
//!     because the whole failure class is a published-asset set and an asset-selecting consumer
//!     drifting apart.
//!
//! WHY ITS OWN FILE: `crates/.../tests/*.rs` is auto-discovered and `scripts/ci-test-targets.sh`
//! derives the required gate from `cargo metadata`, so a new top-level file is in the gate with no
//! edit anywhere — and it collides with nothing, unlike an appended `#[cfg(test)] mod`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("workspace root must resolve")
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Run the selector. Returns the exit code even when non-zero: the refusals are the point, so a
/// helper that panicked on a non-zero status could not test them.
fn select(args: &[&str]) -> Run {
    let root = repo_root();
    let script = root.join("scripts").join("select-release-asset.sh");
    assert!(
        script.exists(),
        "{} does not exist — the host→asset mapping has no single implementation, which is the \
         defect #543 reports (an arm64 host silently gets the x64 asset)",
        script.display()
    );
    let out = Command::new("bash")
        .arg(&script)
        .args(args)
        .current_dir(&root)
        .output()
        .unwrap_or_else(|e| panic!("could not run {}: {e}", script.display()));
    Run {
        // `unwrap_or(-1)` would turn death-by-signal into a plausible number; -1 is not a status
        // any branch of the script returns, so it cannot be mistaken for one.
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn expect_asset(args: &[&str], want: &str) {
    let r = select(args);
    assert_eq!(
        r.code, 0,
        "select-release-asset.sh {args:?} exited {} — stdout {:?}, stderr {:?}",
        r.code, r.stdout, r.stderr
    );
    assert_eq!(
        r.stdout.trim(),
        want,
        "select-release-asset.sh {args:?} selected {:?}, expected {want:?}",
        r.stdout.trim()
    );
}

// ── selection by architecture ─────────────────────────────────────────────────────────────

/// The reported failure, stated as a selection: an arm64 Linux host must not be handed the x64
/// asset. Before #543 there was no selector at all and the README named `-linux-x64` flatly.
#[test]
fn an_arm64_linux_host_selects_the_arm64_asset() {
    expect_asset(&["Linux", "aarch64"], "iris-interop-dev-linux-arm64");
    // `uname -m` says `aarch64` on a Linux kernel and `arm64` on Darwin, and some arm64 Linux
    // userlands (Debian's arm64 port, Alpine) report `arm64` through containers. Both spellings
    // mean the same machine; recognising only one reintroduces the refusal-or-wrong-asset choice.
    expect_asset(&["Linux", "arm64"], "iris-interop-dev-linux-arm64");
}

#[test]
fn an_x64_linux_host_still_selects_the_x64_asset() {
    expect_asset(&["Linux", "x86_64"], "iris-interop-dev-linux-x64");
    expect_asset(&["Linux", "amd64"], "iris-interop-dev-linux-x64");
}

/// #174's static asset has to be selectable for BOTH Linux architectures, or an arm64 host on
/// AL2023/RHEL 8-9/Debian 12 lands back in the download-verify-fail-at-exec hole by a different
/// door (GLIBC floor instead of instruction set).
#[test]
fn the_musl_flavour_is_selectable_for_both_linux_architectures() {
    expect_asset(
        &["--musl", "Linux", "x86_64"],
        "iris-interop-dev-linux-x64-musl",
    );
    expect_asset(
        &["--musl", "Linux", "aarch64"],
        "iris-interop-dev-linux-arm64-musl",
    );
}

#[test]
fn macos_and_windows_hosts_select_their_own_assets() {
    expect_asset(&["Darwin", "arm64"], "iris-interop-dev-macos-arm64");
    expect_asset(&["Darwin", "x86_64"], "iris-interop-dev-macos-x64");
    // git-bash / msys report the kernel like this; a Windows host that is not recognised as
    // Windows would fall into the unknown-platform refusal and report a bug that is not there.
    expect_asset(
        &["MINGW64_NT-10.0-26100", "x86_64"],
        "iris-interop-dev-windows-x64.exe",
    );
    expect_asset(
        &["MSYS_NT-10.0", "x86_64"],
        "iris-interop-dev-windows-x64.exe",
    );
}

/// With no arguments the selector reads the host it is running on. The asserted thing is only that
/// it answers SOMETHING consistent with this machine, because the machine running the gate varies.
#[test]
fn with_no_arguments_it_reads_the_host_it_runs_on() {
    let r = select(&[]);
    assert_eq!(
        r.code, 0,
        "selecting for the host this test runs on failed: stdout {:?}, stderr {:?}",
        r.stdout, r.stderr
    );
    let want = if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "iris-interop-dev-macos-arm64"
        } else {
            "iris-interop-dev-macos-x64"
        }
    } else if cfg!(target_arch = "aarch64") {
        "iris-interop-dev-linux-arm64"
    } else {
        "iris-interop-dev-linux-x64"
    };
    assert_eq!(r.stdout.trim(), want);
}

// ── the refusals: a failure is not answered with an asset name ─────────────────────────────

/// THE house rule applied to this defect. An architecture with no published asset must come back
/// as a third outcome that SAYS SO, not as the nearest-looking asset and not as an empty string an
/// installer would paste into a URL.
#[test]
fn an_architecture_with_no_published_asset_is_refused_by_name() {
    for arch in ["riscv64", "ppc64le", "armv7l", "s390x"] {
        let r = select(&["Linux", arch]);
        assert_eq!(
            r.code, 3,
            "Linux/{arch} exited {} — a host architecture with no asset must be its own outcome \
             (3), not success and not a usage error. stdout {:?}, stderr {:?}",
            r.code, r.stdout, r.stderr
        );
        assert!(
            r.stdout.trim().is_empty(),
            "Linux/{arch} was refused but still printed {:?} on stdout — an installer reading \
             stdout would download that",
            r.stdout
        );
        assert!(
            r.stderr.contains(arch),
            "the refusal for Linux/{arch} does not name {arch}: {:?}",
            r.stderr
        );
        // A refusal that does not say what IS available is a dead end, and this repo requires a
        // next step. The architectures Linux publishes are the remedy.
        assert!(
            r.stderr.contains("x86_64") && r.stderr.contains("aarch64"),
            "the refusal for Linux/{arch} does not name the architectures Linux DOES publish: {:?}",
            r.stderr
        );
    }
}

/// An OS nothing is published for must not be guessed at. This is a DIFFERENT outcome from "known
/// OS, unpublished arch": conflating them hides which half of the pair is missing.
#[test]
fn an_unrecognised_platform_is_refused_rather_than_guessed() {
    for os in ["Plan9", "SunOS", "FreeBSD", ""] {
        let r = select(&[os, "x86_64"]);
        assert_eq!(
            r.code, 4,
            "{os:?}/x86_64 exited {} — an unrecognised platform is its own outcome (4). \
             stdout {:?}, stderr {:?}",
            r.code, r.stdout, r.stderr
        );
        assert!(
            r.stdout.trim().is_empty(),
            "{os:?}/x86_64 was refused but printed {:?} on stdout",
            r.stdout
        );
    }
}

/// `--musl` exists for exactly one platform. Asking for it elsewhere is a request for an asset
/// that is not published, so it takes the same named refusal rather than quietly handing back the
/// dynamically linked asset the caller just said it could not use.
#[test]
fn asking_for_musl_where_none_is_published_is_refused_not_downgraded() {
    for args in [
        ["--musl", "Darwin", "arm64"],
        ["--musl", "Darwin", "x86_64"],
    ] {
        let r = select(&args);
        assert_eq!(
            r.code, 3,
            "{args:?} exited {} — expected the no-such-asset refusal. stdout {:?}, stderr {:?}",
            r.code, r.stdout, r.stderr
        );
        assert!(
            r.stdout.trim().is_empty(),
            "{args:?} was refused but printed {:?} on stdout",
            r.stdout
        );
    }
}

/// CONTROL for the two tests above: the harness can distinguish 3 from 4 from 0, so a run where
/// everything came back 3 would not read as a pass. A malformed invocation is a fourth code again.
#[test]
fn a_usage_error_is_not_one_of_the_platform_refusals() {
    let r = select(&["Linux"]);
    assert_eq!(
        r.code, 2,
        "one bare argument should be a usage error (2), not a platform verdict. \
         stdout {:?}, stderr {:?}",
        r.stdout, r.stderr
    );
}

// ── the catalogue and the build matrix cannot drift ────────────────────────────────────────

/// Pull the `asset:` names out of release.yml's build matrix. The matrix is the only thing that
/// ties an asset NAME to a `--target`, so it is the authority on what exists.
fn matrix_assets() -> BTreeSet<String> {
    let path = repo_root().join(".github/workflows/release.yml");
    let yaml = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {} ({e}) — this guard must not pass by being unable to look",
            path.display()
        )
    });
    let found: BTreeSet<String> = yaml
        .lines()
        .filter_map(|l| l.trim().strip_prefix("asset:"))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect();
    // POSITIVE CONTROL on the parse. A changed indent or key spelling would otherwise yield an
    // empty set and "every matrix asset is selectable" would be vacuously true — the false green
    // this whole family of guards exists to refuse.
    assert!(
        found.len() >= 5,
        "parsed only {} asset names out of {} — the parse is broken, not the matrix: {found:?}",
        found.len(),
        path.display()
    );
    found
}

#[test]
fn every_published_asset_is_selectable_and_every_selectable_asset_is_published() {
    let matrix = matrix_assets();
    let r = select(&["--list"]);
    assert_eq!(
        r.code, 0,
        "`--list` exited {} — stderr {:?}",
        r.code, r.stderr
    );
    let listed: BTreeSet<String> = r
        .stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    assert!(
        !listed.is_empty(),
        "`--list` printed nothing, so the comparison below would pass without comparing anything"
    );

    let unselectable: Vec<&String> = matrix.difference(&listed).collect();
    assert!(
        unselectable.is_empty(),
        "release.yml publishes {unselectable:?} but the selector cannot return them — a host of \
         that architecture is sent to a different asset, which is #543"
    );
    let unpublished: Vec<&String> = listed.difference(&matrix).collect();
    assert!(
        unpublished.is_empty(),
        "the selector can return {unpublished:?} but release.yml publishes no such asset — the \
         download would 404, or worse, resolve to an older release's asset"
    );
}

/// Half 1 of #543, asserted on the matrix rather than on a cut release: Linux must publish an
/// arm64 row, and it must publish it for both libc flavours for the same reason x64 has both.
#[test]
fn the_release_matrix_publishes_linux_arm64() {
    let matrix = matrix_assets();
    for want in [
        "iris-interop-dev-linux-arm64",
        "iris-interop-dev-linux-arm64-musl",
    ] {
        assert!(
            matrix.contains(want),
            "release.yml's build matrix has no `{want}` asset — Linux still publishes x64 only, \
             so an arm64 host has nothing it can execute. Matrix: {matrix:?}"
        );
    }
}

// ── the README's Linux install block, executed ─────────────────────────────────────────────

const README_MARKER: &str = "<!-- install:linux";

/// Lift the fenced block that follows the declared marker. The marker exists so the extraction is
/// a contract rather than a grep someone can break by rewording a heading.
fn readme_linux_block() -> String {
    let path = repo_root().join("README.md");
    let readme = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {} ({e}) — this guard must not pass by being unable to look",
            path.display()
        )
    });
    let after_marker = readme
        .find(README_MARKER)
        .map(|i| &readme[i..])
        .unwrap_or_else(|| {
            panic!(
            "README.md has no `{README_MARKER} ...>` marker — the Linux install block cannot be \
             located, so nothing here is checked. If the install section moved, move the marker."
        )
        });
    let mut lines = after_marker.lines();
    // Walk to the opening fence, but not past the next heading: a marker that has drifted away
    // from its block must fail, not silently adopt whatever fence comes next.
    let mut saw_fence = false;
    for l in lines.by_ref() {
        if l.trim_start().starts_with("## ") {
            panic!("`{README_MARKER}` is followed by a heading before any ```bash fence");
        }
        if l.trim() == "```bash" {
            saw_fence = true;
            break;
        }
    }
    assert!(saw_fence, "no ```bash fence after `{README_MARKER}`");
    let mut body = Vec::new();
    let mut closed = false;
    for l in lines {
        if l.trim() == "```" {
            closed = true;
            break;
        }
        body.push(l);
    }
    assert!(closed, "the fence after `{README_MARKER}` is never closed");
    // SIZE CONTROL: print the window and refuse an implausible one. A truncated extraction that
    // happens to still run would test a fragment and report success for the whole block.
    eprintln!("extracted {} line(s) from README.md:", body.len());
    for l in &body {
        eprintln!("  | {l}");
    }
    assert!(
        body.len() >= 5,
        "extracted only {} line(s) — too short to be the install block",
        body.len()
    );
    let block = body.join("\n");
    assert!(
        block.contains("curl"),
        "the extracted block downloads nothing, so executing it proves nothing:\n{block}"
    );
    block
}

/// Execute the README's Linux block with `uname`, `curl` and `chmod` stubbed on PATH, and report
/// (exit code, every argument curl was given).
///
/// Stubbing `curl` is what makes this a behavioural test instead of a text match: the asserted
/// thing is the URL an arm64 host actually requests. Stubbing `chmod` keeps the block from touching
/// /usr/local/bin on the machine running the gate.
fn run_readme_block(arch: &str) -> (i32, String, String) {
    let dir = std::env::temp_dir().join(format!(
        "iid543-{arch}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let stub_dir = dir.join("stub");
    std::fs::create_dir_all(&stub_dir).expect("stub dir");
    let log = dir.join("curl.log");

    let write_stub = |name: &str, body: &str| {
        let p = stub_dir.join(name);
        std::fs::write(&p, body).expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
    };
    write_stub(
        "uname",
        &format!("#!/bin/sh\ncase \"$1\" in\n  -m) echo '{arch}' ;;\n  *) echo Linux ;;\nesac\n"),
    );
    write_stub(
        "curl",
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\nexit 0\n",
            log.display()
        ),
    );
    write_stub("chmod", "#!/bin/sh\nexit 0\n");

    let script = dir.join("install.sh");
    std::fs::write(&script, readme_linux_block()).expect("write block");

    let path = format!(
        "{}:{}",
        stub_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new("bash")
        .arg(&script)
        .env("PATH", path)
        .output()
        .expect("run the README block");
    let curl_args = std::fs::read_to_string(&log).unwrap_or_default();
    (
        out.status.code().unwrap_or(-1),
        curl_args,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// #543 AS REPORTED, executed. Today this block names `-linux-x64` with no `uname -m` at all, so
/// an arm64 host downloads the x64 binary, the digest matches, and `exec` is the first thing that
/// fails.
#[test]
fn the_readme_linux_block_does_not_hand_an_arm64_host_the_x64_asset() {
    let (code, curl_args, stderr) = run_readme_block("aarch64");
    assert_eq!(
        code, 0,
        "the README's Linux block failed on an arm64 host: stderr {stderr:?}"
    );
    assert!(
        curl_args.contains("iris-interop-dev-linux-arm64"),
        "on aarch64 the README block requested:\n{curl_args}\n— expected the \
         `iris-interop-dev-linux-arm64` asset"
    );
    assert!(
        !curl_args.contains("iris-interop-dev-linux-x64"),
        "on aarch64 the README block downloaded the x64 asset. It hash-verifies and then fails at \
         exec, which is the whole of #543. Requested:\n{curl_args}"
    );
}

#[test]
fn the_readme_linux_block_still_serves_an_x64_host() {
    let (code, curl_args, stderr) = run_readme_block("x86_64");
    assert_eq!(code, 0, "x86_64 host: stderr {stderr:?}");
    assert!(
        curl_args.contains("iris-interop-dev-linux-x64"),
        "on x86_64 the README block requested:\n{curl_args}"
    );
    assert!(
        !curl_args.contains("arm64"),
        "on x86_64 the README block requested an arm64 asset:\n{curl_args}"
    );
}

/// THE ANTI-DRIFT BRIDGE. The README block and `select-release-asset.sh` are two implementations
/// of one decision — unavoidably, because a shell installer cannot call a script it has not cloned
/// and cannot call the binary it is about to download. So the two are compared by BEHAVIOUR, per
/// architecture, instead of being trusted to stay in step: teaching one about a new asset and not
/// the other is red here.
#[test]
fn the_readme_block_and_the_selector_agree_on_every_linux_architecture() {
    for arch in ["x86_64", "aarch64", "arm64", "amd64"] {
        let r = select(&["Linux", arch]);
        assert_eq!(r.code, 0, "selector refused Linux/{arch}: {:?}", r.stderr);
        let want = r.stdout.trim().to_string();
        let (code, curl_args, stderr) = run_readme_block(arch);
        assert_eq!(code, 0, "README block failed on {arch}: {stderr:?}");
        let url_line = curl_args
            .lines()
            .find(|l| l.contains("releases/latest/download/"))
            .unwrap_or_else(|| {
                panic!("README block issued no download URL on {arch}:\n{curl_args}")
            });
        assert!(
            url_line.ends_with(&want),
            "on {arch} the README block fetches {url_line:?} but select-release-asset.sh says the \
             asset is {want:?} — the two have drifted, and the README is the one users follow"
        );
    }
}

/// The refusal, at the only moment it can still help: BEFORE the download. An architecture with no
/// asset must stop here with a non-zero status and no request issued — not fetch something that
/// will verify and then fail to run.
#[test]
fn the_readme_linux_block_refuses_an_architecture_with_no_asset() {
    let (code, curl_args, stderr) = run_readme_block("riscv64");
    assert_ne!(
        code, 0,
        "the README block exited 0 on an architecture with no published asset. \
         curl args:\n{curl_args}"
    );
    assert!(
        curl_args.trim().is_empty(),
        "the README block downloaded something for riscv64 — a wrong-architecture binary that \
         hash-verifies is the defect, so nothing may be fetched at all. curl args:\n{curl_args}"
    );
    assert!(
        stderr.contains("riscv64"),
        "the refusal does not name the architecture it refused: {stderr:?}"
    );
}
