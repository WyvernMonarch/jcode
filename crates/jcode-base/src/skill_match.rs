//! BM25 auto-suggestion of skills against a query.
//!
//! Ranks the loaded skills by BM25 over each skill's precomputed
//! `search_text` (name + description + body, normalized). This is the same
//! scorer used for memory retrieval (`memory.rs::bm25_rank`), adapted to
//! skills so the dynamic system prompt can surface likely-relevant skills for
//! the user's current message without embeddings.

use crate::skill::{SkillRegistry, normalize_skill_search_text};
use std::collections::{HashMap, HashSet};

/// BM25 term-frequency saturation.
const K1: f32 = 1.2;
/// BM25 length normalization.
const B: f32 = 0.75;
/// Minimum BM25 score for a skill to be suggested. Below this, a match is an
/// incidental single-common-word overlap rather than genuine relevance, so
/// unrelated queries return nothing.
// ponytail: fixed floor; raise if hints get too eager, lower if too sparse.
const MIN_SCORE: f32 = 1.0;

/// Suggest up to `k` skills most relevant to `query`, ranked by BM25 over each
/// skill's search text. Returns `(skill_name, score)` sorted by score desc
/// (ties broken by name for determinism). Empty when the query is blank, no
/// skills are loaded, or nothing clears [`MIN_SCORE`].
pub fn suggest_skills(registry: &SkillRegistry, query: &str, k: usize) -> Vec<(String, f32)> {
    if k == 0 {
        return Vec::new();
    }

    let q_norm = normalize_skill_search_text(query);
    let q_set: HashSet<&str> = q_norm.split_whitespace().collect();
    if q_set.is_empty() {
        return Vec::new();
    }

    // User-invoked-only skills (`disable-model-invocation`) are never hinted:
    // suggesting one to the model contradicts the skill's own contract.
    let skills: Vec<_> = registry
        .list()
        .into_iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();
    let docs: Vec<Vec<&str>> = skills
        .iter()
        .map(|s| s.search_text().split_whitespace().collect())
        .collect();
    if docs.is_empty() {
        return Vec::new();
    }

    // Corpus stats: document frequency per term and average document length.
    let n = docs.len() as f32;
    let avgdl = (docs.iter().map(|d| d.len()).sum::<usize>() as f32 / n).max(1.0);
    let mut df: HashMap<&str, f32> = HashMap::new();
    for doc in &docs {
        let unique: HashSet<&str> = doc.iter().copied().collect();
        for term in unique {
            *df.entry(term).or_insert(0.0) += 1.0;
        }
    }

    let mut scored: Vec<(String, f32)> = Vec::new();
    for (idx, doc) in docs.iter().enumerate() {
        if doc.is_empty() {
            continue;
        }
        let dl = doc.len() as f32;
        let mut tf: HashMap<&str, f32> = HashMap::new();
        for term in doc {
            *tf.entry(term).or_insert(0.0) += 1.0;
        }
        let mut score = 0.0f32;
        for term in &q_set {
            let Some(&f) = tf.get(term) else {
                continue;
            };
            let n_q = *df.get(term).unwrap_or(&0.0);
            if n_q == 0.0 {
                continue;
            }
            let idf = (((n - n_q + 0.5) / (n_q + 0.5)) + 1.0).ln();
            let denom = f + K1 * (1.0 - B + B * dl / avgdl);
            score += idf * (f * (K1 + 1.0)) / denom;
        }
        if score >= MIN_SCORE {
            scored.push((skills[idx].name.clone(), score));
        }
    }

    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.truncate(k);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn write_skill(root: &Path, name: &str, description: &str, body: &str) {
        let dir = root.join(".jcode").join("skills").join(name);
        std::fs::create_dir_all(&dir).expect("create skill dir");
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
        )
        .expect("write skill");
    }

    fn write_user_only_skill(root: &Path, name: &str, description: &str, body: &str) {
        let dir = root.join(".jcode").join("skills").join(name);
        std::fs::create_dir_all(&dir).expect("create skill dir");
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: {description}\ndisable-model-invocation: true\n---\n\n{body}\n"
            ),
        )
        .expect("write skill");
    }

    /// Build an isolated registry from temp SKILL.md files (project overlay only,
    /// no global/plugin skills) so ranking is deterministic in tests.
    fn registry_from_temp(temp: &Path) -> SkillRegistry {
        SkillRegistry::load_project_overlay(Some(temp)).expect("load overlay")
    }

    #[test]
    fn relevant_query_ranks_the_matching_skill_first() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_skill(
            temp.path(),
            "db-migrations",
            "Run and author database schema migrations. Apply or roll back a migration file.",
            "Use this to migrate the database schema safely.",
        );
        write_skill(
            temp.path(),
            "css-styling",
            "Style web pages with CSS: layout, flexbox, grid, colors, responsive design.",
            "Use this to make pages look good.",
        );
        let registry = registry_from_temp(temp.path());

        let hits = suggest_skills(&registry, "how do I roll back a database schema migration", 3);
        assert!(!hits.is_empty(), "relevant query should suggest a skill");
        assert_eq!(
            hits[0].0, "db-migrations",
            "the matching skill must rank first, got {hits:?}"
        );
    }

    #[test]
    fn user_invoked_only_skill_is_never_suggested() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_user_only_skill(
            temp.path(),
            "db-migrations",
            "Run and author database schema migrations. Apply or roll back a migration file.",
            "Use this to migrate the database schema safely.",
        );
        let registry = registry_from_temp(temp.path());

        let hits = suggest_skills(&registry, "how do I roll back a database schema migration", 3);
        assert!(
            hits.is_empty(),
            "a disable-model-invocation skill must never be hinted, got {hits:?}"
        );
    }

    #[test]
    fn junk_query_returns_empty() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_skill(
            temp.path(),
            "db-migrations",
            "Run and author database schema migrations. Apply or roll back a migration file.",
            "Use this to migrate the database schema safely.",
        );
        write_skill(
            temp.path(),
            "css-styling",
            "Style web pages with CSS: layout, flexbox, grid, colors, responsive design.",
            "Use this to make pages look good.",
        );
        let registry = registry_from_temp(temp.path());

        let hits = suggest_skills(&registry, "xylophone zeppelin quokka", 3);
        assert!(
            hits.is_empty(),
            "an unrelated query must return no suggestions, got {hits:?}"
        );
    }

    #[test]
    fn blank_query_and_empty_registry_return_empty() {
        let empty = SkillRegistry::default();
        assert!(suggest_skills(&empty, "database migration", 3).is_empty());

        let temp = tempfile::tempdir().expect("tempdir");
        write_skill(temp.path(), "db-migrations", "database schema migration", "body");
        let registry = registry_from_temp(temp.path());
        assert!(suggest_skills(&registry, "   ", 3).is_empty());
        assert!(suggest_skills(&registry, "database migration", 0).is_empty());
    }
}
