//! Skill tool - load, list, reload, and read skills

use super::{Tool, ToolContext, ToolOutput};
use crate::skill::SkillRegistry;
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct SkillTool {
    registry: Arc<RwLock<SkillRegistry>>,
}

impl SkillTool {
    pub fn new(registry: Arc<RwLock<SkillRegistry>>) -> Self {
        Self { registry }
    }

    /// Effective skill set for this call: shared global registry plus the
    /// session's project-local overlay resolved from the tool context working
    /// dir (issue #457). The overlay is read fresh from disk so edits are
    /// visible without daemon restarts and never enter the shared registry.
    async fn effective_registry(&self, working_dir: Option<&std::path::Path>) -> SkillRegistry {
        let global = self.registry.read().await;
        SkillRegistry::effective_for_working_dir(&global, working_dir)
    }
}

#[derive(Deserialize)]
struct SkillInput {
    /// Action to perform: load (default), list, reload, reload_all, read,
    /// create, update, delete. `list` shows both loaded skills and the
    /// jcode-endorsed catalog. `create`/`update`/`delete` manage agent-authored
    /// skills under ~/.jcode/skills.
    #[serde(default = "default_action")]
    action: String,
    /// Skill name (required for load, reload, read, create, update, delete)
    #[serde(alias = "skill")]
    #[serde(default)]
    name: Option<String>,
    /// One-line description (required for create/update).
    #[serde(default)]
    description: Option<String>,
    /// Skill body/instructions in Markdown (required for create/update).
    #[serde(default)]
    body: Option<String>,
    /// Optional Claude-compatible Skill wrapper argument. The skill loader only
    /// needs to load the prompt, so args are currently accepted and ignored.
    #[serde(default)]
    args: Option<String>,
}

fn default_action() -> String {
    "load".to_string()
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill_manage"
    }

    fn description(&self) -> &str {
        "Manage skills."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
                "action": {
                    "type": "string",
                    "enum": ["load", "list", "reload", "reload_all", "read", "create", "update", "delete"],
                    "description": "Action."
                },
                "name": {
                    "type": "string",
                    "description": "Skill name."
                },
                "description": {
                    "type": "string",
                    "description": "One-line skill description (required for create/update)."
                },
                "body": {
                    "type": "string",
                    "description": "Skill body/instructions in Markdown (required for create/update)."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: SkillInput = serde_json::from_value(input)?;
        let action_label = params.action.clone();
        let name_label = params.name.clone().unwrap_or_else(|| "<none>".to_string());
        let _args = params.args.as_deref();

        match params.action.as_str() {
            "load" => {
                self.load_skill(params.name, ctx.working_dir.as_deref())
                    .await
            }
            "list" => self.list_skills(ctx.working_dir.as_deref()).await,
            "reload" => self.reload_skill(params.name).await,
            "reload_all" => self.reload_all_skills(ctx.working_dir.as_deref()).await,
            "read" => {
                self.read_skill(params.name, ctx.working_dir.as_deref())
                    .await
            }
            "create" => {
                self.create_skill(
                    params.name,
                    params.description,
                    params.body,
                    ctx.working_dir.as_deref(),
                )
                .await
            }
            "update" => {
                self.update_skill(params.name, params.description, params.body)
                    .await
            }
            "delete" => self.delete_skill(params.name).await,
            _ => Ok(ToolOutput::new(format!(
                "Unknown action: {}. Use 'load', 'list', 'reload', 'reload_all', 'read', 'create', 'update', or 'delete'.",
                params.action
            ))),
        }
        .map_err(|err| {
            crate::logging::warn(&format!(
                "[tool:skill_manage] action failed action={} skill={} session_id={} error={}",
                action_label, name_label, ctx.session_id, err
            ));
            err
        })
    }
}

impl SkillTool {
    async fn load_skill(
        &self,
        name: Option<String>,
        working_dir: Option<&std::path::Path>,
    ) -> Result<ToolOutput> {
        let name = normalize_skill_name(name, "load")?;

        let registry = self.effective_registry(working_dir).await;
        let skill = registry.get(&name).ok_or_else(|| {
            // Endorsed skills are advertised in `list` but are not bundled;
            // a bare "not found" here reads like a bug (issue #445). Point at
            // the actual install command instead.
            if let Some(endorsed) = crate::skill::endorsed_skills()
                .iter()
                .find(|endorsed| endorsed.name == name)
            {
                match endorsed.install {
                    Some(install) => anyhow::anyhow!(
                        "Skill '{}' is endorsed but not installed. Install it with `{}`, then run skill_manage reload_all.",
                        name,
                        install
                    ),
                    None => anyhow::anyhow!(
                        "Skill '{}' is endorsed but not installed (source: {}). Install it into ~/.jcode/skills/{}/SKILL.md, then run skill_manage reload_all.",
                        name,
                        endorsed.source,
                        name
                    ),
                }
            } else {
                anyhow::anyhow!("Skill '{}' not found", name)
            }
        })?;

        let base_dir = skill
            .path
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| ".".to_string());

        Ok(ToolOutput::new(format!(
            "## Skill: {}\n\n**Base directory**: {}\n\n{}",
            skill.name,
            base_dir,
            skill.get_prompt()
        ))
        .with_title(format!("skill: {}", skill.name)))
    }

    async fn list_skills(&self, working_dir: Option<&std::path::Path>) -> Result<ToolOutput> {
        let registry = self.effective_registry(working_dir).await;
        let mut skills = registry.list();
        skills.sort_by(|a, b| a.name.cmp(&b.name));

        let installed: std::collections::HashSet<&str> =
            skills.iter().map(|s| s.name.as_str()).collect();

        let mut output = if skills.is_empty() {
            "No skills loaded.\n\n\
            Skills are loaded from:\n\
            - ~/.jcode/skills/<skill-name>/SKILL.md (global)\n\
            - ./.jcode/skills/<skill-name>/SKILL.md (project-local)\n\
            - ./.claude/skills/<skill-name>/SKILL.md (compatibility)\n\n\
            Create a SKILL.md file with YAML frontmatter:\n\
            ---\n\
            name: my-skill\n\
            description: What this skill does\n\
            allowed-tools: bash, read, write\n\
            ---\n\n\
            # Skill content here\n"
                .to_string()
        } else {
            let mut output = format!("Loaded skills: {}\n\n", skills.len());
            for skill in &skills {
                output.push_str(&format!("## /{}\n", skill.name));
                output.push_str(&format!("  {}\n", skill.description));
                output.push_str(&format!("  Path: {}\n", skill.path.display()));
                if let Some(ref tools) = skill.allowed_tools {
                    output.push_str(&format!("  Tools: {}\n", tools.join(", ")));
                }
                output.push('\n');
            }
            output
        };

        append_endorsed_skills(&mut output, &installed);

        Ok(ToolOutput::new(output).with_title("Skills: List"))
    }

    async fn reload_skill(&self, name: Option<String>) -> Result<ToolOutput> {
        let name = normalize_skill_name(name, "reload")?;

        let mut registry = self.registry.write().await;

        match registry.reload(&name) {
            Ok(true) => {
                // Re-read to get updated info
                if let Some(skill) = registry.get(&name) {
                    Ok(ToolOutput::new(format!(
                        "Reloaded skill '{}'\n\nDescription: {}\nPath: {}",
                        name,
                        skill.description,
                        skill.path.display()
                    ))
                    .with_title(format!("Skills: Reloaded {}", name)))
                } else {
                    Ok(ToolOutput::new(format!("Reloaded skill '{}'", name))
                        .with_title(format!("Skills: Reloaded {}", name)))
                }
            }
            Ok(false) => Ok(ToolOutput::new(format!(
                "Skill '{}' not found or was deleted.\n\nUse 'list' to see available skills.",
                name
            ))
            .with_title("Skills: Not found")),
            Err(e) => {
                crate::logging::warn(&format!(
                    "[tool:skill_manage] reload failed skill={} error={}",
                    name, e
                ));
                Ok(
                    ToolOutput::new(format!("Failed to reload skill '{}': {}", name, e))
                        .with_title("Skills: Reload failed"),
                )
            }
        }
    }

    async fn reload_all_skills(&self, working_dir: Option<&std::path::Path>) -> Result<ToolOutput> {
        // Reload the shared GLOBAL registry only; the project-local overlay is
        // session-scoped and re-read from disk on every access, so reloading
        // it here would leak this session's project skills to other sessions
        // (issue #457).
        let reloaded = {
            let mut registry = self.registry.write().await;
            registry.reload_global()
        };

        match reloaded {
            Ok(global_count) => {
                let effective = self.effective_registry(working_dir).await;
                let skills = effective.list();
                let mut output = format!(
                    "Reloaded {} global skills ({} effective for this session)\n\n",
                    global_count,
                    skills.len()
                );

                for skill in skills {
                    output.push_str(&format!("- /{}: {}\n", skill.name, skill.description));
                }

                Ok(
                    ToolOutput::new(output)
                        .with_title(format!("Skills: Reloaded {}", global_count)),
                )
            }
            Err(e) => {
                crate::logging::warn(&format!(
                    "[tool:skill_manage] reload_all failed error={}",
                    e
                ));
                Ok(ToolOutput::new(format!("Failed to reload skills: {}", e))
                    .with_title("Skills: Reload failed"))
            }
        }
    }

    async fn read_skill(
        &self,
        name: Option<String>,
        working_dir: Option<&std::path::Path>,
    ) -> Result<ToolOutput> {
        let name = normalize_skill_name(name, "read")?;

        let registry = self.effective_registry(working_dir).await;

        if let Some(skill) = registry.get(&name) {
            let mut output = format!("# Skill: {}\n\n", skill.name);
            output.push_str(&format!("**Description:** {}\n", skill.description));
            output.push_str(&format!("**Path:** {}\n", skill.path.display()));
            if let Some(ref tools) = skill.allowed_tools {
                output.push_str(&format!("**Allowed tools:** {}\n", tools.join(", ")));
            }
            output.push_str("\n---\n\n");
            output.push_str(&skill.content);

            Ok(ToolOutput::new(output).with_title(format!("Skills: {}", name)))
        } else {
            Ok(ToolOutput::new(format!(
                "Skill '{}' not found.\n\nUse 'list' to see available skills.",
                name
            ))
            .with_title("Skills: Not found"))
        }
    }

    /// Managed skills live under the global `~/.jcode/skills/<name>/SKILL.md`.
    fn managed_skill_file(name: &str) -> Result<std::path::PathBuf> {
        Ok(crate::storage::jcode_dir()?
            .join("skills")
            .join(name)
            .join("SKILL.md"))
    }

    /// Reload the shared global registry after a create/update/delete so the
    /// change is immediately loadable (same reload as `reload_all`).
    async fn reload_registry_after_mutation(&self) {
        let mut registry = self.registry.write().await;
        if let Err(e) = registry.reload_global() {
            crate::logging::warn(&format!(
                "[tool:skill_manage] post-mutation reload failed error={}",
                e
            ));
        }
    }

    async fn create_skill(
        &self,
        name: Option<String>,
        description: Option<String>,
        body: Option<String>,
        working_dir: Option<&std::path::Path>,
    ) -> Result<ToolOutput> {
        let name = validate_managed_skill_name(name, "create")?;
        let description = require_field(description, "description", "create")?;
        let body = require_field(body, "body", "create")?;
        let managed_file = Self::managed_skill_file(&name)?;

        // Refuse to clobber a skill already loaded from a different source, or a
        // hand-authored (non-managed) skill at the managed path.
        let registry = self.effective_registry(working_dir).await;
        if let Some(existing) = registry.get(&name) {
            if existing.path != managed_file {
                anyhow::bail!(
                    "A skill named '{}' is already loaded from {}. Pick a different name, or remove that skill first.",
                    name,
                    existing.path.display()
                );
            }
            if !crate::skill::is_managed_skill_file(&existing.path) {
                anyhow::bail!(
                    "A non-managed skill '{}' already exists at {}; refusing to overwrite a hand-authored skill.",
                    name,
                    existing.path.display()
                );
            }
            anyhow::bail!(
                "Managed skill '{}' already exists. Use action=update to change it.",
                name
            );
        }
        // Guard against an on-disk file not yet loaded into the registry.
        if managed_file.exists() && !crate::skill::is_managed_skill_file(&managed_file) {
            anyhow::bail!(
                "A non-managed skill file already exists at {}; refusing to overwrite it.",
                managed_file.display()
            );
        }

        if let Some(dir) = managed_file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(
            &managed_file,
            crate::skill::managed_skill_document(&name, &description, &body),
        )?;
        self.reload_registry_after_mutation().await;

        Ok(ToolOutput::new(format!(
            "Created managed skill '{}' at {}.\nIt is now loadable with skill_manage (action=load).",
            name,
            managed_file.display()
        ))
        .with_title(format!("Skills: Created {}", name)))
    }

    async fn update_skill(
        &self,
        name: Option<String>,
        description: Option<String>,
        body: Option<String>,
    ) -> Result<ToolOutput> {
        let name = validate_managed_skill_name(name, "update")?;
        let description = require_field(description, "description", "update")?;
        let body = require_field(body, "body", "update")?;
        let managed_file = Self::managed_skill_file(&name)?;

        if !managed_file.exists() {
            anyhow::bail!(
                "No managed skill '{}' to update. Create it first with action=create.",
                name
            );
        }
        if !crate::skill::is_managed_skill_file(&managed_file) {
            anyhow::bail!(
                "Skill '{}' is not managed (missing 'managed: true'); refusing to modify a hand-authored skill.",
                name
            );
        }

        std::fs::write(
            &managed_file,
            crate::skill::managed_skill_document(&name, &description, &body),
        )?;
        self.reload_registry_after_mutation().await;

        Ok(
            ToolOutput::new(format!("Updated managed skill '{}'.", name))
                .with_title(format!("Skills: Updated {}", name)),
        )
    }

    async fn delete_skill(&self, name: Option<String>) -> Result<ToolOutput> {
        let name = validate_managed_skill_name(name, "delete")?;
        let managed_file = Self::managed_skill_file(&name)?;

        if !managed_file.exists() {
            anyhow::bail!("No managed skill '{}' to delete.", name);
        }
        if !crate::skill::is_managed_skill_file(&managed_file) {
            anyhow::bail!(
                "Skill '{}' is not managed; refusing to delete a hand-authored skill.",
                name
            );
        }

        // Remove the whole `<name>/` skill directory, not just SKILL.md.
        let skill_dir = managed_file.parent().unwrap_or(&managed_file);
        std::fs::remove_dir_all(skill_dir)?;
        self.reload_registry_after_mutation().await;

        Ok(
            ToolOutput::new(format!("Deleted managed skill '{}'.", name))
                .with_title(format!("Skills: Deleted {}", name)),
        )
    }
}

/// Append the curated jcode-endorsed skill catalog to `output`, grouped by
/// category and marked with installed/not-installed status. `installed` is the
/// set of skill names currently loaded in the registry.
fn append_endorsed_skills(output: &mut String, installed: &std::collections::HashSet<&str>) {
    let endorsed = crate::skill::endorsed_skills();
    if endorsed.is_empty() {
        return;
    }

    output.push_str("\nEndorsed skills (recommended by jcode)\n");

    // Group by category, preserving first-seen order.
    let mut category_order: Vec<&str> = Vec::new();
    for skill in endorsed {
        if !category_order.contains(&skill.category) {
            category_order.push(skill.category);
        }
    }

    for category in category_order {
        let in_category: Vec<_> = endorsed.iter().filter(|e| e.category == category).collect();
        let installed_count = in_category
            .iter()
            .filter(|e| installed.contains(e.name))
            .count();
        output.push_str(&format!(
            "\n  {} ({}/{} installed)\n",
            category,
            installed_count,
            in_category.len()
        ));
        for skill in in_category {
            let is_installed = installed.contains(skill.name);
            let status = if is_installed {
                "installed"
            } else {
                "not installed"
            };
            output.push_str(&format!("  - /{} [{}]\n", skill.name, status));
            output.push_str(&format!("      {}\n", skill.description));
            output.push_str(&format!("      source: {}\n", skill.source));
            if !is_installed && let Some(install) = skill.install {
                output.push_str(&format!("      install: {}\n", install));
            }
        }
    }

    output.push_str(
        "\nActivate a loaded skill by loading it with skill_manage (action=load) or typing its slash command.\n",
    );
    output.push_str(
        "NVIDIA CUDA-X skills come from the official catalog at https://github.com/NVIDIA/skills.\n",
    );
}

fn normalize_skill_name(name: Option<String>, action: &str) -> Result<String> {
    let name = name.ok_or_else(|| anyhow::anyhow!("'name' is required for {} action", action))?;
    let trimmed = name.trim().trim_start_matches('/').to_string();
    if trimmed.is_empty() {
        anyhow::bail!("'name' is required for {} action", action);
    }
    Ok(trimmed)
}

/// Validate a skill name that will become a directory under ~/.jcode/skills.
/// The allowlist (letters, digits, '-', '_') rejects path separators, '..',
/// and leading dots, so a managed name can never escape the skills root.
fn validate_managed_skill_name(name: Option<String>, action: &str) -> Result<String> {
    let name = normalize_skill_name(name, action)?;
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        anyhow::bail!(
            "Invalid skill name '{}'. Use letters, digits, '-' or '_' only.",
            name
        );
    }
    Ok(name)
}

fn require_field(value: Option<String>, field: &str, action: &str) -> Result<String> {
    let value =
        value.ok_or_else(|| anyhow::anyhow!("'{}' is required for {} action", field, action))?;
    if value.trim().is_empty() {
        anyhow::bail!("'{}' must not be empty for {} action", field, action);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_tool() -> SkillTool {
        let registry = Arc::new(RwLock::new(SkillRegistry::default()));
        SkillTool::new(registry)
    }

    fn create_test_tool_with_skill(name: &str) -> (SkillTool, tempfile::TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        let skill_dir = temp_dir.path().join(".jcode").join("skills").join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: Test skill\n---\n\n# Test Skill\n\nUse this test skill."
            ),
        )
        .unwrap();

        let registry = SkillRegistry::load_for_working_dir(Some(temp_dir.path())).unwrap();
        let tool = SkillTool::new(Arc::new(RwLock::new(registry)));
        (tool, temp_dir)
    }

    fn create_test_context() -> ToolContext {
        ToolContext {
            session_id: "test-session".to_string(),
            message_id: "test-message".to_string(),
            tool_call_id: "test-tool-call".to_string(),
            working_dir: None,
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: crate::tool::ToolExecutionMode::Direct,
        }
    }

    #[test]
    fn test_tool_name() {
        let tool = create_test_tool();
        assert_eq!(tool.name(), "skill_manage");
    }

    #[test]
    fn test_tool_description() {
        let tool = create_test_tool();
        assert!(tool.description().contains("skill"));
    }

    #[test]
    fn test_parameters_schema() {
        let tool = create_test_tool();
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["action"].is_object());
        assert!(schema["properties"]["name"].is_object());
    }

    #[tokio::test]
    async fn test_list_empty() {
        let tool = create_test_tool();
        let ctx = create_test_context();
        let input = json!({"action": "list"});

        let result = tool.execute(input, ctx).await.unwrap();
        assert!(result.output.contains("No skills loaded"));
        // Even with no skills loaded, the endorsed catalog should be listed.
        assert!(result.output.contains("Endorsed skills"));
    }

    #[tokio::test]
    async fn test_list_includes_endorsed_skills() {
        let tool = create_test_tool();
        let ctx = create_test_context();
        let input = json!({"action": "list"});

        let result = tool.execute(input, ctx).await.unwrap();
        // Every endorsed skill should appear with an install-status marker.
        for endorsed in crate::skill::endorsed_skills() {
            assert!(
                result.output.contains(&format!("/{}", endorsed.name)),
                "expected endorsed skill /{} in:\n{}",
                endorsed.name,
                result.output
            );
        }
        // No skills are loaded in this tool, so they should be "not installed".
        assert!(result.output.contains("[not installed]"));
    }

    #[tokio::test]
    async fn test_load_missing_name() {
        let tool = create_test_tool();
        let ctx = create_test_context();
        let input = json!({"action": "load"});

        let result = tool.execute(input, ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("name"));
    }

    #[tokio::test]
    async fn test_load_accepts_skill_alias_and_args() {
        let (tool, _temp_dir) = create_test_tool_with_skill("optimization");
        let ctx = create_test_context();
        let input = json!({"skill": "optimization", "args": "optimize this"});

        let result = tool.execute(input, ctx).await.unwrap();
        assert!(result.output.contains("## Skill: optimization"));
        assert_eq!(result.title.as_deref(), Some("skill: optimization"));
    }

    #[tokio::test]
    async fn test_load_strips_leading_slash_from_name() {
        let (tool, _temp_dir) = create_test_tool_with_skill("optimization");
        let ctx = create_test_context();
        let input = json!({"action": "load", "name": "/optimization"});

        let result = tool.execute(input, ctx).await.unwrap();
        assert!(result.output.contains("## Skill: optimization"));
    }

    #[tokio::test]
    async fn test_reload_missing_name() {
        let tool = create_test_tool();
        let ctx = create_test_context();
        let input = json!({"action": "reload"});

        let result = tool.execute(input, ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("name"));
    }

    #[tokio::test]
    async fn test_read_missing_name() {
        let tool = create_test_tool();
        let ctx = create_test_context();
        let input = json!({"action": "read"});

        let result = tool.execute(input, ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("name"));
    }

    #[tokio::test]
    async fn test_reload_nonexistent() {
        let tool = create_test_tool();
        let ctx = create_test_context();
        let input = json!({"action": "reload", "name": "nonexistent"});

        let result = tool.execute(input, ctx).await.unwrap();
        assert!(result.output.contains("not found"));
    }

    #[tokio::test]
    async fn test_unknown_action() {
        let tool = create_test_tool();
        let ctx = create_test_context();
        let input = json!({"action": "invalid"});

        let result = tool.execute(input, ctx).await.unwrap();
        assert!(result.output.contains("Unknown action"));
    }

    #[tokio::test]
    async fn test_reload_all() {
        let tool = create_test_tool();
        let ctx = create_test_context();
        let input = json!({"action": "reload_all"});

        let result = tool.execute(input, ctx).await.unwrap();
        // The output format is "Reloaded N skills" where N is any number
        // (depends on what skills exist on the system)
        assert!(
            result.output.contains("Reloaded"),
            "Expected 'Reloaded' in output, got: {}",
            result.output
        );
        assert!(
            result.output.contains("skills"),
            "Expected 'skills' in output, got: {}",
            result.output
        );
    }

    fn context_with_working_dir(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            working_dir: Some(dir.to_path_buf()),
            ..create_test_context()
        }
    }

    fn write_project_skill(root: &std::path::Path, name: &str) {
        let skill_dir = root.join(".agents").join("skills").join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Project skill {name}\n---\n\nBody."),
        )
        .unwrap();
    }

    /// Issue #457: project-local skills must be session-scoped. Two contexts
    /// with different working dirs share one registry but must each see only
    /// their own project skills, immediately and without reload_all.
    #[tokio::test]
    async fn test_project_skills_are_scoped_to_tool_context_working_dir() {
        let tool = create_test_tool();
        let repo_a = tempfile::tempdir().unwrap();
        let repo_b = tempfile::tempdir().unwrap();
        write_project_skill(repo_a.path(), "repo-a-skill");
        write_project_skill(repo_b.path(), "repo-b-skill");

        // Immediately visible in each session without any reload.
        let list_a = tool
            .execute(
                json!({"action": "list"}),
                context_with_working_dir(repo_a.path()),
            )
            .await
            .unwrap();
        assert!(list_a.output.contains("repo-a-skill"));
        assert!(
            !list_a.output.contains("repo-b-skill"),
            "session A must not see session B's project skills"
        );

        let list_b = tool
            .execute(
                json!({"action": "list"}),
                context_with_working_dir(repo_b.path()),
            )
            .await
            .unwrap();
        assert!(list_b.output.contains("repo-b-skill"));
        assert!(!list_b.output.contains("repo-a-skill"));

        // reload_all in session A must not leak A's project skills into the
        // shared registry that session B reads.
        tool.execute(
            json!({"action": "reload_all"}),
            context_with_working_dir(repo_a.path()),
        )
        .await
        .unwrap();
        let shared = tool.registry.read().await;
        assert!(
            shared.get("repo-a-skill").is_none(),
            "shared registry must stay free of project-local skills"
        );
        drop(shared);

        // Skill file edits are visible without any reload/restart.
        let skill_md = repo_a.path().join(".agents/skills/repo-a-skill/SKILL.md");
        std::fs::write(
            &skill_md,
            "---\nname: repo-a-skill\ndescription: Updated description\n---\n\nNew body.",
        )
        .unwrap();
        let read = tool
            .execute(
                json!({"action": "read", "name": "repo-a-skill"}),
                context_with_working_dir(repo_a.path()),
            )
            .await
            .unwrap();
        assert!(
            read.output.contains("Updated description"),
            "skill edits must be visible without daemon restart, got: {}",
            read.output
        );
    }

    // --- Agent-authored (managed) skills: create / update / delete ------------
    //
    // These write into the global ~/.jcode/skills root, so they isolate it via
    // JCODE_HOME under the shared test-env lock (same pattern as goal_tests).

    fn write_unmanaged_global_skill(home: &std::path::Path, name: &str) {
        let dir = home.join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Hand authored\n---\n\nBody."),
        )
        .unwrap();
    }

    fn restore_home(prev_home: Option<std::ffi::OsString>) {
        match prev_home {
            Some(value) => crate::env::set_var("JCODE_HOME", value),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }

    #[tokio::test]
    async fn test_create_writes_managed_skill_and_makes_it_loadable() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp.path());

        let tool = create_test_tool();
        let result = tool
            .execute(
                json!({
                    "action": "create",
                    "name": "my-skill",
                    "description": "Do a specific managed thing",
                    "body": "# Managed\n\nSteps here.",
                }),
                create_test_context(),
            )
            .await
            .unwrap();
        assert!(
            result.output.contains("Created managed skill 'my-skill'"),
            "got: {}",
            result.output
        );

        // File written with the managed marker and the body.
        let content =
            std::fs::read_to_string(temp.path().join("skills/my-skill/SKILL.md")).unwrap();
        assert!(
            content.contains("managed: true"),
            "frontmatter must mark managed: {content}"
        );
        assert!(content.contains("Steps here."));

        // The post-mutation reload makes it immediately loadable.
        let loaded = tool
            .execute(
                json!({"action": "load", "name": "my-skill"}),
                create_test_context(),
            )
            .await
            .unwrap();
        assert!(
            loaded.output.contains("## Skill: my-skill"),
            "created skill should be loadable, got: {}",
            loaded.output
        );

        restore_home(prev_home);
    }

    #[tokio::test]
    async fn test_create_refuses_to_overwrite_unmanaged_skill() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp.path());

        write_unmanaged_global_skill(temp.path(), "legacy");

        let tool = create_test_tool();
        let result = tool
            .execute(
                json!({
                    "action": "create",
                    "name": "legacy",
                    "description": "hijack",
                    "body": "nope",
                }),
                create_test_context(),
            )
            .await;
        assert!(result.is_err(), "create must refuse to clobber unmanaged skill");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("non-managed") && msg.contains("legacy"),
            "unexpected error: {msg}"
        );
        // Original file untouched.
        let content = std::fs::read_to_string(temp.path().join("skills/legacy/SKILL.md")).unwrap();
        assert!(content.contains("Hand authored"));

        restore_home(prev_home);
    }

    #[tokio::test]
    async fn test_create_refuses_when_name_loaded_from_different_source() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp.path());

        // A skill of the same name already loaded from a project-local dir.
        let project = tempfile::tempdir().unwrap();
        write_project_skill(project.path(), "conflict");

        let tool = create_test_tool();
        let result = tool
            .execute(
                json!({
                    "action": "create",
                    "name": "conflict",
                    "description": "managed variant",
                    "body": "body",
                }),
                context_with_working_dir(project.path()),
            )
            .await;
        assert!(
            result.is_err(),
            "create must refuse when the name is loaded from another source"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("already loaded from")
        );

        restore_home(prev_home);
    }

    #[tokio::test]
    async fn test_update_managed_skill_and_refusals() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp.path());

        let tool = create_test_tool();

        // Update before create: refused.
        let missing = tool
            .execute(
                json!({"action": "update", "name": "ghost", "description": "d", "body": "b"}),
                create_test_context(),
            )
            .await;
        assert!(missing.is_err());
        assert!(missing.unwrap_err().to_string().contains("No managed skill"));

        // Create then update.
        tool.execute(
            json!({"action": "create", "name": "editable", "description": "v1", "body": "first"}),
            create_test_context(),
        )
        .await
        .unwrap();
        tool.execute(
            json!({"action": "update", "name": "editable", "description": "v2", "body": "second"}),
            create_test_context(),
        )
        .await
        .unwrap();
        let content =
            std::fs::read_to_string(temp.path().join("skills/editable/SKILL.md")).unwrap();
        assert!(content.contains("v2") && content.contains("second"));
        assert!(!content.contains("first"), "old body must be gone: {content}");

        // Update a hand-authored (non-managed) skill: refused.
        write_unmanaged_global_skill(temp.path(), "handmade");
        let refused = tool
            .execute(
                json!({"action": "update", "name": "handmade", "description": "x", "body": "y"}),
                create_test_context(),
            )
            .await;
        assert!(refused.is_err());
        assert!(refused.unwrap_err().to_string().contains("not managed"));

        restore_home(prev_home);
    }

    #[tokio::test]
    async fn test_delete_managed_skill_and_refusal() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp.path());

        let tool = create_test_tool();
        tool.execute(
            json!({"action": "create", "name": "temp-skill", "description": "d", "body": "b"}),
            create_test_context(),
        )
        .await
        .unwrap();
        assert!(temp.path().join("skills/temp-skill/SKILL.md").exists());

        let deleted = tool
            .execute(
                json!({"action": "delete", "name": "temp-skill"}),
                create_test_context(),
            )
            .await
            .unwrap();
        assert!(deleted.output.contains("Deleted managed skill 'temp-skill'"));
        assert!(!temp.path().join("skills/temp-skill").exists());
        // Reload dropped it from the registry.
        assert!(tool.registry.read().await.get("temp-skill").is_none());

        // Deleting a hand-authored skill: refused.
        write_unmanaged_global_skill(temp.path(), "protected");
        let refused = tool
            .execute(
                json!({"action": "delete", "name": "protected"}),
                create_test_context(),
            )
            .await;
        assert!(refused.is_err());
        assert!(refused.unwrap_err().to_string().contains("not managed"));
        assert!(temp.path().join("skills/protected/SKILL.md").exists());

        restore_home(prev_home);
    }

    #[tokio::test]
    async fn test_create_rejects_unsafe_name() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp.path());

        let tool = create_test_tool();
        let result = tool
            .execute(
                json!({"action": "create", "name": "../escape", "description": "d", "body": "b"}),
                create_test_context(),
            )
            .await;
        assert!(result.is_err(), "path-traversal name must be rejected");
        assert!(result.unwrap_err().to_string().contains("Invalid skill name"));

        restore_home(prev_home);
    }
}
