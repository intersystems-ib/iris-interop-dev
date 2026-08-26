use crate::manifest::schema::{DependencySpec, Manifest};
use anyhow::{anyhow, Result};
use semver::{Version, VersionReq};
use std::collections::HashSet;

pub struct Resolve {
    pub packages: Vec<ResolvedPackage>,
}

pub struct ResolvedPackage {
    pub name: String,
    pub version: Version,
    pub source: ResolvedSource,
}

#[derive(Debug, Clone)]
pub enum ResolvedSource {
    Local(std::path::PathBuf),
    Git(String),
    GitHub { owner: String, repo: String },
    OpenExchange(String),
}

impl Resolve {
    pub fn from_manifest(manifest: &Manifest) -> Result<Self> {
        let mut packages = vec![];
        let mut seen: HashSet<String> = HashSet::new();

        for (name, dep) in &manifest.dependencies {
            if seen.contains(name) {
                continue;
            }
            seen.insert(name.clone());

            let version_req = VersionReq::parse(&dep.version).map_err(|e| {
                anyhow!("invalid semver '{}' for dep '{}': {}", dep.version, name, e)
            })?;

            let source = dep_to_source(name, dep)?;
            let version = resolve_version(&version_req, &source)?;

            packages.push(ResolvedPackage {
                name: name.clone(),
                version,
                source,
            });
        }

        Ok(Self { packages })
    }

    pub fn to_lock(&self) -> ResolveLock {
        ResolveLock {
            packages: self
                .packages
                .iter()
                .map(|p| {
                    // Bug 11: format repository as a proper URL string, not Rust Debug output.
                    let repository = match &p.source {
                        ResolvedSource::GitHub { owner, repo } => {
                            format!("https://github.com/{}/{}", owner, repo)
                        }
                        ResolvedSource::Git(url) => url.clone(),
                        ResolvedSource::Local(path) => path.to_string_lossy().into_owned(),
                        ResolvedSource::OpenExchange(id) => {
                            format!("openexchange:{}", id)
                        }
                    };
                    PackageLock {
                        name: p.name.clone(),
                        version: p.version.to_string(),
                        repository,
                        checksum: None,
                    }
                })
                .collect(),
        }
    }
}

fn dep_to_source(name: &str, dep: &DependencySpec) -> Result<ResolvedSource> {
    if let Some(github) = &dep.github {
        let parts: Vec<_> = github.splitn(2, '/').collect();
        if parts.len() == 2 {
            return Ok(ResolvedSource::GitHub {
                owner: parts[0].to_string(),
                repo: parts[1].to_string(),
            });
        }
    }
    if let Some(git) = &dep.git {
        return Ok(ResolvedSource::Git(git.clone()));
    }
    if let Some(repo) = &dep.repository {
        return Ok(ResolvedSource::Local(std::path::PathBuf::from(repo)));
    }
    if let Some(ox) = &dep.openexchange {
        return Ok(ResolvedSource::OpenExchange(ox.clone()));
    }
    Err(anyhow!(
        "dependency '{}' has no source (git, github, repository, or openexchange)",
        name
    ))
}

fn resolve_version(req: &VersionReq, source: &ResolvedSource) -> Result<Version> {
    // Sync wrapper — spins up a tokio runtime for the async GitHub fetch.
    // Called from Resolve::from_manifest which is sync.
    match source {
        ResolvedSource::GitHub { .. } => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(resolve_github_version_async(req, source))
        }
        ResolvedSource::Local(path) => {
            // Read version from a local iris-agentic-dev.toml or Cargo.toml
            let manifest_path = path.join("iris-agentic-dev.toml");
            if manifest_path.exists() {
                let content = std::fs::read_to_string(&manifest_path)?;
                let parsed: toml::Value = toml::from_str(&content)?;
                let v_str = parsed
                    .get("package")
                    .and_then(|p| p.get("version"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("no [package].version in {:?}", manifest_path))?;
                let v = Version::parse(v_str)?;
                if req.matches(&v) {
                    return Ok(v);
                }
                anyhow::bail!("local version {} does not satisfy {}", v, req);
            }
            anyhow::bail!("local source {:?} has no iris-agentic-dev.toml", path)
        }
        _ => anyhow::bail!(
            "version resolution not yet implemented for source {:?} (requirement: {})",
            source,
            req
        ),
    }
}

/// Live GitHub REST base. Overridable per-call so tests can point at a mock server (#87).
const GITHUB_API: &str = "https://api.github.com";

/// Fetch GitHub tags and return the highest version satisfying `req`.
/// Exported for use in async tests.
pub async fn resolve_github_version_async(
    req: &VersionReq,
    source: &ResolvedSource,
) -> Result<Version> {
    resolve_github_version_at(GITHUB_API, req, source).await
}

/// Same as [`resolve_github_version_async`], against an explicit API base URL.
/// The tests drive this with a wiremock server so tag selection is covered offline (#87).
pub async fn resolve_github_version_at(
    api_base: &str,
    req: &VersionReq,
    source: &ResolvedSource,
) -> Result<Version> {
    let (owner, repo) = match source {
        ResolvedSource::GitHub { owner, repo } => (owner.as_str(), repo.as_str()),
        _ => anyhow::bail!("resolve_github_version_async called with non-GitHub source"),
    };

    let url = format!(
        "{}/repos/{}/{}/tags?per_page=100",
        api_base.trim_end_matches('/'),
        owner,
        repo
    );
    let client = reqwest::Client::builder()
        .user_agent("iris-agentic-dev/resolver")
        .build()?;

    // #87: unauthenticated GitHub allows 60 requests/hour PER IP, shared by every job on a
    // runner. A token lifts that to 1000/hr per repo (the token Actions injects) or 5000/hr
    // (a PAT). The value is only ever put in a header — never logged, never in an error.
    let token = std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .ok()
        .filter(|t| !t.trim().is_empty());

    let mut request = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(token) = &token {
        request = request.bearer_auth(token);
    }

    let resp = request.send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("GitHub repo {}/{} not found", owner, repo);
    }
    // #87: say "rate limit" out loud. A bare "GitHub API returned 403" reads like a
    // permissions bug and cost a full red gate during the #78/#119 review.
    let rate_limited = matches!(
        resp.status(),
        reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::TOO_MANY_REQUESTS
    ) && resp
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        == Some("0");
    if rate_limited {
        anyhow::bail!(
            "GitHub API rate limit exceeded while resolving {}/{} ({} request). \
             Set GITHUB_TOKEN to raise the 60 requests/hour that unauthenticated \
             callers share per IP.",
            owner,
            repo,
            if token.is_some() {
                "authenticated"
            } else {
                "unauthenticated"
            }
        );
    }
    if !resp.status().is_success() {
        anyhow::bail!(
            "GitHub API returned {} for {}/{}",
            resp.status(),
            owner,
            repo
        );
    }

    let tags: serde_json::Value = resp.json().await?;
    let tag_array = tags
        .as_array()
        .ok_or_else(|| anyhow!("unexpected GitHub tags response"))?;

    let mut candidates: Vec<Version> = tag_array
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .filter_map(|name| {
            // Accept "v1.2.3" and "1.2.3" tag formats
            let stripped = name.strip_prefix('v').unwrap_or(name);
            Version::parse(stripped).ok()
        })
        .filter(|v| req.matches(v))
        .collect();

    if candidates.is_empty() {
        anyhow::bail!(
            "no tags in {}/{} satisfy version requirement {}",
            owner,
            repo,
            req
        );
    }

    candidates.sort();
    Ok(candidates.into_iter().last().unwrap())
}

pub struct ResolveLock {
    pub packages: Vec<PackageLock>,
}

pub struct PackageLock {
    pub name: String,
    pub version: String,
    pub repository: String,
    pub checksum: Option<String>,
}

impl ResolveLock {
    pub fn to_toml(&self) -> String {
        let mut out = String::from("[metadata]\nformat-version = 1\n\n");
        for pkg in &self.packages {
            // Bug 11: use proper TOML string quoting, not Rust Debug format ({:?}).
            out.push_str(&format!(
                "[[package]]\nname = \"{}\"\nversion = \"{}\"\nrepository = \"{}\"\n\n",
                pkg.name.replace('\\', "\\\\").replace('"', "\\\""),
                pkg.version.replace('\\', "\\\\").replace('"', "\\\""),
                pkg.repository.replace('\\', "\\\\").replace('"', "\\\""),
            ));
        }
        out
    }
}
