use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::types::Tier;

#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
    #[error("parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("validation: {0}")]
    Validation(String),
}

pub type Result<T> = std::result::Result<T, TemplateError>;

/// A workflow template parsed from TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Template {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub vars: HashMap<String, VarDef>,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VarDef {
    pub description: String,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub needs: Vec<String>,
    pub tier: Option<Tier>,
}

impl Template {
    /// Parse a template from TOML content.
    pub fn from_toml(content: &str) -> Result<Self> {
        let template: Template = toml::from_str(content)?;
        template.validate()?;
        Ok(template)
    }

    /// Validate the template for structural correctness.
    fn validate(&self) -> Result<()> {
        let mut seen_ids = HashSet::new();
        let all_ids: HashSet<&str> = self.steps.iter().map(|s| s.id.as_str()).collect();

        for step in &self.steps {
            if step.id.is_empty() {
                return Err(TemplateError::Validation("step has empty id".into()));
            }
            if !seen_ids.insert(&step.id) {
                return Err(TemplateError::Validation(format!(
                    "duplicate step id: {}",
                    step.id
                )));
            }
            for dep in &step.needs {
                if !all_ids.contains(dep.as_str()) {
                    return Err(TemplateError::Validation(format!(
                        "step '{}' depends on unknown step '{}'",
                        step.id, dep
                    )));
                }
                if dep == &step.id {
                    return Err(TemplateError::Validation(format!(
                        "step '{}' depends on itself",
                        step.id
                    )));
                }
            }
        }

        // Check for cycles via topological sort
        self.check_cycles()?;

        Ok(())
    }

    fn check_cycles(&self) -> Result<()> {
        // Kahn's algorithm
        let mut in_degree: HashMap<&str, usize> = HashMap::new();
        let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();

        for step in &self.steps {
            in_degree.entry(step.id.as_str()).or_insert(0);
            adjacency.entry(step.id.as_str()).or_default();
            for dep in &step.needs {
                *in_degree.entry(step.id.as_str()).or_insert(0) += 1;
                adjacency.entry(dep.as_str()).or_default().push(step.id.as_str());
            }
        }

        let mut queue: Vec<&str> = in_degree
            .iter()
            .filter(|entry| *entry.1 == 0)
            .map(|entry| *entry.0)
            .collect();

        let mut visited = 0;
        while let Some(node) = queue.pop() {
            visited += 1;
            if let Some(neighbors) = adjacency.get(node) {
                for &neighbor in neighbors {
                    let deg = in_degree.get_mut(neighbor).unwrap();
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push(neighbor);
                    }
                }
            }
        }

        if visited != self.steps.len() {
            return Err(TemplateError::Validation("cycle detected in step dependencies".into()));
        }

        Ok(())
    }

    /// Substitute `{{var}}` placeholders in step descriptions with provided values.
    pub fn render(&self, vars: &HashMap<String, String>) -> Result<Template> {
        // Check required vars are provided
        for (name, def) in &self.vars {
            if def.required && !vars.contains_key(name) {
                return Err(TemplateError::Validation(format!(
                    "required variable '{}' not provided",
                    name
                )));
            }
        }

        let steps = self
            .steps
            .iter()
            .map(|step| {
                let mut desc = step.description.clone();
                for (key, value) in vars {
                    desc = desc.replace(&format!("{{{{{key}}}}}"), value);
                }
                Step {
                    id: step.id.clone(),
                    title: step.title.clone(),
                    description: desc,
                    needs: step.needs.clone(),
                    tier: step.tier,
                }
            })
            .collect();

        Ok(Template {
            name: self.name.clone(),
            description: self.description.clone(),
            vars: self.vars.clone(),
            steps,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHINY_TEMPLATE: &str = r#"
name = "shiny"
description = "Design before code, review before ship"

[vars.feature]
description = "Feature to implement"
required = true

[[steps]]
id = "design"
title = "Design"
description = "Think about architecture for {{feature}}"

[[steps]]
id = "implement"
title = "Implement"
description = "Implement {{feature}} based on the design"
needs = ["design"]

[[steps]]
id = "test"
title = "Test"
description = "Write tests for {{feature}}"
needs = ["design"]

[[steps]]
id = "review"
title = "Review"
description = "Review implementation and tests for {{feature}}"
needs = ["implement", "test"]

[[steps]]
id = "merge"
title = "Merge"
description = "Merge {{feature}} to main"
needs = ["review"]
"#;

    #[test]
    fn parse_shiny_template() {
        let template = Template::from_toml(SHINY_TEMPLATE).unwrap();
        assert_eq!(template.name, "shiny");
        assert_eq!(template.steps.len(), 5);
        assert_eq!(template.steps[0].id, "design");
        assert!(template.steps[0].needs.is_empty());
        assert_eq!(template.steps[3].needs, vec!["implement", "test"]);
    }

    #[test]
    fn render_variables() {
        let template = Template::from_toml(SHINY_TEMPLATE).unwrap();
        let mut vars = HashMap::new();
        vars.insert("feature".into(), "JWT auth".into());
        let rendered = template.render(&vars).unwrap();
        assert_eq!(
            rendered.steps[0].description,
            "Think about architecture for JWT auth"
        );
        assert_eq!(
            rendered.steps[1].description,
            "Implement JWT auth based on the design"
        );
    }

    #[test]
    fn missing_required_var() {
        let template = Template::from_toml(SHINY_TEMPLATE).unwrap();
        let vars = HashMap::new();
        let result = template.render(&vars);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("feature"));
    }

    #[test]
    fn duplicate_step_id() {
        let toml = r#"
name = "bad"
description = "duplicate ids"

[[steps]]
id = "a"
title = "A"
description = "step a"

[[steps]]
id = "a"
title = "A again"
description = "duplicate"
"#;
        let result = Template::from_toml(toml);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("duplicate"));
    }

    #[test]
    fn unknown_dependency() {
        let toml = r#"
name = "bad"
description = "unknown dep"

[[steps]]
id = "a"
title = "A"
description = "step a"
needs = ["nonexistent"]
"#;
        let result = Template::from_toml(toml);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown"));
    }

    #[test]
    fn self_dependency() {
        let toml = r#"
name = "bad"
description = "self dep"

[[steps]]
id = "a"
title = "A"
description = "step a"
needs = ["a"]
"#;
        let result = Template::from_toml(toml);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("itself"));
    }

    #[test]
    fn cycle_detection() {
        let toml = r#"
name = "bad"
description = "cyclic"

[[steps]]
id = "a"
title = "A"
description = "step a"
needs = ["b"]

[[steps]]
id = "b"
title = "B"
description = "step b"
needs = ["a"]
"#;
        let result = Template::from_toml(toml);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cycle"));
    }

    #[test]
    fn template_with_tiers() {
        let toml = r#"
name = "tiered"
description = "with model tiers"

[[steps]]
id = "design"
title = "Design"
description = "Architecture"
tier = "heavy"

[[steps]]
id = "implement"
title = "Implement"
description = "Code it"
needs = ["design"]
tier = "standard"

[[steps]]
id = "test"
title = "Test"
description = "Test it"
needs = ["design"]
tier = "light"
"#;
        let template = Template::from_toml(toml).unwrap();
        assert_eq!(template.steps[0].tier, Some(Tier::Heavy));
        assert_eq!(template.steps[1].tier, Some(Tier::Standard));
        assert_eq!(template.steps[2].tier, Some(Tier::Light));
    }
}
