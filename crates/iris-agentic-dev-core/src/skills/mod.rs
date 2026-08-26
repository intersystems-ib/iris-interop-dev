use anyhow::{Context, Result};
use reqwest::Client;

/// Live GitHub raw-content base. Overridable per-call so tests can point at a mock
/// server (#92, mirroring `manifest::resolve::GITHUB_API` from #87).
const RAW_GITHUB: &str = "https://raw.githubusercontent.com";

pub struct SkillRegistry {
    skills: Vec<Skill>,
    kb_items: Vec<KbItem>,
}

pub struct Skill {
    pub name: String,
    pub description: String,
    pub content: String,
    pub source_repo: String,
}

pub struct KbItem {
    pub title: String,
    pub content: String,
    pub source_repo: String,
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: vec![],
            kb_items: vec![],
        }
    }

    pub fn list_skills(&self) -> &[Skill] {
        &self.skills
    }
    pub fn list_kb_items(&self) -> &[KbItem] {
        &self.kb_items
    }

    /// Subscribe to a GitHub package, loading its skills and KB items.
    pub async fn load_from_github(&mut self, owner_repo: &str) -> Result<()> {
        self.load_from_github_at(RAW_GITHUB, owner_repo).await
    }

    /// Same as [`load_from_github`](Self::load_from_github), against an explicit raw-content
    /// base URL. The tests drive this with a wiremock server so the subscription path is
    /// covered offline (#92, same seam as `manifest::resolve::resolve_github_version_at`).
    pub async fn load_from_github_at(&mut self, raw_base: &str, owner_repo: &str) -> Result<()> {
        let raw_base = raw_base.trim_end_matches('/');
        let client = Client::builder()
            .user_agent("iris-agentic-dev/0.3.1")
            .build()?;

        let (owner, repo) = owner_repo
            .split_once('/')
            .with_context(|| format!("invalid owner/repo: {}", owner_repo))?;

        let manifest = self
            .fetch_manifest_at(raw_base, owner, repo, &client)
            .await?;

        for skill_path in &manifest.provides_skills {
            let skill_md_url = format!(
                "{}/{}/{}/HEAD/{}/SKILL.md",
                raw_base, owner, repo, skill_path
            );
            if let Ok(content) = fetch_text(&skill_md_url, &client).await {
                if let Some(name) = extract_frontmatter_field(&content, "name") {
                    let description = extract_frontmatter_field(&content, "description")
                        .unwrap_or_else(|| name.clone());
                    self.skills.push(Skill {
                        name,
                        description,
                        content,
                        source_repo: owner_repo.to_string(),
                    });
                }
            }
        }

        for kb_path in &manifest.provides_kb_items {
            let kb_url = format!("{}/{}/{}/HEAD/{}", raw_base, owner, repo, kb_path);
            if let Ok(content) = fetch_text(&kb_url, &client).await {
                let title = extract_frontmatter_field(&content, "title")
                    .or_else(|| extract_h1_title(&content))
                    .unwrap_or_else(|| kb_path.clone());
                self.kb_items.push(KbItem {
                    title,
                    content,
                    source_repo: owner_repo.to_string(),
                });
            }
        }

        tracing::info!(
            "Loaded {} skills + {} KB items from {}",
            self.skills.len(),
            self.kb_items.len(),
            owner_repo
        );
        Ok(())
    }

    async fn fetch_manifest_at(
        &self,
        raw_base: &str,
        owner: &str,
        repo: &str,
        client: &Client,
    ) -> Result<Manifest> {
        let url = format!("{}/{}/{}/HEAD/iris-agentic-dev.toml", raw_base, owner, repo);
        let text = fetch_text(&url, client)
            .await
            .with_context(|| format!("no iris-agentic-dev.toml found in {}/{}", owner, repo))?;
        let manifest: TomlManifest = toml::from_str(&text)
            .with_context(|| format!("invalid iris-agentic-dev.toml in {}/{}", owner, repo))?;
        Ok(Manifest {
            provides_skills: manifest
                .provides
                .as_ref()
                .map(|p| p.skills.clone())
                .unwrap_or_default(),
            provides_kb_items: manifest
                .provides
                .as_ref()
                .map(|p| p.kb_items.clone())
                .unwrap_or_default(),
        })
    }
}

struct Manifest {
    provides_skills: Vec<String>,
    provides_kb_items: Vec<String>,
}

#[derive(serde::Deserialize)]
struct TomlManifest {
    provides: Option<TomlProvides>,
}

#[derive(serde::Deserialize)]
struct TomlProvides {
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    kb_items: Vec<String>,
}

async fn fetch_text(url: &str, client: &Client) -> Result<String> {
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("HTTP {} fetching {}", resp.status(), url);
    }
    Ok(resp.text().await?)
}

fn extract_frontmatter_field(content: &str, field: &str) -> Option<String> {
    let inside = content.strip_prefix("---")?.split("---").next()?;
    for line in inside.lines() {
        if let Some(val) = line.strip_prefix(&format!("{}:", field)) {
            return Some(val.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn extract_h1_title(content: &str) -> Option<String> {
    content
        .lines()
        .find(|l| l.starts_with("# "))
        .map(|l| l.trim_start_matches("# ").to_string())
}
